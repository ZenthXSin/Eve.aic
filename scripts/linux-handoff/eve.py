#!/usr/bin/env python3
"""完整 Linux 交接包的本机启动器；不联网取凭据，不自动开始 QQ。"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent
RUN = ROOT / 'run'


def load(path):
    return json.loads(path.read_text(encoding='utf8'))


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf8')
    temporary.chmod(0o600)
    os.replace(temporary, path)


def mapped(value):
    if isinstance(value, str):
        return value.replace('/workspace/', str(ROOT / 'payload/workspace') + '/')
    if isinstance(value, list):
        return [mapped(v) for v in value]
    if isinstance(value, dict):
        return {k: mapped(v) for k, v in value.items()}
    return value


def identity(pid):
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
        if fields[0] == 'Z':
            return None
        return (Path('/proc/sys/kernel/random/boot_id').read_text().strip(), fields[19])
    except (OSError, IndexError):
        return None


def running():
    record = RUN / 'process.json'
    if not record.exists():
        return None
    value = load(record)
    return value if identity(value['pid']) == tuple(value['identity']) else None


def fix_git_paths():
    base = ROOT / 'payload/workspace'
    for directory, dirs, files in os.walk(base):
        dirs[:] = [d for d in dirs if d not in ('node_modules', 'target', 'objects', 'incremental', 'deps', 'memory-archive')]
        parent = Path(directory)
        for name in [*dirs, *files]:
            link = parent / name
            if link.is_symlink():
                old = os.readlink(link)
                new = mapped(old)
                if old != new:
                    link.unlink()
                    link.symlink_to(new)
        paths = []
        if '.git' in files:
            paths.append(parent / '.git')
        if parent.name == '.git' or parent.name.endswith('.git'):
            paths.append(parent / 'config')
            paths.extend((parent / 'worktrees').glob('*/gitdir'))
        for path in paths:
            if not path.is_file() or path.stat().st_size > 64 * 1024:
                continue
            old = path.read_text(encoding='utf8')
            new = mapped(old)
            if new != old:
                path.write_text(new, encoding='utf8')


def configure_rust(config, env):
    tools = ROOT / 'payload/workspace/eve-restored/workspace/tools'
    bundled = tools / 'cargo/bin/cargo'
    if bundled.is_file():
        config['cargo_program'] = str(bundled)
        env['CARGO_HOME'] = str(tools / 'cargo')
        env['RUSTUP_HOME'] = str(tools / 'rustup')
        cargo_bin = str(bundled.parent)
    else:
        # 运行版不携带构建缓存和 Rust；沿用新电脑安装的工具链及其配置。
        for key in ('CARGO_HOME', 'RUSTUP_HOME'):
            env.pop(key, None)
            if key in os.environ:
                env[key] = os.environ[key]
        config['cargo_program'] = shutil.which('cargo', path=os.environ.get('PATH')) or 'cargo'
        config['cargo_arguments'] = [x for x in config.get('cargo_arguments', []) if not x.startswith('+')]
        cargo_bin = ''
    env['PATH'] = os.pathsep.join(x for x in [str(ROOT / 'runtime/bin'), cargo_bin, os.environ.get('PATH', '/usr/bin:/bin')] if x)


def prepare():
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        raise RuntimeError('本包运行器需要 Linux x86_64；其他架构请从随包源码重新构建')
    if os.getuid() == 0:
        raise RuntimeError('请以普通用户运行，不要使用 sudo 或 root 启动满权限 Eve')
    RUN.mkdir(exist_ok=True, mode=0o700)
    spec = load(ROOT / 'handoff.json')
    expected = spec.get('stable_binary_sha256')
    if expected:
        with (ROOT / spec['binary']).open('rb') as stream:
            if hashlib.file_digest(stream, 'sha256').hexdigest() != expected:
                raise RuntimeError('Eve 程序 SHA-256 与交付版本不符；请核对更新文件，未启动')
    exported = ROOT / 'payload/workspace/eve-migration/private'
    config = mapped(load(exported / 'developer-features.json'))
    if spec.get('profile') == 'hosted':
        config.update(self_upgrade=False, github=False, github_write_repositories=[])
    if config.get('self_upgrade'):
        config['live_upgrade'] = str(RUN / 'upgrade-control')
    else:
        config.pop('live_upgrade', None)
    if spec.get('profile') == 'hosted':
        config['workspace'] = str(ROOT / 'data/workspace')
        config['storage_directory'] = str(ROOT / 'data/developer-state')
        Path(config['workspace']).mkdir(parents=True, exist_ok=True, mode=0o700)
    config['cargo_arguments'] = [x for x in config.get('cargo_arguments', []) if '/opt/codex/' not in x]
    env = dict(os.environ)
    # 导出文件仅含 Eve 配置；不重用云会话的代理、CA、PATH 或平台认证。
    env.update({k: mapped(v) for k, v in load(exported / 'features-environment.json').items()
                if k.startswith(('EVE_', 'QQBOT_')) or k in ('CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_PROFILE_TEST_DEBUG')})
    if spec.get('profile') == 'hosted':
        for key in ('EVE_GITHUB_TOKEN', 'GH_TOKEN', 'GITHUB_TOKEN'):
            env.pop(key, None)
    env['EVE_DEVELOPER_CONFIG'] = str(RUN / 'developer.json')
    env['EVE_RUNTIME_LOADER'] = str(ROOT / 'runtime/lib/ld-linux-x86-64.so.2')
    env['EVE_RUNTIME_LIBRARY_PATH'] = str(ROOT / 'runtime/lib')
    env['PYTHONHOME'] = str(ROOT / 'runtime/python')
    env['JAVA_HOME'] = str(ROOT / 'runtime/java')
    env['GIT_EXEC_PATH'] = str(ROOT / 'runtime/git-core')
    env['GIT_TEMPLATE_DIR'] = str(ROOT / 'runtime/git-templates')
    configure_rust(config, env)
    save(RUN / 'developer.json', config)
    command = mapped(load(exported / 'features-command.json'))
    command[0] = str(ROOT / spec['binary'])
    for flag, relative in spec.get('resources', {}).items():
        command[command.index(flag) + 1] = str(ROOT / relative)
    for index, value in enumerate(command):
        if index and command[index - 1] == '--reference':
            name = value.split('=', 1)[0]
            if name in spec.get('references', {}):
                command[index] = name + '=' + str(ROOT / spec['references'][name])
    for flag, name in [('--node', 'node'), ('--practice-python', 'python3'), ('--practice-java', 'java')]:
        if flag in command:
            command[command.index(flag) + 1] = str(ROOT / 'runtime/bin' / name)
    # 迁移路径改写仅作用于配置/版本库元数据；持久会话与原始证据不改写。
    marker = RUN / 'relocated.json'
    if not marker.exists() or load(marker).get('root') != str(ROOT):
        fix_git_paths()
        for path in Path(config['storage_directory']).glob('candidates/*/manifest.json'):
            value = load(path)
            binary = path.parent / 'eve-qqbot'
            if binary.is_file():
                value['candidate_path'] = str(binary)
                save(path, value)
        save(marker, {'root': str(ROOT)})
    return spec, config, command, env


def address(command):
    value = command[command.index('--web-listen') + 1]
    host, port = value.rsplit(':', 1)
    if host != '127.0.0.1':
        raise RuntimeError('交接包面板必须只监听 127.0.0.1')
    return host, int(port)


def readiness(command, env):
    host, port = address(command)
    conn = http.client.HTTPConnection(host, port, timeout=3)
    try:
        conn.request('GET', '/api/readiness', headers={'Authorization': 'Bearer ' + env['EVE_WEB_TOKEN']})
        response = conn.getresponse()
        if response.status != 200:
            raise RuntimeError('本机面板认证失败')
        return json.loads(response.read())
    finally:
        conn.close()


def doctor(command, env):
    results = []
    for name, args in [('Python', ['python3', '--version']), ('Node', ['node', '--version']),
                       ('Java', ['java', '-version']), ('Git', ['git', '--version'])]:
        try:
            r = subprocess.run(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=25)
            text = r.stdout.decode(errors='replace').splitlines()
            results.append((name, r.returncode == 0, text[0] if text else '无输出'))
        except (OSError, subprocess.TimeoutExpired):
            results.append((name, False, '无法执行'))
    loader = env['EVE_RUNTIME_LOADER']
    r = subprocess.run([loader, '--library-path', env['EVE_RUNTIME_LIBRARY_PATH'], command[0], '--help'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
    results.append(('Eve', r.returncode == 0, '运行器加载检查'))
    for name, ok, text in results:
        print(f'{name}: {"通过" if ok else "失败"} — {text}')
    try:
        cargo = subprocess.run(['cargo', '--version'], env=env, capture_output=True, text=True, timeout=25)
        print('Rust 构建工具（可选）:', cargo.stdout.strip() if cargo.returncode == 0 else '不可用；运行 Eve 不需要，自编译升级需要安装 Rust')
    except (OSError, subprocess.TimeoutExpired):
        print('Rust 构建工具（可选）: 未安装；运行 Eve 不需要，自编译升级需要安装 Rust')
    print('宿主 C 编译器:', shutil.which('cc') or '未安装（运行不需要，自编译升级需要 build-essential 或对应工具链）')
    print('可用磁盘 GiB:', round(shutil.disk_usage(ROOT).free / 2**30, 1))
    print('范围：以上只验证本机运行依赖，未连接 QQ 或请求模型。')
    return 0 if all(ok for _, ok, _ in results) else 1


def main():
    parser = argparse.ArgumentParser(description='Eve Linux 完整迁移包')
    parser.add_argument('action', choices=['prepare', 'doctor', 'start', 'stop', 'status'], nargs='?', default='status')
    args = parser.parse_args()
    os.umask(0o077)
    spec, config, command, env = prepare()
    current = running()
    if args.action == 'prepare':
        print('本机路径已准备；未启动 QQ。')
        return 0
    if args.action == 'doctor':
        return doctor(command, env)
    if args.action == 'stop':
        if not current:
            print('未运行。')
            return 0
        os.kill(current['pid'], signal.SIGTERM)
        for _ in range(120):
            if not running():
                (RUN / 'process.json').unlink(missing_ok=True)
                print('已停止，持久状态保留。')
                return 0
            time.sleep(1)
        raise RuntimeError('停止尚未确认；未强制终止或恢复旧状态，请查看 run/eve.log')
    if args.action == 'status':
        if not current:
            print('未运行。启动命令：./eve start')
            return 0
        value = readiness(command, env)
        print(json.dumps({'pid': current['pid'], 'channel': value['channel'], 'model': value['model']['model'], 'tools': value['tools_registered']}, ensure_ascii=False, indent=2))
        return 0
    if current:
        print('已有本包实例运行；./eve status 可查看状态。')
        return 0
    with socket.socket() as sock:
        sock.settimeout(1)
        if sock.connect_ex(address(command)) == 0:
            raise RuntimeError('本机面板端口已被占用；拒绝重复启动，请先停止其他 Eve 实例')
    argv = [str(ROOT / 'runtime/bin/python3'), str(ROOT / spec['supervisor']),
            '--directory', str(RUN / 'upgrade-control'), '--candidates', str(Path(config['storage_directory']) / 'candidates'), '--', *command]
    with (RUN / 'eve.log').open('ab') as log:
        process = subprocess.Popen(argv, env=env, cwd=Path(config['workspace']), stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    token = identity(process.pid)
    if token is None:
        raise RuntimeError('监督器退出，请查看 run/eve.log')
    save(RUN / 'process.json', {'pid': process.pid, 'identity': token})
    for _ in range(65):
        if process.poll() is not None:
            raise RuntimeError('启动失败；查看 run/eve.log，未回滚或清空持久状态')
        try:
            value = readiness(command, env)
            if value['channel']['connected']:
                print(f'QQ 已连接；模型 {value["model"]["model"]}，工具 {value["tools_registered"]} 个。进程在后台运行，无有效期。')
                return 0
        except (OSError, ValueError, RuntimeError):
            pass
        time.sleep(1)
    raise RuntimeError('仍在等待连接，请用 ./eve status 和 run/eve.log 排查；未重复启动')


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        print('Eve:', str(error), file=sys.stderr)
        raise SystemExit(1)
