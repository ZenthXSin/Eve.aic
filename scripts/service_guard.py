"""Linux Eve 外层守护：只监管自己创建的监督器进程组，限次恢复，正常停止不拉起。"""
import argparse
import contextlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time

from self_upgrade_runtime import RECOVERY_STOP, atomic, read_json


def identity(pid):
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
        if fields[0] == 'Z':
            return None
        return (Path('/proc/sys/kernel/random/boot_id').read_text().strip(), fields[19])
    except (OSError, IndexError):
        return None


def members(group):
    result = {}
    for path in Path('/proc').glob('[0-9]*'):
        try:
            fields = (path / 'stat').read_text().rsplit(') ', 1)[1].split()
            if fields[0] != 'Z' and int(fields[3]) == group:
                result[int(path.name)] = fields[19]
        except (OSError, ValueError, IndexError):
            pass
    return result


class Guard:
    def __init__(self, options):
        if not sys.platform.startswith('linux'):
            raise ValueError('service_guard_requires_linux')
        import fcntl
        self.options = options
        self.root = options.directory.resolve()
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.lock = (self.root / 'guard.lock').open('a+b')
        fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        path = self.root / 'guard.json'
        old = read_json(path) if path.exists() else {}
        # 守护自身曾被终止时，不凭旧 PID 收养或杀死可能仍在运行的监督器。
        if old.get('child_identity') and identity(old.get('child_pid')) == tuple(old['child_identity']):
            raise ValueError('previous_supervisor_still_running')
        if old.get('phase') in ('running', 'backoff', 'blocked') and old.get('child_pid') and members(old['child_pid']):
            raise ValueError('previous_supervisor_group_not_closed')
        history = self.root / 'guard-recovery.json'
        self.crashes = read_json(history)['crashes'] if history.exists() else []
        if not isinstance(self.crashes, list) or any(type(c) is not int for c in self.crashes):
            raise ValueError('invalid_guard_recovery_history')
        self.stopping = threading.Event()
        self.child = None
        self.known = {}

    def publish(self, phase, **extra):
        status = dict(format_version=1, phase=phase, pid=os.getpid(), identity=identity(os.getpid()),
                      heartbeat_ms=int(time.time() * 1000), child_pid=self.child.pid if self.child else None,
                      child_identity=identity(self.child.pid) if self.child else None,
                      members={str(k): v for k, v in self.known.items()})
        status.update(extra)
        atomic(self.root / 'guard.json', status)

    def cleanup_group(self):
        current = members(self.child.pid)
        if not current:
            return
        if not any(self.known.get(pid) == born for pid, born in current.items()):
            raise ValueError('unconfirmed_surviving_group_no_restart')
        # 唯一目标是当前 Popen 创建的新会话；不按程序名、旧 PID 文件或宿主其他进程清理。
        for sig, timeout in [(signal.SIGTERM, self.options.stop_timeout), (signal.SIGKILL, 5)]:
            groups = set()
            for pid, born in members(self.child.pid).items():
                with contextlib.suppress(ProcessLookupError):
                    if identity(pid) == (identity(os.getpid())[0], born) and os.getsid(pid) == self.child.pid:
                        groups.add(os.getpgid(pid))
            for group in groups:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(group, sig)
            end = time.monotonic() + timeout
            while members(self.child.pid) and time.monotonic() < end:
                time.sleep(0.05)
            if not members(self.child.pid):
                return
        raise ValueError('previous_process_group_not_closed')

    def run(self):
        def stop(_signum, _frame):
            self.stopping.set()
        for sig in [signal.SIGINT, signal.SIGTERM]:
            signal.signal(sig, stop)
        try:
            while not self.stopping.is_set():
                self.child = subprocess.Popen(self.options.command, stdin=subprocess.DEVNULL,
                                              start_new_session=True)
                self.known = members(self.child.pid)
                self.publish('running')
                last_publish = time.monotonic()
                while self.child.poll() is None and not self.stopping.wait(0.1):
                    self.known.update(members(self.child.pid))
                    if time.monotonic() - last_publish >= 1:
                        self.publish('running')
                        last_publish = time.monotonic()
                if self.stopping.is_set() and self.child.poll() is None:
                    self.child.send_signal(signal.SIGTERM)
                    try:
                        self.child.wait(timeout=self.options.stop_timeout)
                    except subprocess.TimeoutExpired:
                        self.cleanup_group()
                        self.child.wait(timeout=5)
                code = self.child.wait()
                self.cleanup_group()
                if self.stopping.is_set() or code in (0, RECOVERY_STOP):
                    self.publish('stopped' if code == 0 or self.stopping.is_set() else 'blocked', exit_code=code)
                    return 0 if self.stopping.is_set() else code
                now = int(time.time() * 1000)
                self.crashes = [t for t in self.crashes if now - t <= self.options.restart_window_seconds * 1000]
                self.crashes.append(now)
                atomic(self.root / 'guard-recovery.json', dict(format_version=1, crashes=self.crashes))
                if len(self.crashes) > self.options.restart_limit:
                    raise ValueError('guard_restart_limit_exhausted')
                self.publish('backoff', exit_code=code, restart_count=len(self.crashes))
                if self.stopping.wait(min(60, self.options.restart_delay * 2 ** (len(self.crashes) - 1))):
                    self.publish('stopped', exit_code=0)
                    return 0
            return 0
        except Exception as error:
            code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
            with contextlib.suppress(Exception):
                self.publish('blocked', error=code[:100])
            raise
        finally:
            if self.child and self.child.poll() is None:
                self.child.send_signal(signal.SIGTERM)
                with contextlib.suppress(subprocess.TimeoutExpired):
                    self.child.wait(timeout=self.options.stop_timeout)
                if self.child.poll() is None:
                    self.cleanup_group()
                    self.child.wait(timeout=5)
            self.lock.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--restart-limit', type=int, default=3)
    parser.add_argument('--restart-window-seconds', type=float, default=600)
    parser.add_argument('--restart-delay', type=float, default=1)
    parser.add_argument('--stop-timeout', type=float, default=100)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if options.command[:1] == ['--']:
        options.command = options.command[1:]
    if (not options.command or not 0 <= options.restart_limit <= 10
            or not 0 < options.restart_window_seconds <= 86400
            or not 0 < options.restart_delay <= 60 or not 0 < options.stop_timeout <= 600):
        parser.error('命令不能为空；恢复次数为 0 至 10；窗口、退避和停止期限必须在允许范围内且大于零')
    os.umask(0o077)
    try:
        return Guard(options).run()
    except Exception as error:
        print('EVE_SERVICE_GUARD_STOPPED ' + type(error).__name__, file=sys.stderr, flush=True)
        return RECOVERY_STOP


if __name__ == '__main__':
    raise SystemExit(main())
