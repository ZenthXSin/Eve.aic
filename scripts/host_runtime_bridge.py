"""固定 Linux 宿主桥：以完整脚本包与业务程序为单位切换，保留当前业务状态。"""
import argparse
import contextlib
import importlib.abc
import importlib.util
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import threading
import time

# 固定部署目录中的协议模块；绝不通过候选目录导入宿主验证实现。
sys.path.insert(0, str(Path(__file__).resolve().parent))
import host_runtime_contract as contract

STARTUP_CHECKPOINT_PROTOCOL = 1
STARTUP_CHECKPOINT_ENV = 'EVE_STARTUP_CHECKPOINT_REQUEST'


def identity(pid):
    try:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
        if fields[0] == 'Z':
            return None
        return [Path('/proc/sys/kernel/random/boot_id').read_text().strip(), fields[19]]
    except (OSError, IndexError, TypeError):
        return None


def members(session):
    result = {}
    for path in Path('/proc').glob('[0-9]*'):
        try:
            fields = (path / 'stat').read_text().rsplit(') ', 1)[1].split()
            if fields[0] != 'Z' and int(fields[3]) == session:
                result[int(path.name)] = fields[19]
        except (OSError, ValueError, IndexError):
            pass
    return result


class Sources(importlib.abc.MetaPathFinder, importlib.abc.Loader):
    """仅从已核对源字节加载受控模块，不读取 pyc，也不搜索候选目录。"""
    def __init__(self, sources):
        self.sources = sources

    def find_spec(self, fullname, path=None, target=None):
        if fullname in self.sources:
            return importlib.util.spec_from_loader(fullname, self)
        return None

    def create_module(self, spec):
        return None

    def exec_module(self, module):
        path, data = self.sources[module.__name__]
        module.__file__ = str(path)
        exec(compile(data, str(path), 'exec'), module.__dict__)


def bootstrap(arguments):
    parser = argparse.ArgumentParser()
    parser.add_argument('--record', type=Path, required=True)
    parser.add_argument('--candidates', type=Path, required=True)
    parser.add_argument('--descriptor', type=Path, required=True)
    parser.add_argument('--start-fd', type=int, required=True)
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    options = parser.parse_args(arguments)
    with os.fdopen(options.start_fd, 'rb', buffering=0) as gate:
        if gate.read(1) != b'1':
            raise ValueError('host_parent_did_not_confirm_spawn')
    record = contract.read_json(options.record)
    descriptor = contract.read_json(options.descriptor)
    if record['descriptor'] != descriptor:
        raise ValueError('host_launch_descriptor_changed')
    state = Path(contract.argument(descriptor['command'], '--state-dir'))
    contract.validate_record(record, options.candidates, state)
    sources = {}
    for name, expected in record['scripts'].items():
        data = contract.read_bytes(Path(name), 8 * 1024**2)
        if contract.hashlib.sha256(data).hexdigest() != expected:
            raise ValueError('host_script_changed_before_execution')
        sources[Path(name).stem] = (Path(name), data)
    loader = Sources(sources)
    sys.meta_path.insert(0, loader)
    fixed_directory = Path(__file__).resolve().parent
    sys.path[:] = [p for p in sys.path if Path(p).resolve() != fixed_directory]
    sys.dont_write_bytecode = True
    path, data = sources['self_upgrade_runtime']
    args = options.arguments
    if args[:1] == ['--']:
        args = args[1:]
    sys.argv = [str(path), *args]
    # 两种模块身份共享同一源模块，source_update 中的 import 不会二次加载宿主文件。
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader('self_upgrade_runtime', loader))
    sys.modules['self_upgrade_runtime'] = module
    module.__name__ = '__main__'
    module.__file__ = str(path)
    exec(compile(data, str(path), 'exec'), module.__dict__)
    return 0


