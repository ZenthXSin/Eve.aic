"""可信容器入口：编译子进程降权，冻结产物前清理其全部进程；不在宿主直接运行。"""
import hashlib
import json
import os
from pathlib import Path
import resource
import select
import shutil
import signal
import stat
import subprocess
import sys
import time

USER = 65534
ARTIFACT_LIMIT = 16 * 1024 * 1024
DEADLINE = time.monotonic() + 30
SAFE_NAMESPACE = False
STAGE = 'resource_guard'


class SandboxFailure(Exception):
    pass


def emit(**value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def limits():
    global USER
    if os.geteuid() != 0 or os.getpid() != 1:
        raise SandboxFailure('trusted_controller_requires_private_pid_namespace')
    root = Path('/sys/fs/cgroup')
    memory = int((root / 'memory.max').read_text())
    pids = int((root / 'pids.max').read_text())
    swap = int((root / 'memory.swap.max').read_text())
    quota, period = map(int, (root / 'cpu.max').read_text().split())
    if not (0 < memory <= 2 * 1024**3 and swap == 0 and 0 < pids <= 64 and 0 < quota <= period):
        raise SandboxFailure('resource_limits_unverified')
    filesystem = os.statvfs('/')
    if not filesystem.f_flag & os.ST_RDONLY:
        raise SandboxFailure('root_filesystem_writable')
    free_disk = filesystem.f_bavail * filesystem.f_frsize
    if free_disk < 512 * 1024**2:
        raise SandboxFailure('host_disk_reserve_low')
    if 'NoNewPrivs:\t1' not in Path('/proc/self/status').read_text():
        raise SandboxFailure('privilege_guard_missing')
    # 私有 toolchain 的属主可能只有 0700；使用其非 root UID，不放宽宿主权限。
    # 只读资源挂载与私有 PID namespace 仍限定其能看到和影响的范围。
    owner = Path('/toolchain').stat().st_uid
    USER = owner if owner > 0 else 65534
    return {'memory_bytes': memory, 'pids': pids, 'cpu_quota': quota, 'cpu_period': period,
            'compiler_uid': USER, 'swap_bytes': swap, 'disk_available_bytes': free_disk}


def stop_children():
    if not SAFE_NAMESPACE or os.getpid() != 1:
        raise SandboxFailure('cleanup_requires_attested_namespace')
    # PID namespace 只包含本容器；降权用户不能改变回 root 或获得其他身份。
    until = time.monotonic() + 2
    while True:
        found = False
        for name in os.listdir('/proc'):
            if not name.isdecimal():
                continue
            try:
                status = Path('/proc', name, 'status').read_text()
                uid = next(line for line in status.splitlines() if line.startswith('Uid:')).split()[1:]
                if str(USER) in uid:
                    found = True
                    os.kill(int(name), signal.SIGKILL)
            except (FileNotFoundError, ProcessLookupError):
                pass
        while True:
            try:
                child, _ = os.waitpid(-1, os.WNOHANG)
                if child == 0:
                    break
            except ChildProcessError:
                break
        if not found:
            return
        if time.monotonic() >= until:
            raise SandboxFailure('children_not_quiescent')
        time.sleep(0.005)


def drop_privileges():
    resource.setrlimit(resource.RLIMIT_FSIZE, (32 * 1024**2, 32 * 1024**2))
    resource.setrlimit(resource.RLIMIT_NOFILE, (64, 64))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_CPU, (20, 20))
    os.setgroups([])
    os.setgid(USER)
    os.setuid(USER)


def child(arguments, environment, log):
    process = subprocess.Popen(arguments, cwd='/project', env=environment,
        stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        close_fds=True, preexec_fn=drop_privileges)
    try:
        return process.wait(timeout=max(0.001, DEADLINE - time.monotonic()))
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
        raise SandboxFailure('compilation_timeout')
    finally:
        stop_children()


