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
RECOVERY_STOP = 78

def digest(path):
    if not path.is_file() or path.is_symlink() or path.stat().st_size > 2 * 1024 * 1024 * 1024:
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
    # Windows 上另一进程用 os.replace 原子替换同一文件的瞬间，打开会短暂报 PermissionError；
    # 只对这一种错误稍等重试，其他错误照常抛出。
    for attempt in range(20):
        try:
            with path.open(encoding='utf8') as stream:
                return json.load(stream, object_pairs_hook=unique_object)
        except PermissionError:
            if os.name != 'nt' or attempt == 19:
                raise
            time.sleep(0.05)

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
    # Windows 上目标文件正被其他进程读取时替换会短暂报 PermissionError；同 read_json 只对此重试。
    for attempt in range(20):
        try:
            os.replace(temporary, path)
            return
        except PermissionError:
            if os.name != 'nt' or attempt == 19:
                temporary.unlink(missing_ok=True)
                raise
            time.sleep(0.05)

def sha(value):
    return isinstance(value, str) and len(value) == 64 and all(c in '0123456789abcdef' for c in value)

def argument(command, name):
    index = command.index(name)
    return command[index + 1]

# 已从宿主移除的启动参数（连同参数值）；旧部署的启动命令带着它们时，候选版本启动前去掉。
REMOVED_FLAGS = ['--practice-mindustry-json-rules', '--practice-python']

def without_removed_flags(command):
    result = list(command)
    for flag in REMOVED_FLAGS:
        while flag in result:
            index = result.index(flag)
            del result[index:index + 2]
    return result

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

def receipt_fingerprint(receipt):
    """完整记录摘要；历史记录只要变化，就重新视作本次运行的在途记录。"""
    return hashlib.sha256(json.dumps(receipt, ensure_ascii=False, sort_keys=True,
                                     separators=(',', ':')).encode()).hexdigest()

def inherited_receipts(state_path):
    """启动子进程前捕获历史未决回执；仅供门禁比较，不改写、不重放。"""
    if not state_path.exists():
        return frozenset()
    state = read_json(state_path, 64 * 1024 * 1024)
    if state.get('version') != 1:
        raise ValueError('unknown_state_version')
    raw = state['entries'].get('eve.channel.qqbot', {}).get('receipts.v1')
    if raw is None:
        return frozenset()
    receipts = json.loads(bytes(raw), object_pairs_hook=unique_object)
    if receipts['version'] not in (1, 2, 3, 4, 5):
        raise ValueError('unknown_ledger_version')
    validate_merged_receipts(receipts)
    return frozenset(receipt_fingerprint(r) for r in receipts['entries']
                     if r['state'] in ('Processing', 'ReplyPending')
                     or any(p.get('state') == 'Sending' for p in r.get('progress', []))
                     or sticker_in_flight(r))

def sticker_in_flight(receipt):
    """格式 5 正文发完后附带的表情包：待发或发送中都算在途投递。"""
    sticker = receipt.get('sticker')
    return isinstance(sticker, dict) and sticker.get('state') in ('Pending', 'Sending') \
        and receipt['state'] == 'Sent'

