"""Eve 实时升级监督器：验证候选、等待回复送达、切换，失败只回退二进制。"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

VERSION = 1
MAX_JSON = 1024 * 1024

def digest(path):
    if not path.is_file() or path.is_symlink() or path.stat().st_size > 256 * 1024 * 1024:
        raise ValueError('invalid_binary_or_resource')
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError('duplicate_field')
        value[key] = item
    return value

def read_json(path, limit=MAX_JSON):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > limit:
        raise ValueError('invalid_json_file')
    with path.open(encoding='utf8') as stream:
        return json.load(stream, object_pairs_hook=unique_object)

def atomic(path, value):
    with tempfile.NamedTemporaryFile(mode='w', encoding='utf8', dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    os.replace(temporary, path)

def sha(value):
    return isinstance(value, str) and len(value) == 64 and all(c in '0123456789abcdef' for c in value)

def argument(command, name):
    index = command.index(name)
    return command[index + 1]

def replace_argument(command, name, value):
    result = list(command)
    result[result.index(name) + 1] = str(value)
    return result

def validate_candidate(request, candidates):
    required = {'format_version', 'candidate_sha256', 'candidate_directory', 'session_id', 'requested_by', 'turn_id'}
    if not isinstance(request, dict) or set(request) != required or type(request['format_version']) is not int or request['format_version'] != VERSION or not sha(request['candidate_sha256']):
        raise ValueError('invalid_request')
    if type(request['turn_id']) is not int or request['turn_id'] <= 0:
        raise ValueError('invalid_turn')
    directory = Path(request['candidate_directory'])
    if directory.is_symlink() or directory.resolve() != candidates.resolve() / request['candidate_sha256']:
        raise ValueError('candidate_outside_host_directory')
    manifest = read_json(directory / 'manifest.json')
    if (not isinstance(manifest, dict) or type(manifest.get('format_version')) is not int
            or manifest.get('format_version') != VERSION or manifest.get('prepared') is not True
            or manifest.get('success') is not True or manifest.get('candidate_sha256') != request['candidate_sha256']
            or len(manifest.get('checks', [])) != 4
            or any(check.get('success') is not True for check in manifest['checks'])):
        raise ValueError('candidate_not_verified')
    binary = directory / ('eve-qqbot.exe' if os.name == 'nt' else 'eve-qqbot')
    if Path(manifest['candidate_path']) != binary or digest(binary) != manifest['binary_sha256']:
        raise ValueError('candidate_binary_changed')
    resources = manifest['resources']
    if not isinstance(resources, dict) or len(resources) > 10000:
        raise ValueError('invalid_resources')
    required_resources = {'AGENT.md', 'connectors/qqbot/bridge.mjs', 'connectors/qqbot/bridge-core.mjs',
                          'connectors/qqbot/package.json', 'connectors/qqbot/package-lock.json'}
    if not required_resources <= set(resources):
        raise ValueError('missing_runtime_resources')
    total = 0
    for name, expected in resources.items():
        relative = PurePosixPath(name)
        if (relative.is_absolute() or '..' in relative.parts or str(relative) != name
                or not (name == 'AGENT.md' or name.startswith(('connectors/qqbot/', 'vendor/')))):
            raise ValueError('invalid_resource_path')
        path = directory / 'resources'
        for part in relative.parts:
            path /= part
            if path.is_symlink():
                raise ValueError('resource_symlink')
        if digest(path) != expected:
            raise ValueError('resource_changed')
        total += path.stat().st_size
        if total > 64 * 1024 * 1024:
            raise ValueError('resources_too_large')
    identity = {'source': manifest['source'], 'binary_sha256': manifest['binary_sha256'], 'resources': resources}
    actual = hashlib.sha256(json.dumps(identity, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    if actual != request['candidate_sha256']:
        raise ValueError('candidate_identity_changed')
    return binary, directory / 'resources', manifest

def delivery_gate(state_path, request):
    """只在确切申请轮次 Completed、同条 QQ 回执 Sent 且全部会话空闲后交接。"""
    state = read_json(state_path, 64 * 1024 * 1024)
    if state['version'] != 1:
        raise ValueError('unknown_state_version')
    entries = state['entries']
    def document(owner, key):
        return json.loads(bytes(entries[owner][key]), object_pairs_hook=unique_object)
    sessions = document('eve.session', 'sessions.v1')
    receipts = document('eve.channel.qqbot', 'receipts.v1')
    if sessions['format_version'] != 1 or receipts['version'] not in (1, 2):
        raise ValueError('unknown_ledger_version')
    session = sessions['sessions'].get(request['session_id'])
    if not session or session['key']['user_id'] != request['requested_by']:
        raise ValueError('request_scope_mismatch')
    turn = next((t for t in session['turns'] if t['id'] == request['turn_id']), None)
    if not turn:
        raise ValueError('request_turn_missing')
    status = turn['status']['state']
    if status == 'Pending':
        return False
    if status != 'Completed':
        raise ValueError('request_turn_not_completed')
    reply = turn['status']['messages'][-1].get('text')
    matching = None
    for receipt in receipts['entries']:
        msg = receipt['message']
        identity = json.dumps([receipt['app_id'], msg['scope'], msg['target_id'], msg['user_id']],
                              ensure_ascii=False, separators=(',', ':')).encode()
        scope = 'qq:' + hashlib.sha256(identity).hexdigest()
        if scope == request['session_id'] and msg['text'] == turn['input']:
            matching = receipt
        if receipt['state'] in ('Processing', 'ReplyPending'):
            return False
    return bool(matching and matching['state'] == 'Sent' and matching.get('reply') == reply) and all(t['status']['state'] != 'Pending'
                        for s in sessions['sessions'].values() for t in s['turns'])

class Child:
    def __init__(self, command):
        self.ready = threading.Event()
        options = {'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == 'nt' else {}
        self.process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, **options)
        def copy():
            for chunk in iter(lambda: self.process.stderr.readline(65536), b''):
                if b'EVE_QQBOT_READY' in chunk:
                    self.ready.set()
                sys.stderr.buffer.write(chunk)
                sys.stderr.buffer.flush()
        self.thread = threading.Thread(target=copy, daemon=True)
        self.thread.start()

    def stop(self, timeout):
        if self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
            self.process.wait(timeout=timeout)
        self.thread.join(timeout=1)
        return self.process.returncode

class Supervisor:
    def __init__(self, options):
        self.options = options
        self.root = options.directory.resolve()
        self.root.mkdir(parents=True, exist_ok=True)
        self.lock = (self.root / 'upgrade.lock').open('a+b')
        self.lock.seek(0)
        if os.name == 'nt':
            import msvcrt
            if self.lock.read(1) == b'': self.lock.write(b'0'); self.lock.flush()
            self.lock.seek(0); msvcrt.locking(self.lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.command = options.command
        if self.command[:1] == ['--']: self.command = self.command[1:]
        if '--database-config' in self.command:
            raise ValueError('file_state_supervisor_requires_file_backend')
        self.state = Path(argument(self.command, '--state-dir')) / 'state.json'
        if not self.state.is_absolute(): raise ValueError('state_directory_must_be_absolute')
        self.stopping = False
        self.child = None
        self.status_lock = threading.RLock()
        self.status = dict(format_version=VERSION, pid=os.getpid(), heartbeat_ms=0, phase='starting',
                           active_sha256=digest(Path(self.command[0])), candidate_sha256=None, result=None, error=None)
        for name in ['request.json', 'processing.json']:
            if (self.root / name).exists():
                self.archive_request(self.root / name, 'interrupted_no_replay')
                self.status['result'] = 'interrupted_no_replay'
        pointer = self.root / 'active-command.json'
        if pointer.exists():
            stored = read_json(pointer)
            if set(stored) != {'command', 'binary_sha256', 'candidate_request'} or digest(Path(stored['command'][0])) != stored['binary_sha256']:
                raise ValueError('active_command_changed')
            binary, resources, _ = validate_candidate(stored['candidate_request'], self.options.candidates)
            if stored['command'][0] != str(binary) or argument(stored['command'], '--agent') != str(resources / 'AGENT.md'):
                raise ValueError('active_command_resources_changed')
            if argument(stored['command'], '--state-dir') != argument(self.command, '--state-dir'):
                raise ValueError('state_directory_changed')
            self.command = stored['command']
            self.status['active_sha256'] = stored['binary_sha256']
        self.finished = threading.Event()
        def heartbeat():
            while not self.finished.is_set():
                self.publish()
                self.finished.wait(1)
        self.heart = threading.Thread(target=heartbeat, daemon=True)

    def publish(self, **changes):
        with self.status_lock:
            self.status.update(changes, heartbeat_ms=int(time.time() * 1000))
            atomic(self.root / 'supervisor.json', self.status)

    def archive_request(self, path, result):
        history = self.root / 'history'
        history.mkdir(exist_ok=True)
        path.rename(history / f'{time.time_ns()}-{result}.json')

    def wait_ready(self, child):
        deadline = time.monotonic() + self.options.ready_timeout
        while not child.ready.wait(0.1):
            if self.stopping or child.process.poll() is not None or time.monotonic() >= deadline:
                raise ValueError('candidate_not_ready')
        stable = time.monotonic() + self.options.health_seconds
        while time.monotonic() < stable:
            if self.stopping or child.process.poll() is not None:
                raise ValueError('candidate_not_stable')
            time.sleep(0.05)

    def candidate_command(self, request):
        binary, resources, manifest = validate_candidate(request, self.options.candidates)
        subprocess.run([str(binary), '--help'], check=True, timeout=30, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        bridge = resources / 'connectors/qqbot'
        modules = bridge / 'node_modules'
        if not modules.exists():
            old_bridge = Path(argument(self.command, '--bridge-script')).parent
            if (old_bridge / 'node_modules').exists() and digest(bridge / 'package-lock.json') == digest(old_bridge / 'package-lock.json'):
                shutil.copytree(old_bridge / 'node_modules', modules, symlinks=True)
            else:
                subprocess.run(['npm', 'ci', '--ignore-scripts', '--no-audit', '--no-fund'], cwd=bridge,
                               check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        result = [str(binary), *self.command[1:]]
        for flag, value in [('--agent', resources / 'AGENT.md'), ('--bridge-script', bridge / 'bridge.mjs')]:
            result = replace_argument(result, flag, value)
        for flag, relative in [('--practice-mindustry-json-rules', 'vendor/eve-portable-mindustry-json-validator'),
                               ('--research-corpus', 'vendor/eve-portable-knowledge')]:
            if flag in result:
                result = replace_argument(result, flag, resources / relative)
        for index, value in enumerate(result[:-1]):
            if value == '--reference' and result[index+1].startswith('eve-portable='):
                result[index+1] = 'eve-portable=' + str(resources / 'vendor/eve-portable-knowledge')
        validate_candidate(request, self.options.candidates)
        return result, manifest

    def switch(self, request, command, manifest):
        if self.stopping: raise ValueError('shutdown_requested')
        # 再次核对磁盘上的请求、资源与执行文件，不能切换一份预检之后发生变化的候选。
        if read_json(self.root / 'request.json') != request or manifest['candidate_sha256'] != request['candidate_sha256']:
            raise ValueError('request_changed')
        validate_candidate(request, self.options.candidates)
        pending = self.root / 'request.json'
        processing = self.root / 'processing.json'
        pending.rename(processing)
        self.publish(phase='switching', candidate_sha256=request['candidate_sha256'], result=None, error=None)
        old_command = self.command
        if self.child.stop(self.options.stop_timeout) != 0:
            raise ValueError('old_process_did_not_close_cleanly')
        try:
            backup = self.root / 'backups' / str(time.time_ns())
            shutil.copytree(self.state.parent, backup)
            self.child = Child(command)
            self.wait_ready(self.child)
            # 状态指针只在候选真正连通后提交；不恢复旧状态快照。
            atomic(self.root / 'active-command.json', dict(command=command, binary_sha256=manifest['binary_sha256'], candidate_request=request))
            self.command = command
            self.publish(phase='running', active_sha256=manifest['binary_sha256'], result='applied')
        except (ValueError, OSError, subprocess.SubprocessError):
            self.child.stop(self.options.stop_timeout)
            if self.stopping: raise ValueError('shutdown_during_upgrade')
            self.child = Child(old_command)
            self.wait_ready(self.child)
            self.publish(phase='running', result='rolled_back_binary_state_preserved')
        self.archive_request(processing, self.status['result'])

    def run(self):
        def stop(_signum, _frame): self.stopping = True
        signals = [signal.SIGINT, signal.SIGTERM]
        if os.name == 'nt': signals.append(signal.SIGBREAK)
        for sig in signals: signal.signal(sig, stop)
        self.heart.start()
        try:
            self.child = Child(self.command)
            self.wait_ready(self.child)
            self.publish(phase='running')
            waiting_since = None
            quiet_since = None
            prepared = None
            state_stamp = None
            gate = False
            while not self.stopping:
                if self.child.process.poll() is not None:
                    raise ValueError('active_process_exited')
                pending = self.root / 'request.json'
                if pending.exists():
                    try:
                        request = read_json(pending)
                        if not isinstance(request, dict) or not sha(request.get('candidate_sha256')):
                            raise ValueError('invalid_request')
                        if prepared is None:
                            self.publish(phase='preparing', candidate_sha256=request.get('candidate_sha256'), result=None, error=None)
                            prepared = (request, *self.candidate_command(request))
                            waiting_since = time.monotonic()
                            self.publish(phase='waiting')
                        if time.monotonic() - waiting_since > self.options.drain_timeout:
                            raise ValueError('reply_or_idle_timeout')
                        stat = self.state.stat()
                        stamp = (stat.st_ino, stat.st_mtime_ns, stat.st_size)
                        if state_stamp != stamp:
                            gate = delivery_gate(self.state, request)
                            state_stamp = stamp
                        if gate:
                            quiet_since = quiet_since or time.monotonic()
                            if time.monotonic() - quiet_since >= self.options.quiet_seconds:
                                if prepared[0] != request: raise ValueError('request_changed')
                                if not delivery_gate(self.state, request):
                                    quiet_since = None
                                    continue
                                self.switch(*prepared)
                                prepared = waiting_since = quiet_since = None
                                state_stamp = None
                        else:
                            quiet_since = None
                    except (ValueError, OSError, KeyError, TypeError, AttributeError, subprocess.SubprocessError) as error:
                        # 拒绝候选或不能确认停机时，不宣称回退/应用成功。
                        if pending.exists(): self.archive_request(pending, 'rejected')
                        if (self.root / 'processing.json').exists():
                            self.publish(phase='blocked', result='interrupted_state_preserved')
                            raise
                        code = str(error) if type(error) is ValueError and str(error).replace('_','').isalnum() else type(error).__name__
                        self.publish(phase='running', result='rejected', error=code[:100])
                        prepared = waiting_since = quiet_since = None
                        state_stamp = None
                time.sleep(0.1)
            return self.child.stop(self.options.stop_timeout)
        finally:
            self.finished.set()
            if self.child and self.child.process.poll() is None:
                with contextlib.suppress(subprocess.TimeoutExpired): self.child.stop(self.options.stop_timeout)
                if self.child.process.poll() is None:
                    self.child.process.kill()
                    self.child.process.wait()
            self.publish(phase='stopped')
            self.lock.close()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--candidates', type=Path, required=True)
    parser.add_argument('--ready-timeout', type=float, default=60)
    parser.add_argument('--stop-timeout', type=float, default=90)
    parser.add_argument('--drain-timeout', type=float, default=300)
    parser.add_argument('--quiet-seconds', type=float, default=1)
    parser.add_argument('--health-seconds', type=float, default=2)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if any(not 0 < v <= 600 for v in [options.ready_timeout, options.stop_timeout, options.drain_timeout, options.quiet_seconds, options.health_seconds]):
        parser.error('期限必须大于 0 且不超过 600 秒')
    try:
        return Supervisor(options).run()
    except Exception as error:
        # 不输出子进程参数、原文、状态正文或凭据。
        print('EVE_SELF_UPGRADE_STOPPED ' + type(error).__name__, file=sys.stderr, flush=True)
        return 1

if __name__ == '__main__':
    raise SystemExit(main())