def diagnostic(path):
    with path.open('rb') as source:
        data = source.read(8193)
    text = data[:8192].decode('utf-8', errors='replace')
    encoded = text.encode()
    return encoded[:8192].decode('utf-8', errors='ignore'), len(data) > 8192 or len(encoded) > 8192


def main():
    global SAFE_NAMESPACE, STAGE
    observed = limits()
    SAFE_NAMESPACE = True
    STAGE = 'prepare_project'
    for directory in ['/project', '/project/src', '/target', '/cargo', '/tmp/home']:
        Path(directory).mkdir(parents=True, exist_ok=True)
    for source, target in [('Cargo.toml', '/project/Cargo.toml'),
                           ('main.rs', '/project/src/main.rs'), ('build.rs', '/project/build.rs')]:
        path = Path('/input', source)
        if not path.exists() and source == 'build.rs':
            continue
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 256 * 1024:
            raise SandboxFailure('invalid_source')
        shutil.copyfile(path, target)
        os.chown(target, USER, USER)
    Path('/cargo/config.toml').write_text('[source.crates-io]\nreplace-with="approved"\n'
        '[source.approved]\ndirectory="/vendor"\n[net]\noffline=true\n')
    for directory in ['/project', '/project/src', '/target', '/cargo', '/tmp/home']:
        os.chown(directory, USER, USER)
    environment = {'PATH': '/toolchain/bin:/usr/bin', 'HOME': '/tmp/home', 'CARGO_HOME': '/cargo',
        'RUSTC': '/toolchain/bin/rustc', 'CARGO_TARGET_DIR': '/target', 'CARGO_BUILD_JOBS': '1',
        'RUSTFLAGS': '-Cdebuginfo=0 -Ccodegen-units=1 -Cpanic=abort -Clinker=/usr/bin/x86_64-linux-gnu-gcc',
        'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8', 'SOURCE_DATE_EPOCH': '0'}
    log_path = Path('/tmp/compiler.log')
    with log_path.open('wb') as log:
        STAGE = 'lockfile'
        result = child(['/toolchain/bin/cargo', 'generate-lockfile', '--offline'], environment, log)
        if result == 0:
            STAGE = 'compile'
            result = child(['/toolchain/bin/cargo', 'build', '--offline', '--locked', '--jobs=1'], environment, log)
    text, truncated = diagnostic(log_path)
    if result != 0:
        emit(ok=False, code='compilation_failed', stage=STAGE, exit_code=result, diagnostics=text, truncated=truncated)
        return 1
    # 子进程已经杀净；只提升固定名字的普通产物，不跟随链接。
    STAGE = 'freeze_artifact'
    path = Path('/target/debug/eve_plugin')
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or not (0 < metadata.st_size <= ARTIFACT_LIMIT):
            raise SandboxFailure('invalid_artifact')
        Path('/out').chmod(0o700)
        digest = hashlib.sha256()
        with Path('/out/plugin').open('xb') as output:
            size = 0
            while data := source.read(65536):
                size += len(data)
                if size > ARTIFACT_LIMIT:
                    raise SandboxFailure('artifact_limit')
                digest.update(data)
                output.write(data)
        if size != metadata.st_size:
            raise SandboxFailure('artifact_changed')
    Path('/out/plugin').chmod(0o600)
    emit(ok=True, code='compiled', artifact_bytes=size, artifact_sha256=digest.hexdigest(),
        limits=observed, diagnostics=text, truncated=truncated)
    # 宿主仅复制这个已冻结文件；输入确认或硬期限到达即退出，容器随后删除。
    select.select([sys.stdin.buffer], [], [], max(0, min(10, DEADLINE - time.monotonic())))
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        emit(ok=False, code=str(error) if isinstance(error, SandboxFailure) else 'controller_failed',
            reason=type(error).__name__, stage=STAGE)
        sys.exit(1)
    finally:
        if SAFE_NAMESPACE:
            stop_children()