def delivery_gate(state_path, request, inherited=frozenset()):
    """确切申请轮次已完成送达且本次运行空闲；不将历史未知回执当成当前任务。"""
    state = read_json(state_path, 64 * 1024 * 1024)
    if state['version'] != 1:
        raise ValueError('unknown_state_version')
    entries = state['entries']
    def document(owner, key):
        return json.loads(bytes(entries[owner][key]), object_pairs_hook=unique_object)
    sessions = document('eve.session', 'sessions.v1')
    receipts = document('eve.channel.qqbot', 'receipts.v1')
    if sessions['format_version'] != 1 or receipts['version'] not in (1, 2, 3, 4, 5):
        raise ValueError('unknown_ledger_version')
    validate_merged_receipts(receipts)
    for receipt in receipts['entries']:
        progress = receipt.get('progress', [])
        if not isinstance(progress, list) or len(progress) > 3:
            raise ValueError('invalid_progress_ledger')
        if progress and receipts['version'] not in (3, 4, 5):
            raise ValueError('invalid_progress_ledger')
        sticker = receipt.get('sticker')
        if sticker is not None and (receipts['version'] != 5 or not isinstance(sticker, dict)
                                    or set(sticker) != {'path', 'state'}
                                    or not isinstance(sticker['path'], str) or not sticker['path']
                                    or sticker['state'] not in ('Pending', 'Sending', 'Sent', 'Failed', 'Skipped')):
            raise ValueError('invalid_sticker_ledger')
        if sticker_in_flight(receipt) and receipt_fingerprint(receipt) not in inherited:
            return False
        for part in progress:
            if (not isinstance(part, dict) or set(part) != {'text', 'state'}
                    or not isinstance(part['text'], str) or not part['text'].strip()
                    or len(part['text'].encode()) > 1024
                    or part['state'] not in ('Sending', 'Sent', 'Failed')):
                raise ValueError('invalid_progress_ledger')
            if part['state'] == 'Sending' and receipt_fingerprint(receipt) not in inherited:
                return False
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
        # 当前进程启动前已存在且完全未变化的未知记录：既不阻塞空闲，也不能证明本次请求已送达。
        if receipt_fingerprint(receipt) in inherited:
            continue
        msg = receipt['message']
        identity = json.dumps([receipt['app_id'], msg['scope'], msg['target_id'], msg['user_id']],
                              ensure_ascii=False, separators=(',', ':')).encode()
        scope = 'qq:' + hashlib.sha256(identity).hexdigest()
        merged_text = '\n'.join([r['message']['text'] for r in receipts['entries']
            if r.get('merged_into') == msg.get('id') and r['app_id'] == receipt['app_id']]
            + [msg['text']]) if receipts['version'] in (4, 5) else msg['text']
        if receipt['state'] != 'Merged' and scope == request['session_id'] and merged_text == turn['input']:
            matching = receipt
        if receipt['state'] in ('Processing', 'ReplyPending'):
            return False
    return bool(matching and matching['state'] == 'Sent' and matching.get('reply') == reply) and all(t['status']['state'] != 'Pending'
                        for s in sessions['sessions'].values() for t in s['turns'])

def validate_merged_receipts(receipts):
    """格式 4 起的合并关系只关联同身份、顺序在后的非合并目标；不得隐藏未决投递。"""
    if receipts['version'] not in (4, 5):
        return
    lookup = {}
    for index, entry in enumerate(receipts['entries']):
        message = entry['message']
        identity = (entry['app_id'], message.get('id'))
        if not isinstance(identity[1], str) or not identity[1] or identity in lookup:
            raise ValueError('invalid_merged_receipt')
        lookup[identity] = (index, entry)
    for index, entry in enumerate(receipts['entries']):
        into = entry.get('merged_into')
        if entry['state'] != 'Merged':
            if into is not None:
                raise ValueError('invalid_merged_receipt')
            continue
        if not isinstance(into, str):
            raise ValueError('invalid_merged_receipt')
        target = lookup.get((entry['app_id'], into))
        if (not target or target[0] <= index or target[1]['state'] == 'Merged'
                or any(entry['message'][key] != target[1]['message'][key]
                       for key in ('scope', 'target_id', 'user_id'))
                or entry.get('reply') is not None or entry.get('segments') is not None
                or entry.get('progress')):
            raise ValueError('invalid_merged_receipt')

def execution_command(command):
    """Linux 便携包可选的私有动态加载器；逻辑命令和候选摘要保持原样。"""
    loader = os.environ.get('EVE_RUNTIME_LOADER')
    libraries = os.environ.get('EVE_RUNTIME_LIBRARY_PATH')
    if os.name == 'nt' or (loader is None and libraries is None):
        return command
    if (not loader or not libraries or not Path(loader).is_absolute()
            or not Path(loader).is_file()
            or any(not p or not Path(p).is_absolute() or not Path(p).is_dir()
                   for p in libraries.split(os.pathsep))):
        raise ValueError('portable_runtime_loader_invalid')
    with Path(command[0]).open('rb') as stream:
        elf = stream.read(4) == b'\x7fELF'
    return [loader, '--library-path', libraries, *command] if elf else command

def command_resources(command):
    paths = set()
    for flag in ['--agent', '--bridge-script', '--practice-mindustry-server', '--research-corpus']:
        if flag not in command:
            continue
        path = Path(argument(command, flag))
        if path.is_dir():
            paths.update(p for p in path.rglob('*') if p.is_file())
        else:
            paths.add(path)
        if flag == '--bridge-script':
            paths.update(p for p in [path.parent / name for name in
                         ['bridge-core.mjs', 'package.json', 'package-lock.json']] if p.exists())
    return paths