class Bridge:
    def __init__(self, options):
        import fcntl
        self.options = options
        self.root = options.directory.resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        contract.directory(self.root)
        self.lock = (self.root / 'host.lock').open('a+b')
        fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.state = Path(contract.argument(options.command, '--state-dir'))
        self.stopping = threading.Event()
        self.child = None
        self.startup_checkpoint_pending = STARTUP_CHECKPOINT_ENV in os.environ
        self.startup_checkpoint_spawned = False
        self.known = {}
        self.nonce = None
        self.child_identity = None
        self.record = None
        self.previous = None
        self.last_status = None
        self.active_path = self.root / 'host-active.json'
        self.journal_path = self.root / 'host-handoff.json'
        self.recovery_path = self.root / 'host-recovery.json'
        self.status_path = self.root / 'host-status.json'
        self.anchors = self.root / 'host-anchors'
        self.anchors.mkdir(exist_ok=True)
        contract.directory(self.anchors)
        self.anchor_version = self.anchors / 'version.json'
        old = contract.read_json(self.status_path) if self.status_path.exists() else None
        if old is not None:
            if not isinstance(old, dict) or old.get('format_version') != 1:
                raise ValueError('host_invalid_status_preserved')
            pid = old.get('child_pid')
            born = old.get('child_identity')
            # 旧 Popen 所属关系不会因重启重新成立；不收养、不猜测、不杀宿主现有进程。
            if pid and (born is not None and identity(pid) == born or self.surviving_recorded_members(old)):
                raise ValueError('host_previous_session_still_alive')
            if old.get('phase') == 'blocked':
                raise ValueError('host_persisted_block')
        self.recovery = contract.read_json(self.recovery_path) if self.recovery_path.exists() else {'format_version': 1, 'crashes': []}
        if (not isinstance(self.recovery, dict) or set(self.recovery) != {'format_version', 'crashes'}
                or self.recovery['format_version'] != 1 or not isinstance(self.recovery['crashes'], list)
                or len(self.recovery['crashes']) > 1000 or any(type(n) is not int for n in self.recovery['crashes'])):
            raise ValueError('host_invalid_recovery_preserved')
        if self.active_path.exists():
            self.record = self.validate(contract.read_json(self.active_path))
        else:
            pointer = self.root / 'active-command.json'
            active = contract.read_json(pointer) if pointer.exists() else None
            if active is not None and (not isinstance(active, dict) or set(active) != {'command', 'binary_sha256', 'candidate_request'}
                    or not isinstance(active.get('command'), list) or not active['command']
                    or contract.digest(Path(active['command'][0])) != active.get('binary_sha256')):
                raise ValueError('host_initial_active_pointer_changed')
            command = active['command'] if active else options.command
            request = active.get('candidate_request') if active else None
            source = options.source_revision
            if request:
                _, _, manifest = contract.validate_candidate(request, options.candidates)
                source = contract.source_revision(manifest)
            descriptor = dict(format_version=1, command=command, candidate_request=request, source_revision=source)
            contract.validate_descriptor(descriptor, options.candidates, self.state)
            self.record = dict(format_version=1, descriptor=descriptor, binary_sha256=contract.digest(Path(command[0])),
                               scripts=contract.script_binding(descriptor, options.candidates, options.supervisor))
            self.validate(self.record)
            contract.atomic(self.active_path, self.record)
        self.initialize_anchors()
        self.reconcile()
        self.previous = self.read_anchor(self.record)
        self.mirror()

    def anchor_path(self, record):
        return self.anchors / (contract.value_digest(record) + '.json')

    def write_anchor(self, record, previous):
        key = contract.value_digest(record)
        value = dict(format_version=1, active_sha256=key, previous=previous)
        path = self.anchor_path(record)
        if path.exists():
            if contract.read_json(path, 128 * 1024) != value:
                raise ValueError('host_anchor_conflict_preserved')
            return
        entries = [p for p in self.anchors.iterdir() if p.name != 'version.json']
        if len(entries) >= 16:
            raise ValueError('host_anchor_storage_limit')
        if len(contract.json.dumps(value).encode()) > 128 * 1024:
            raise ValueError('host_anchor_too_large')
        contract.atomic(path, value)

    def read_anchor(self, record, seen=None):
        seen = set() if seen is None else seen
        key = contract.value_digest(record)
        if key in seen or len(seen) >= 16:
            raise ValueError('host_anchor_cycle_or_limit')
        seen.add(key)
        value = contract.read_json(self.anchor_path(record), 128 * 1024)
        if (not isinstance(value, dict) or set(value) != {'format_version', 'active_sha256', 'previous'}
                or type(value['format_version']) is not int or value['format_version'] != 1
                or value['active_sha256'] != key):
            raise ValueError('host_invalid_anchor_preserved')
        previous = value['previous']
        if previous is not None:
            self.validate(previous)
            if previous == record:
                raise ValueError('host_anchor_cycle_or_limit')
            self.read_anchor(previous, seen)
        return previous

    def initialize_anchors(self):
        entries = [p for p in self.anchors.iterdir() if p.name != 'version.json']
        if len(entries) > 16:
            raise ValueError('host_anchor_storage_limit_preserved')
        for path in entries:
            if not contract.sha(path.stem) or path.suffix != '.json' or contract.regular(path).st_size > 128 * 1024:
                raise ValueError('host_invalid_anchor_storage_preserved')
        if self.anchor_version.exists():
            version = contract.read_json(self.anchor_version)
            if version != {'format_version': 1} or type(version['format_version']) is not int:
                raise ValueError('host_unknown_anchor_version')
            # 活动记录缺少锚点属于损坏；不能悄悄丢弃之前承诺的回退路径。
            self.read_anchor(self.record)
            return
        # 仅首次安装可把旧版journal中已确认的前版导入；落盘标记以后不再猜测缺失状态。
        previous = None
        if self.journal_path.exists():
            journal = contract.read_json(self.journal_path)
            if (not isinstance(journal, dict) or journal.get('format_version') != 1
                    or set(journal) != {'format_version', 'phase', 'previous', 'next', 'error', 'updated_at_ms'}):
                raise ValueError('host_invalid_handoff_journal')
            if journal['phase'] in ('committed', 'trial', 'rolling_back') and journal['next'] == self.record:
                previous = self.validate(journal['previous'])
        if previous is not None:
            self.write_anchor(previous, None)
        self.write_anchor(self.record, previous)
        contract.atomic(self.anchor_version, {'format_version': 1})

    def surviving_recorded_members(self, old):
        pid = old.get('child_pid')
        boot = (old.get('child_identity') or [None])[0]
        if boot != identity(os.getpid())[0] or type(pid) is not int:
            return False
        known = old.get('members', {})
        return any(known.get(str(p)) == born for p, born in members(pid).items())

    def validate(self, record):
        return contract.validate_record(record, self.options.candidates, self.state, self.options.supervisor)

    def candidate(self, record=None):
        request = (record or self.record)['descriptor']['candidate_request']
        return request['candidate_sha256'] if request else None

    def publish(self, phase, **extra):
        value = dict(format_version=1, phase=phase, pid=os.getpid(), identity=identity(os.getpid()),
                     child_pid=self.child.pid if self.child else None,
                     child_identity=self.child_identity if self.child else None,
                     members={str(p): born for p, born in self.known.items()},
                     candidate_sha256=self.candidate() if self.record else None,
                     heartbeat_ms=int(time.time() * 1000), host_nonce=self.nonce)
        if self.last_status:
            value.update({k: self.last_status[k] for k in ('result', 'error') if k in self.last_status})
        value.update(extra)
        contract.atomic(self.status_path, value)
        self.last_status = value

    def journal(self, phase, previous, next_record, error=None):
        value = dict(format_version=1, phase=phase, previous=previous, next=next_record,
                     error=error, updated_at_ms=int(time.time() * 1000))
        contract.atomic(self.journal_path, value)

    def reconcile(self):
        if not self.journal_path.exists():
            return
        value = contract.read_json(self.journal_path)
        if (not isinstance(value, dict) or set(value) != {'format_version', 'phase', 'previous', 'next', 'error', 'updated_at_ms'}
                or value['format_version'] != 1 or value['phase'] not in
                ('prepared', 'old_closed', 'trial', 'committed', 'rolling_back', 'rolled_back', 'blocked')):
            raise ValueError('host_invalid_handoff_journal')
        self.validate(value['previous'])
        self.read_anchor(value['previous'])
        if value['phase'] == 'blocked':
            raise ValueError('host_handoff_blocked')
        if self.record == value['next'] and value['phase'] == 'rolling_back':
            # 回退意图先于活动指针提交；重启不能把失败候选重新解释成已确认成功。
            self.quarantine(value['next'], 'host_interrupted_rollback')
            contract.atomic(self.active_path, value['previous'])
            self.record = value['previous']
            self.previous = self.read_anchor(self.record)
            self.journal('rolled_back', self.record, value['next'], 'host_interrupted_rollback')
            self.archive_handoff('interrupted_rollback')
        elif self.record == value['next'] and value['phase'] in ('trial', 'committed'):
            self.validate(value['next'])
            self.previous = self.read_anchor(self.record)
            if self.previous != value['previous']:
                raise ValueError('host_journal_anchor_conflict')
            self.journal('committed', self.previous, self.record)
            self.archive_handoff('committed_reconciled')
        elif self.record == value['previous'] and value['phase'] != 'committed':
            self.previous = self.read_anchor(self.record)
            if value['phase'] in ('prepared', 'old_closed', 'trial', 'rolling_back'):
                self.quarantine(value['next'], 'host_interrupted_trial')
                self.journal('rolled_back', self.record, value['next'], 'host_interrupted_trial')
                self.archive_handoff('interrupted_no_replay')
        else:
            raise ValueError('host_handoff_pointer_conflict')

    def mirror(self):
        descriptor = self.record['descriptor']
        if descriptor['candidate_request'] is None:
            path = self.root / 'active-command.json'
            if path.exists():
                path.unlink()
                contract.sync_directory(self.root)
        else:
            contract.atomic(self.root / 'active-command.json', dict(command=descriptor['command'],
                            binary_sha256=contract.digest(Path(descriptor['command'][0])),
                            candidate_request=descriptor['candidate_request']))

    def archive_handoff(self, result):
        history = self.root / 'history'
        history.mkdir(exist_ok=True)
        for name in ('handoff-request.json', 'processing.json'):
            path = self.root / name
            if path.exists():
                contract.regular(path)
                path.rename(history / f'{time.time_ns()}-host-{result}-{name}')
        contract.sync_directory(history)
        contract.sync_directory(self.root)

    def quarantine(self, record, error):
        candidate = self.candidate(record)
        if candidate:
            root = self.root / 'quarantine'
            root.mkdir(exist_ok=True)
            path = root / (candidate + '.json')
            if not path.exists():
                contract.atomic(path, dict(format_version=1, candidate_sha256=candidate,
                                           failed_at_ms=int(time.time() * 1000), error=error))

    def start(self, record):
        if self.startup_checkpoint_pending and self.startup_checkpoint_spawned:
            raise ValueError('host_startup_checkpoint_not_ready')
        self.validate(record)
        self.nonce = secrets.token_hex(32)
        launch = self.root / 'host-launch.json'
        descriptor = self.root / 'host-launch-descriptor.json'
        contract.atomic(launch, record)
        contract.atomic(descriptor, record['descriptor'])
        read_fd, write_fd = os.pipe()
        command = [sys.executable, '-I', '-B', str(Path(__file__).resolve()), '--bootstrap',
                   '--record', str(launch), '--candidates', str(self.options.candidates),
                   '--descriptor', str(descriptor), '--start-fd', str(read_fd), '--', '--directory', str(self.root),
                   '--candidates', str(self.options.candidates), '--host-bridge', str(self.root),
                   '--host-nonce', self.nonce, '--launch-descriptor', str(descriptor)]
        for name in ('ready_timeout', 'stop_timeout', 'drain_timeout', 'quiet_seconds', 'health_seconds',
                     'restart_limit', 'restart_window_seconds', 'restart_delay'):
            command.extend(['--' + name.replace('_', '-'), str(getattr(self.options, name))])
        if self.options.auto_update_config:
            command.extend(['--auto-update-config', str(self.options.auto_update_config)])
        command.extend(['--', *record['descriptor']['command']])
        try:
            self.child = subprocess.Popen(command, stdin=subprocess.DEVNULL, start_new_session=True, close_fds=True, pass_fds=(read_fd,))
            # 一次性发布请求继续交给刚创建的监督器，后续自动重启不再次继承。
            self.startup_checkpoint_spawned = True
            os.environ.pop(STARTUP_CHECKPOINT_ENV, None)
            os.close(read_fd)
            read_fd = None
            self.child_identity = identity(self.child.pid)
            self.known = members(self.child.pid)
            self.publish('starting', trial_candidate_sha256=self.candidate(record))
            os.write(write_fd, b'1')
        finally:
            if read_fd is not None:
                os.close(read_fd)
            os.close(write_fd)
        deadline, stable = time.monotonic() + self.options.ready_timeout, None
        while not self.stopping.is_set():
            self.known.update(members(self.child.pid))
            if self.child.poll() is not None:
                raise ValueError('host_supervisor_startup_failed')
            status = None
            try:
                status = contract.read_json(self.root / 'supervisor.json')
            except (OSError, ValueError):
                pass
            expected = (status is not None and status.get('pid') == self.child.pid
                        and status.get('host_nonce') == self.nonce and status.get('phase') == 'running'
                        and status.get('active_candidate_sha256') == self.candidate(record)
                        and status.get('active_sha256') == contract.digest(Path(record['descriptor']['command'][0])))
            if expected:
                stable = stable or time.monotonic()
                if time.monotonic() - stable >= self.options.health_seconds:
                    self.validate(record)
                    if self.child.poll() is not None:
                        raise ValueError('host_supervisor_not_stable')
                    self.startup_checkpoint_pending = False
                    return
            else:
                stable = None
            if time.monotonic() >= deadline:
                raise ValueError('host_supervisor_not_ready')
            self.stopping.wait(.05)
        raise ValueError('host_shutdown_requested')

    def close(self, graceful=False):
        if not self.child:
            return
        self.known.update(members(self.child.pid))
        deadline = time.monotonic() + self.options.stop_timeout
        if self.child.poll() is None:
            self.child.send_signal(signal.SIGTERM)
            try:
                self.child.wait(timeout=self.options.stop_timeout)
            except subprocess.TimeoutExpired:
                pass
        for sig in (signal.SIGTERM, signal.SIGKILL):
            current = members(self.child.pid)
            if not current:
                break
            if not any(self.known.get(p) == born for p, born in current.items()):
                raise ValueError('host_unconfirmed_surviving_session')
            if sig == signal.SIGTERM and time.monotonic() >= deadline:
                continue
            groups = set()
            boot = identity(os.getpid())[0]
            for pid, born in current.items():
                with contextlib.suppress(ProcessLookupError):
                    if identity(pid) == [boot, born] and os.getsid(pid) == self.child.pid:
                        groups.add(os.getpgid(pid))
            for group in groups:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(group, sig)
            until = deadline if sig == signal.SIGTERM else time.monotonic() + 3
            while members(self.child.pid) and time.monotonic() < until:
                time.sleep(.05)
        if members(self.child.pid):
            raise ValueError('host_previous_session_not_closed')
        self.child.wait(timeout=3)

    def handoff(self):
        path = self.root / 'handoff-request.json'
        value = contract.read_json(path)
        if (not isinstance(value, dict) or set(value) != {'format_version', 'nonce', 'base_candidate_sha256', 'request', 'command'}
                or type(value.get('format_version')) is not int or value['format_version'] != 1
                or value.get('nonce') != self.nonce or value.get('base_candidate_sha256') != self.candidate()):
            raise ValueError('host_invalid_handoff_request')
        request = value['request']
        candidate = request.get('candidate_sha256') if isinstance(request, dict) else None
        if contract.sha(candidate) and (self.root / 'quarantine' / (candidate + '.json')).exists():
            raise ValueError('host_candidate_quarantined')
        _, _, manifest = contract.validate_candidate(request, self.options.candidates, True)
        if manifest['source_diff']['base_revision'] != self.record['descriptor']['source_revision']:
            raise ValueError('host_source_diff_base_changed')
        descriptor = contract.candidate_command(self.record['descriptor']['command'], value['command'], request,
                                                self.root, self.options.candidates)
        next_record = dict(format_version=1, descriptor=descriptor, binary_sha256=contract.digest(Path(descriptor['command'][0])),
                           scripts=contract.script_binding(descriptor, self.options.candidates, self.options.supervisor))
        self.validate(next_record)
        previous = self.record
        self.read_anchor(previous)
        try:
            self.write_anchor(next_record, previous)
        except ValueError as error:
            if str(error) not in ('host_anchor_storage_limit', 'host_anchor_too_large'):
                raise
            # 旧监督器已经完成75交接；容量拒绝仍需恢复已验证旧运行实例。
            self.journal('rolling_back', previous, next_record, str(error))
            self.close()
            self.start(previous)
            self.previous = self.read_anchor(previous)
            self.journal('rolled_back', previous, next_record, str(error))
            self.archive_handoff('rejected')
            self.publish('running', result='rejected', error=str(error))
            return
        self.journal('prepared', previous, next_record)
        self.close()
        self.journal('old_closed', previous, next_record)
        self.journal('trial', previous, next_record)
        try:
            self.start(next_record)
            contract.atomic(self.active_path, next_record)
            self.record = next_record
            self.previous = self.read_anchor(next_record)
            self.mirror()
            self.journal('committed', previous, next_record)
            self.archive_handoff('applied')
            self.publish('running', result='applied')
        except Exception as error:
            self.rollback(previous, next_record, error)

    def rollback(self, previous, failed, error):
        if self.startup_checkpoint_pending:
            raise ValueError('host_startup_checkpoint_not_ready')
        code = safe_error(error)
        self.journal('rolling_back', previous, failed, code)
        self.close()
        self.quarantine(failed, code)
        if self.stopping.is_set():
            raise ValueError('host_shutdown_during_trial')
        self.validate(previous)
        # 指针与运行版本先恢复一致；从不复制旧业务状态快照。
        contract.atomic(self.active_path, previous)
        self.record = previous
        self.previous = self.read_anchor(previous)
        self.mirror()
        try:
            self.start(previous)
        except Exception:
            self.journal('blocked', previous, failed, 'host_rollback_failed')
            raise ValueError('host_rollback_failed') from None
        self.journal('rolled_back', previous, failed, code)
        self.archive_handoff('rolled_back')
        self.publish('running', result='rolled_back_runtime_state_preserved', error=code)

    def recover(self):
        if self.startup_checkpoint_pending:
            raise ValueError('host_startup_checkpoint_not_ready')
        self.close()
        now = int(time.time() * 1000)
        self.recovery['crashes'] = [n for n in self.recovery['crashes'] if now - n <= self.options.restart_window_seconds * 1000]
        self.recovery['crashes'].append(now)
        contract.atomic(self.recovery_path, self.recovery)
        if len(self.recovery['crashes']) > self.options.restart_limit:
            raise ValueError('host_restart_limit_exhausted')
        self.publish('backoff')
        self.stopping.wait(min(60, self.options.restart_delay * 2 ** (len(self.recovery['crashes']) - 1)))
        if not self.stopping.is_set():
            self.start(self.record)
            self.publish('running', result='runtime_restarted_state_preserved')

    def run(self):
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, lambda *_: self.stopping.set())
        try:
            while not self.stopping.is_set():
                try:
                    self.start(self.record)
                    break
                except Exception:
                    if self.stopping.is_set():
                        return 0
                    self.close()
                    # 首次恢复未验明时禁止无检查点的自动重启或回退。
                    # 发布端保留失败证据，用新请求发起显式恢复。
                    if self.startup_checkpoint_pending:
                        raise ValueError('host_startup_checkpoint_not_ready') from None
                    if self.child and self.child.returncode == contract.RUNTIME_FAILED and self.previous is not None:
                        self.rollback(self.previous, self.record, ValueError('host_managed_runtime_failed'))
                        break
                    if self.child and self.child.returncode == contract.RECOVERY_STOP:
                        raise ValueError('host_supervisor_blocked')
                    self.recover()
                    if self.child and self.child.poll() is None:
                        break
            self.publish('running')
            last = time.monotonic()
            while not self.stopping.is_set():
                self.known.update(members(self.child.pid))
                code = self.child.poll()
                if code is None:
                    if time.monotonic() - last >= 1:
                        self.publish('running')
                        last = time.monotonic()
                    self.stopping.wait(.05)
                    continue
                self.close()
                if code == contract.HANDOFF:
                    self.handoff()
                elif code == contract.RUNTIME_FAILED and self.previous is not None:
                    self.rollback(self.previous, self.record, ValueError('host_managed_runtime_failed'))
                elif code in (0, contract.RECOVERY_STOP):
                    self.publish('stopped' if code == 0 else 'blocked', exit_code=code)
                    return code
                else:
                    self.recover()
            return 0
        except Exception as error:
            self.publish('blocked', error=safe_error(error))
            raise
        finally:
            self.close(graceful=True)
            if self.stopping.is_set():
                self.publish('stopped', exit_code=0)
            self.lock.close()


