"""Eve 宿主启动入口：复用已验证候选与两层守护，支持登录恢复和显式停止。"""
import argparse
import datetime
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import time


class Service:
    def __init__(self, record, private):
        self.record = record.resolve()
        self.private = private.resolve()
        self.directory = self.private / 'service-runtime'
        self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.settings = self.directory / 'autostart.json'
        self.process = None
        self.reload()
        root = Path(self.data['guard_script']).parent
        sys.path.insert(0, str(root))
        spec = importlib.util.spec_from_file_location('eve_service_guard', root / 'service_guard.py')
        self.guard = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.guard)
        self.runtime = sys.modules['self_upgrade_runtime']

    def reload(self):
        self.data = json.loads(self.record.read_text())

    def enabled(self):
        if not self.settings.exists():
            return False
        value = self.runtime.read_json(self.settings)
        if set(value) != {'version', 'enabled'} or value['version'] != 1 or type(value['enabled']) is not bool:
            raise ValueError('invalid_autostart_configuration_preserved')
        return value['enabled']

    def set_enabled(self, value):
        self.runtime.atomic(self.settings, {'version': 1, 'enabled': value})

    def guard_alive(self):
        pid = self.data['guard_pid']
        expected = self.data.get('guard_identity')
        if not expected or self.guard.identity(pid) != tuple(expected):
            return False
        args = [s.decode() for s in Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0') if s]
        if self.data['guard_script'] not in args:
            raise ValueError('guard_process_identity_conflict')
        return True

    def active(self):
        return self.runtime.read_json(Path(self.data['control_directory']) / 'active-command.json')

    def address(self):
        value = self.runtime.argument(self.active()['command'], '--web-listen')
        host, port = value.rsplit(':', 1)
        if host != '127.0.0.1' or not 0 < int(port) < 65536:
            raise ValueError('readiness_requires_loopback_address')
        return host, int(port)

    def probe(self):
        env = self.runtime.read_json(self.private / 'features-environment.json')
        connection = http.client.HTTPConnection(*self.address(), timeout=3)
        try:
            connection.request('GET', '/api/readiness', headers={'Authorization': 'Bearer ' + env['EVE_WEB_TOKEN']})
            response = connection.getresponse()
            if response.status != 200:
                raise ValueError('readiness_http_failed')
            return json.loads(response.read())
        finally:
            connection.close()

    def status(self):
        result = {'autostart_enabled': self.enabled(), 'guard_running': self.guard_alive(), 'qq_connected': False}
        try:
            ready = self.probe()
            result.update(qq_connected=ready['channel']['connected'], tools_registered=ready['tools_registered'])
        except (OSError, ValueError, KeyError):
            pass
        return result

    def start(self):
        self.reload()
        if self.guard_alive():
            return self.status()
        guard_directory = Path(self.data['guard_directory'])
        guard_file = guard_directory / 'guard.json'
        old = self.runtime.read_json(guard_file) if guard_file.exists() else {}
        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
        old_boot = (old.get('identity') or [None])[0]
        same_boot = old_boot == boot
        if old and old_boot is None:
            raise ValueError('invalid_guard_record_preserved')
        if old.get('phase') == 'blocked':
            raise ValueError('recovery_block_preserved_requires_operator')
        if same_boot:
            child = old.get('child_pid')
            if child and (self.guard.identity(child) == tuple(old.get('child_identity') or ()) or self.guard.members(child)):
                raise ValueError('supervisor_still_alive_no_duplicate_start')
        with socket.socket() as port:
            if port.connect_ex(self.address()) == 0:
                raise ValueError('service_port_in_use_no_duplicate_start')
        active = self.active()
        config = self.runtime.read_json(self.private / 'developer-features.json')
        candidates = Path(config['storage_directory']) / 'candidates'
        binary, _, manifest = self.runtime.validate_candidate(active['candidate_request'], candidates)
        if self.runtime.digest(binary) != active['binary_sha256']:
            raise ValueError('active_binary_changed')
        state = Path(self.data['state_directory'])
        if self.runtime.argument(active['command'], '--state-dir') != str(state) or not (state / 'state.json').is_file():
            raise ValueError('existing_state_required')
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
        backup = self.directory / ('before-start-' + stamp)
        backup.mkdir(mode=0o700)
        shutil.copytree(state, backup / 'state')
        shutil.copytree(Path(self.data['control_directory']), backup / 'control')
        shutil.copytree(guard_directory, backup / 'guard')
        shutil.copy2(self.record, backup / self.record.name)
        if old and not same_boot:
            # 仅归档已确认属于旧 boot 的进程元数据；不清理业务状态或碰当前同号 PID。
            history = guard_directory / 'history'
            history.mkdir(exist_ok=True)
            guard_file.rename(history / (stamp + '-previous-boot.json'))
        env = dict(os.environ)
        saved = self.runtime.read_json(self.private / 'features-environment.json')
        allow = {'CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL',
                 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_PROFILE_TEST_DEBUG'}
        env.update({k: v for k, v in saved.items() if k.startswith(('EVE_', 'QQBOT_')) or k in allow})
        prefixes = []
        if saved.get('CARGO_HOME') and (Path(saved['CARGO_HOME']) / 'bin').is_dir():
            prefixes.append(str(Path(saved['CARGO_HOME']) / 'bin'))
        if '--node' in active['command']:
            prefixes.append(str(Path(self.runtime.argument(active['command'], '--node')).parent))
        env['PATH'] = os.pathsep.join(prefixes + [env.get('PATH', '/usr/bin:/bin')])
        command = [sys.executable, self.data['guard_script'], '--directory', str(guard_directory),
                   '--stop-timeout', '110', '--', sys.executable, self.data['supervisor_script'],
                   '--directory', self.data['control_directory'], '--candidates', str(candidates),
                   '--stop-timeout', '90', '--ready-timeout', '60', '--drain-timeout', '600', '--', *active['command']]
        log_path = self.directory / ('eve-' + stamp + '.log')
        with log_path.open('ab') as log:
            process = subprocess.Popen(command, cwd=config['workspace'], env=env, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True, close_fds=True)
        self.process = process
        self.data.update(guard_pid=process.pid, guard_identity=self.guard.identity(process.pid),
                         channel='starting', log=str(log_path), state_backup=str(backup),
                         source_sha=manifest['source']['head'], binary_sha256=active['binary_sha256'])
        self.runtime.atomic(self.record, self.data)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise ValueError('guard_exited_startup_log_preserved')
            try:
                ready = self.probe()
                status = self.runtime.read_json(Path(self.data['control_directory']) / 'supervisor.json')
                if ready['channel']['connected'] and status['phase'] == 'running' and status['active_sha256'] == active['binary_sha256']:
                    self.data.update(channel='connected', supervisor_pid=status['pid'], tools_registered=ready['tools_registered'])
                    self.runtime.atomic(self.record, self.data)
                    return self.status()
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(.5)
        raise ValueError('startup_not_ready_no_duplicate_retry')

    def stop(self):
        self.set_enabled(False)
        self.reload()
        if self.guard_alive():
            pid = self.data['guard_pid']
            expected = tuple(self.data['guard_identity'])
            os.kill(pid, signal.SIGTERM)
            deadline = time.monotonic() + 115
            while self.guard.identity(pid) == expected:
                if time.monotonic() > deadline:
                    raise ValueError('graceful_stop_timeout_no_unverified_kill')
                time.sleep(.1)
        if self.process is not None:
            self.process.wait(timeout=3)
            self.process = None
        self.data['channel'] = 'stopped'
        self.runtime.atomic(self.record, self.data)
        return self.status()


def main():
    if not sys.platform.startswith('linux'):
        raise ValueError('linux_service_required')
    import fcntl
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--service-record', type=Path, required=True)
    parser.add_argument('--private-directory', type=Path, required=True)
    parser.add_argument('action', choices=['start', 'ensure', 'stop', 'status'])
    parser.add_argument('--background', action='store_true')
    args = parser.parse_args()
    os.umask(0o077)
    service = Service(args.service_record, args.private_directory)
    if args.action == 'status':
        print(json.dumps(service.status(), ensure_ascii=False))
        return 0
    if args.background:
        if args.action not in ['start', 'ensure']:
            raise ValueError('background_only_for_start_or_ensure')
        if args.action == 'start':
            service.set_enabled(True)
        if not service.enabled() or service.guard_alive():
            return 0
        command = [sys.executable, str(Path(__file__).resolve()), '--service-record', str(args.service_record),
                   '--private-directory', str(args.private_directory), 'ensure']
        with (service.directory / 'startup.log').open('ab') as log:
            subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                             start_new_session=True, close_fds=True)
        return 0
    with (service.directory / 'start.lock').open('a+b') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0
        if args.action == 'stop':
            result = service.stop()
        else:
            if args.action == 'start':
                service.set_enabled(True)
            if not service.enabled():
                return 0
            result = service.start()
        print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        code = str(error) if isinstance(error, ValueError) and str(error).replace('_', '').isalnum() else type(error).__name__
        print('EVE_SERVICE_ERROR ' + code, file=sys.stderr)
        raise SystemExit(1)