def snapshot_command(command, candidate_request=None):
    return dict(format_version=1, command=command, binary_sha256=digest(Path(command[0])),
                resources={str(p): digest(p) for p in sorted(command_resources(command))},
                candidate_request=candidate_request)

class Child:
    def __init__(self, command):
        self.ready = threading.Event()
        options = {'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == 'nt' else {}
        if sys.platform.startswith('linux'):
            # 独立进程组但继承监督器会话；内层可关闭桥接，外层仍可收拢整场会话。
            options['process_group'] = 0
        self.process = subprocess.Popen(execution_command(command), stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, **options)
        def copy():
            for chunk in iter(lambda: self.process.stderr.readline(65536), b''):
                if b'EVE_QQBOT_READY' in chunk:
                    self.ready.set()
                sys.stderr.buffer.write(chunk)
                sys.stderr.buffer.flush()
        self.thread = threading.Thread(target=copy, daemon=True)
        self.thread.start()

    def close_group(self, timeout):
        if not sys.platform.startswith('linux'):
            return
        def remaining():
            result = []
            for path in Path('/proc').glob('[0-9]*'):
                try:
                    fields = (path / 'stat').read_text().rsplit(') ', 1)[1].split()
                    if fields[0] != 'Z' and int(fields[2]) == self.process.pid and int(fields[3]) == os.getsid(0):
                        result.append(int(path.name))
                except (OSError, ValueError, IndexError):
                    pass
            return result
        for sig, grace in [(signal.SIGTERM, timeout), (signal.SIGKILL, 5)]:
            if not remaining():
                return
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, sig)
            deadline = time.monotonic() + grace
            while remaining() and time.monotonic() < deadline:
                time.sleep(0.05)
        if remaining():
            raise ValueError('runtime_process_group_not_closed')

    def stop(self, timeout):
        if self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
            self.process.wait(timeout=timeout)
        self.close_group(timeout)
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
        self.inherited_receipts = inherited_receipts(self.state)
        if self.inherited_receipts:
            print(f'EVE_UPGRADE_RECOVERY inherited_unknown_receipts={len(self.inherited_receipts)} preserved_no_replay', file=sys.stderr, flush=True)
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
        active_request = None
        if pointer.exists():
            stored = read_json(pointer)
            if set(stored) != {'command', 'binary_sha256', 'candidate_request'} or digest(Path(stored['command'][0])) != stored['binary_sha256']:
                raise ValueError('active_command_changed')
            binary, resources, _ = validate_candidate(stored['candidate_request'], self.options.candidates)
            if (self.root / 'quarantine' / (stored['candidate_request']['candidate_sha256'] + '.json')).exists():
                raise ValueError('active_candidate_quarantined')
            if stored['command'][0] != str(binary) or argument(stored['command'], '--agent') != str(resources / 'AGENT.md'):
                raise ValueError('active_command_resources_changed')
            if argument(stored['command'], '--state-dir') != argument(self.command, '--state-dir'):
                raise ValueError('state_directory_changed')
            self.command = stored['command']
            active_request = stored['candidate_request']
            self.status['active_sha256'] = stored['binary_sha256']
        self.active_descriptor = snapshot_command(self.command, active_request)
        recovery = self.root / 'recovery.json'
        self.recovery = read_json(recovery) if recovery.exists() else dict(format_version=1, crashes=[], result=None)
        if (set(self.recovery) != {'format_version', 'crashes', 'result'}
                or self.recovery['format_version'] != 1 or not isinstance(self.recovery['crashes'], list)
                or any(not isinstance(c, dict) or set(c) != {'at_ms', 'exit_code', 'binary_sha256'}
                       or type(c['at_ms']) is not int or type(c['exit_code']) is not int
                       or not sha(c['binary_sha256']) for c in self.recovery['crashes'])):
            raise ValueError('invalid_recovery_history')
        self.rollback_used = False
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
        # 调度暂停可能跨过整个稳定窗口；提交指针前仍须核对进程尚存。
        if self.stopping or child.process.poll() is not None:
            raise ValueError('candidate_not_stable')

    def candidate_command(self, request):
        if (self.root / 'quarantine' / (request['candidate_sha256'] + '.json')).exists():
            raise ValueError('candidate_quarantined')
        binary, resources, manifest = validate_candidate(request, self.options.candidates)
        subprocess.run(execution_command([str(binary), '--help']), check=True, timeout=30, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        bridge = resources / 'connectors/qqbot'
        modules = bridge / 'node_modules'
        original_bridge = Path(argument(self.command, '--bridge-script'))
        bundled_bridge = original_bridge.name == 'bridge.mjs' and original_bridge.parent.name == 'qqbot'
        if bundled_bridge and not modules.exists():
            old_bridge = original_bridge.parent
            if (old_bridge / 'node_modules').exists() and digest(bridge / 'package-lock.json') == digest(old_bridge / 'package-lock.json'):
                shutil.copytree(old_bridge / 'node_modules', modules, symlinks=True)
            else:
                subprocess.run(['npm', 'ci', '--ignore-scripts', '--no-audit', '--no-fund'], cwd=bridge,
                               check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        result = [str(binary), *without_removed_flags(self.command[1:])]
        replacements = [('--agent', resources / 'AGENT.md')]
        if bundled_bridge: replacements.append(('--bridge-script', bridge / 'bridge.mjs'))
        for flag, value in replacements:
            result = replace_argument(result, flag, value)
        if '--research-corpus' in result:
            result = replace_argument(result, '--research-corpus', resources / 'vendor/eve-portable-knowledge')
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
        self.validate_descriptor(self.active_descriptor)
        atomic(self.root / 'previous-command.json', self.active_descriptor)
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
            self.active_descriptor = snapshot_command(command, request)
            self.rollback_used = False
            self.publish(phase='running', active_sha256=manifest['binary_sha256'], result='applied')
        except (ValueError, OSError, subprocess.SubprocessError):
            self.child.stop(self.options.stop_timeout)
            if self.stopping: raise ValueError('shutdown_during_upgrade')
            self.validate_descriptor(self.active_descriptor)
            self.child = Child(old_command)
            self.wait_ready(self.child)
            self.publish(phase='running', result='rolled_back_binary_state_preserved')
        self.archive_request(processing, self.status['result'])

    def validate_descriptor(self, descriptor):
        if (not isinstance(descriptor, dict)
                or set(descriptor) != {'format_version', 'command', 'binary_sha256', 'resources', 'candidate_request'}
                or descriptor['format_version'] != 1
                or not isinstance(descriptor['command'], list) or not descriptor['command']
                or len(descriptor['command']) > 512
                or any(not isinstance(s, str) or '\0' in s for s in descriptor['command'])):
            raise ValueError('invalid_previous_command')
        command = descriptor['command']
        if (not Path(command[0]).is_absolute()
                or Path(argument(command, '--state-dir')) != self.state.parent
                or snapshot_command(command, descriptor['candidate_request']) != descriptor):
            raise ValueError('previous_command_changed')
        if descriptor['candidate_request'] is not None:
            binary, resources, _ = validate_candidate(descriptor['candidate_request'], self.options.candidates)
            if str(binary) != command[0] or argument(command, '--agent') != str(resources / 'AGENT.md'):
                raise ValueError('previous_candidate_changed')

    def recover(self):
        """有限恢复；仅重启程序，绝不还原状态、重放旧请求或重试未知投递。"""
        if self.stopping:
            return
        if self.child.process.poll() is None:
            raise ValueError('old_process_still_running')
        self.child.close_group(self.options.stop_timeout)
        self.child.thread.join(timeout=1)
        now = int(time.time() * 1000)
        crashes = [c for c in self.recovery['crashes']
                   if now - c['at_ms'] <= self.options.restart_window_seconds * 1000]
        crashes.append(dict(at_ms=now, exit_code=self.child.process.returncode,
                            binary_sha256=self.status['active_sha256']))
        self.recovery.update(crashes=crashes, result='recovering')
        atomic(self.root / 'recovery.json', self.recovery)
        if len(crashes) > self.options.restart_limit:
            self.recovery['result'] = 'restart_limit_exhausted'
            atomic(self.root / 'recovery.json', self.recovery)
            raise ValueError('restart_limit_exhausted')
        if not self.state.is_file():
            raise ValueError('runtime_state_missing')
        # 必须在旧进程真正退出后重新捕获，保留中断轮次与未知回执的真实字节。
        self.inherited_receipts = inherited_receipts(self.state)
        pending = self.root / 'request.json'
        if pending.exists():
            self.archive_request(pending, 'runtime_interrupted_no_replay')
        previous = self.root / 'previous-command.json'
        candidate = self.active_descriptor['candidate_request']
        result = 'runtime_restarted_state_preserved'
        if candidate is not None and previous.exists() and not self.rollback_used:
            descriptor = read_json(previous)
            self.validate_descriptor(descriptor)
            if descriptor != self.active_descriptor:
                quarantine = self.root / 'quarantine'
                quarantine.mkdir(exist_ok=True)
                atomic(quarantine / (candidate['candidate_sha256'] + '.json'),
                       dict(format_version=1, candidate_sha256=candidate['candidate_sha256'],
                            failed_at_ms=now, exit_code=self.child.process.returncode))
                pointer = self.root / 'active-command.json'
                if pointer.exists():
                    self.archive_request(pointer, 'runtime_failed_active_command')
                if descriptor['candidate_request'] is not None:
                    atomic(pointer, {k: descriptor[k] for k in ['command', 'binary_sha256', 'candidate_request']})
                self.active_descriptor = descriptor
                self.command = descriptor['command']
                self.rollback_used = True
                result = 'runtime_rolled_back_binary_state_preserved'
        self.validate_descriptor(self.active_descriptor)
        self.publish(phase='recovering', result=result, error='active_process_exited',
                     active_sha256=self.active_descriptor['binary_sha256'])
        until = time.monotonic() + min(60, self.options.restart_delay * 2 ** (len(crashes) - 1))
        while not self.stopping and time.monotonic() < until:
            time.sleep(0.05)
        if self.stopping:
            return
        self.child = Child(self.command)
        self.wait_ready(self.child)
        self.recovery['result'] = result
        atomic(self.root / 'recovery.json', self.recovery)
        self.publish(phase='running', result=result, error=None)

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
                    self.recover()
                    prepared = waiting_since = quiet_since = state_stamp = None
                    gate = False
                    continue
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
                            gate = delivery_gate(self.state, request, self.inherited_receipts)
                            state_stamp = stamp
                        if gate:
                            quiet_since = quiet_since or time.monotonic()
                            if time.monotonic() - quiet_since >= self.options.quiet_seconds:
                                if prepared[0] != request: raise ValueError('request_changed')
                                if not delivery_gate(self.state, request, self.inherited_receipts):
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
        except Exception as error:
            code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
            with contextlib.suppress(Exception):
                self.recovery['result'] = code[:100]
                atomic(self.root / 'recovery.json', self.recovery)
                self.publish(result='stopped_state_preserved', error=code[:100])
            raise
        finally:
            self.finished.set()
            if self.child and self.child.process.poll() is None:
                with contextlib.suppress(subprocess.TimeoutExpired): self.child.stop(self.options.stop_timeout)
                if self.child.process.poll() is None:
                    self.child.process.kill()
                    self.child.process.wait()
            if self.child:
                self.child.close_group(self.options.stop_timeout)
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
    parser.add_argument('--restart-limit', type=int, default=3)
    parser.add_argument('--restart-window-seconds', type=float, default=600)
    parser.add_argument('--restart-delay', type=float, default=1)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if any(not 0 < v <= 600 for v in [options.ready_timeout, options.stop_timeout, options.drain_timeout, options.quiet_seconds, options.health_seconds]):
        parser.error('期限必须大于 0 且不超过 600 秒')
    if not 0 <= options.restart_limit <= 10 or not 0 < options.restart_window_seconds <= 86400 or not 0 < options.restart_delay <= 60:
        parser.error('恢复次数为 0 至 10，窗口为 0 至 86400 秒，退避为 0 至 60 秒；期限必须大于零')
    try:
        return Supervisor(options).run()
    except Exception as error:
        # 不输出子进程参数、原文、状态正文或凭据。
        print('EVE_SELF_UPGRADE_STOPPED ' + type(error).__name__, file=sys.stderr, flush=True)
        return RECOVERY_STOP

if __name__ == '__main__':
    raise SystemExit(main())