def safe_error(error):
    text = str(error)
    return text[:100] if type(error) is ValueError and text.replace('_', '').isalnum() else type(error).__name__


def main():
    if sys.argv[1:2] == ['--bootstrap']:
        return bootstrap(sys.argv[2:])
    if not sys.platform.startswith('linux'):
        raise ValueError('host_bridge_requires_linux')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--supervisor', type=Path, required=True)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--candidates', type=Path, required=True)
    parser.add_argument('--auto-update-config', type=Path)
    parser.add_argument('--source-revision')
    for name, default in [('ready-timeout', 60), ('stop-timeout', 90), ('drain-timeout', 300),
                          ('quiet-seconds', 1), ('health-seconds', 2), ('restart-window-seconds', 600), ('restart-delay', 1)]:
        parser.add_argument('--' + name, type=float, default=default)
    parser.add_argument('--restart-limit', type=int, default=3)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if options.command[:1] == ['--']:
        options.command = options.command[1:]
    if (options.source_revision is not None and not contract.revision(options.source_revision)
            or not options.command or not options.supervisor.is_absolute()
            or not options.candidates.is_absolute() or not options.directory.is_absolute()
            or not 0 <= options.restart_limit <= 10
            or any(not 0 < getattr(options, name) <= 600 for name in
                   ('ready_timeout', 'stop_timeout', 'drain_timeout', 'quiet_seconds', 'health_seconds', 'restart_delay'))
            or not 0 < options.restart_window_seconds <= 86400):
        parser.error('运行路径须绝对路径；次数与各期限须在有效范围内')
    os.umask(0o077)
    return Bridge(options).run()


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        print('EVE_HOST_RUNTIME_STOPPED ' + safe_error(error), file=sys.stderr, flush=True)
        raise SystemExit(contract.RECOVERY_STOP)
