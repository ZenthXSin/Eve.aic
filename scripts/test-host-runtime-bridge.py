"""固定宿主桥接器的真实 Linux 进程验收；所有文件和进程均由测试创建。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import sysconfig
import tempfile
import time
import unittest


BRIDGE = Path(__file__).with_name('host_runtime_bridge.py').resolve()
RUNTIME_SCRIPTS = [
    'scripts/build_resources.py',
    'scripts/self_upgrade_runtime.py',
    'scripts/source_update.py',
]
BASE_RESOURCES = {
    'AGENT.md': b'test agent',
    'connectors/qqbot/bridge.mjs': b'export {};',
    'connectors/qqbot/bridge-core.mjs': b'export {};',
    'connectors/qqbot/package.json': b'{}',
    'connectors/qqbot/package-lock.json': b'{}',
}


def value_digest(value):
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':'))
    return hashlib.sha256(raw.encode()).hexdigest()


def file_digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def atomic(path, value):
    temporary = path.with_name(path.name + '.test-tmp')
    with temporary.open('w', encoding='utf8') as stream:
        json.dump(value, stream, ensure_ascii=False, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def read_json(path):
    return json.loads(path.read_text(encoding='utf8'))


def alive(pid):
    try:
        # 僵尸进程已经停止执行，不能用 kill(pid, 0) 误判为重复运行。
        fields = Path('/proc') / str(pid) / 'stat'
        return fields.read_text().split(') ', 1)[1].split()[0] != 'Z'
    except (FileNotFoundError, ProcessLookupError):
        return False


SUPERVISOR_SOURCE = r'''
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import build_resources
import source_update

TAG = __TAG__
MODE = __MODE__


def atomic(path, value):
    temporary = path.with_name(path.name + '.fixture-tmp')
    with temporary.open('w', encoding='utf8') as stream:
        json.dump(value, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument('--host-bridge', required=True)
    parser.add_argument('--host-nonce', required=True)
    parser.add_argument('--launch-descriptor', required=True)
    args, _ = parser.parse_known_args()
    control = Path(args.host_bridge)
    descriptor = json.loads(Path(args.launch_descriptor).read_text())
    command = descriptor['command']
    state = Path(command[command.index('--state-dir') + 1])
    events = Path(command[command.index('--events') + 1])
    request = descriptor['candidate_request']
    identity = request['candidate_sha256'] if request else None
    child = None

    def event(phase):
        record = dict(tag=TAG, phase=phase, pid=os.getpid(),
                      pgid=os.getpgrp(), sid=os.getsid(0), nonce=args.host_nonce,
                      child_pid=child.pid if child else None,
                      child_pgid=os.getpgid(child.pid) if child else None,
                      candidate=identity,
                      dependencies=[build_resources.MARKER, source_update.MARKER])
        with events.open('a', encoding='utf8') as stream:
            stream.write(json.dumps(record, sort_keys=True) + '\n')
            stream.flush()
            os.fsync(stream.fileno())

    def close():
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=.5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=1)

    def stop(*_):
        close()
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    event('boot')
    sentinel = state / 'sentinel.json'
    data = json.loads(sentinel.read_text()) if sentinel.exists() else {'visits': []}
    data['visits'].append(TAG)
    atomic(sentinel, data)
    if MODE == 'startup_failure':
        raise SystemExit(9)
    child = subprocess.Popen(command, process_group=0)
    event('child_started')
    try:
        if MODE != 'never_ready':
            ready = dict(pid=os.getpid(), host_nonce=args.host_nonce, phase='running',
                         active_sha256=hashlib.sha256(Path(command[0]).read_bytes()).hexdigest(),
                         active_candidate_sha256=identity)
            invalid = {'wrong_pid': ('pid', os.getppid()),
                       'wrong_nonce': ('host_nonce', 'stale'),
                       'wrong_binary': ('active_sha256', 'f' * 64),
                       'wrong_candidate': ('active_candidate_sha256', 'f' * 64)}
            if MODE in invalid:
                field, value = invalid[MODE]
                ready[field] = value
            atomic(control / 'supervisor.json', ready)
            event('running')
        started = time.monotonic()
        while True:
            if MODE == 'ready_failure' and time.monotonic() - started > .04:
                raise SystemExit(9)
            trigger = control / 'test-next.json'
            if trigger.exists():
                action = json.loads(trigger.read_text())
                trigger.unlink()
                if action.get('action') == 'crash':
                    raise SystemExit(9)
                if action.get('action') == 'runtime_failure':
                    raise SystemExit(76)
                if action.get('action') == 'forged_exit':
                    raise SystemExit(75)
                atomic(control / 'handoff-request.json', dict(
                    format_version=1,
                    nonce=action.get('nonce', args.host_nonce),
                    base_candidate_sha256=action.get('base_candidate_sha256', identity),
                    request=action['request'], command=action['command']))
                event('handoff')
                raise SystemExit(75)
            time.sleep(.01)
    finally:
        close()


if __name__ == '__main__':
    main()
'''


@unittest.skipUnless(sys.platform.startswith('linux'), '需要 Linux 的真实会话和 /proc 进程证据')
class HostRuntimeBridgeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='eve-host-bridge-')
        self.root = Path(self.temporary.name).resolve()
        self.control = self.root / 'control'
        self.candidates = self.root / 'candidates'
        self.initial = self.root / 'initial'
        self.state = self.root / 'state'
        for path in (self.control, self.candidates, self.initial, self.state):
            path.mkdir()
        self.events = self.root / 'events.jsonl'
        self.business = self.root / 'business.py'
        self.business.write_text(
            'import signal,time\n'
            'signal.signal(signal.SIGTERM,lambda *_:exit(0))\n'
            'signal.signal(signal.SIGINT,lambda *_:exit(0))\n'
            'while True:time.sleep(.02)\n', encoding='utf8')
        self.binary = self.root / 'initial-eve-qqbot'
        self.binary.write_bytes(Path(sys.executable).read_bytes())
        self.binary.chmod(0o700)
        for name, contents in BASE_RESOURCES.items():
            path = self.initial / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)
        self.trusted = self.root / 'trusted'
        self.trusted.mkdir()
        for name, contents in self.runtime_files('A').items():
            (self.trusted / Path(name).name).write_bytes(contents)
        self.command = [str(self.binary), str(self.business),
                        '--agent', str(self.initial / 'AGENT.md'),
                        '--bridge-script', str(self.initial / 'connectors/qqbot/bridge.mjs'),
                        '--state-dir', str(self.state), '--events', str(self.events)]
        self.env = dict(os.environ, PYTHONHOME=sys.base_prefix, PYTHONUTF8='1')
        libdir = sysconfig.get_config_var('LIBDIR')
        if libdir:
            self.env['LD_LIBRARY_PATH'] = libdir + os.pathsep + self.env.get('LD_LIBRARY_PATH', '')
        self.processes = []
        self.logs = []
        self.process = None
        self.bridge_script = BRIDGE
        self.health_seconds = '.15'

    def tearDown(self):
        for process in self.processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
        # SIGKILL 桥接器的测试会有意留下子会话，只清理本测试记录的会话。
        groups = {event['pgid'] for event in self.event_list()}
        groups.update(event['child_pgid'] for event in self.event_list() if event.get('child_pgid'))
        for group in groups:
            if group <= 1 or group == os.getpgrp():
                continue
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for stream in self.logs:
            stream.close()
        self.temporary.cleanup()

    def runtime_files(self, tag, mode='running'):
        source = SUPERVISOR_SOURCE.replace('__TAG__', repr(tag)).replace('__MODE__', repr(mode))
        return {'scripts/self_upgrade_runtime.py': source.encode(),
                'scripts/build_resources.py': ('MARKER = ' + repr(tag + '-build') + '\n').encode(),
                'scripts/source_update.py': ('MARKER = ' + repr(tag + '-source') + '\n').encode()}

    def candidate(self, tag='B', mode='running', runtime_files=None, diff_bytes=None,
                  base_revision=None, source_diff_overrides=None, source_revision=None):
        editable = self.root / ('editable-' + tag)
        editable.mkdir(exist_ok=True)
        for name, contents in (runtime_files or self.runtime_files(tag, mode)).items():
            (editable / Path(name).name).write_bytes(contents)
        payload = dict(BASE_RESOURCES, **{name: (editable / Path(name).name).read_bytes()
                                        for name in RUNTIME_SCRIPTS})
        resources = {name: hashlib.sha256(contents).hexdigest() for name, contents in payload.items()}
        runtime = dict(host_protocol=1, scripts=RUNTIME_SCRIPTS)
        source = {'head': source_revision or '1' * 40, 'diff': hashlib.sha256(b'').hexdigest(),
                  'untracked': hashlib.sha256(b'').hexdigest()}
        binary_sha = file_digest(self.binary)
        if diff_bytes is None:
            diff_bytes = b'--- a/runtime-fixture\n+++ b/runtime-fixture\n@@ -1 +1 @@\n-A\n+B\n'
        source_diff = dict(path='source-diff.patch', sha256=hashlib.sha256(diff_bytes).hexdigest(),
                           base_revision=base_revision or 'a' * 40, target_revision=source['head'])
        source_diff.update(source_diff_overrides or {})
        identity = dict(source=source, binary_sha256=binary_sha, resources=resources, runtime=runtime,
                        source_diff=source_diff)
        sha = value_digest(identity)
        directory = self.candidates / sha
        directory.mkdir()
        (directory / 'source-diff.patch').write_bytes(diff_bytes)
        binary = directory / 'eve-qqbot'
        binary.write_bytes(self.binary.read_bytes())
        binary.chmod(0o700)
        for name, contents in payload.items():
            path = directory / 'resources' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)
        manifest = dict(format_version=1, prepared=True, success=True, applied=False,
                        candidate_sha256=sha, candidate_path=str(binary), binary_sha256=binary_sha,
                        source=source, resources=resources, runtime=runtime, source_diff=source_diff,
                        checks=[{'success': True}] * 4)
        atomic(directory / 'manifest.json', manifest)
        now = int(time.time() * 1000)
        plan = dict(operation='switch_runtime_preserve_state', candidate_sha256=sha,
                    state_schema_version=1, validation_policy='format-clippy-tests-release-build-v1',
                    runtime=runtime)
        lease = dict(format_version=1, candidate_sha256=sha,
                     manifest_sha256=file_digest(directory / 'manifest.json'),
                     plan_sha256=value_digest(plan), validation_sha256=value_digest(manifest['checks']),
                     state_schema_version=1, created_at_ms=now, expires_at_ms=now + 60000)
        request = dict(format_version=1, candidate_sha256=sha, candidate_directory=str(directory),
                       session_id='fixture-session', requested_by='fixture-session', turn_id=1, lease=lease)
        command = list(self.command)
        command[0] = str(binary)
        command[command.index('--agent') + 1] = str(directory / 'resources/AGENT.md')
        command[command.index('--bridge-script') + 1] = str(directory / 'resources/connectors/qqbot/bridge.mjs')
        return request, command

    def launch(self):
        log = (self.root / ('bridge-' + str(len(self.processes)) + '.log')).open('w+b')
        self.logs.append(log)
        self.process = subprocess.Popen(
            [sys.executable, '-B', str(self.bridge_script), '--supervisor', str(self.trusted / 'self_upgrade_runtime.py'),
             '--directory', str(self.control), '--candidates', str(self.candidates),
             '--source-revision', 'a' * 40,
             '--ready-timeout', '2', '--health-seconds', self.health_seconds, '--stop-timeout', '1',
             '--quiet-seconds', '.05', '--drain-timeout', '3',
             '--restart-delay', '.05', '--restart-limit', '1', '--'] + self.command,
            cwd=self.root, env=self.env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
            start_new_session=True)
        self.processes.append(self.process)
        return self.process

    def logs_text(self):
        texts = []
        for log in self.logs:
            log.flush()
            log.seek(0)
            texts.append(log.read().decode(errors='replace'))
        return '\n'.join(texts)

    def event_list(self):
        if not self.events.exists():
            return []
        records = []
        for line in self.events.read_text(encoding='utf8').splitlines():
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                # 读取刚追加了一半的末行，下一轮再观察。
                pass
        return records

    def wait_for(self, predicate, timeout=6):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            result = predicate()
            if result:
                return result
            time.sleep(.015)
        self.fail('等待真实进程证据超时\n' + self.logs_text() + '\n'
                  + json.dumps({p.name: read_json(p) for p in [self.control / 'host-status.json'] if p.exists()},
                               ensure_ascii=False))

    def running(self, tag, after=0):
        def match():
            status_path = self.control / 'host-status.json'
            if not status_path.exists():
                return None
            status = read_json(status_path)
            for record in reversed(self.event_list()[after:]):
                if (record['tag'] == tag and record['phase'] == 'running' and alive(record['pid'])
                        and status.get('phase') == 'running' and status.get('child_pid') == record['pid']):
                    return record
            return None
        return self.wait_for(match)

    def active_candidate(self):
        path = self.control / 'host-active.json'
        if not path.exists():
            return None
        request = read_json(path)['descriptor']['candidate_request']
        return request['candidate_sha256'] if request else None

    def trigger(self, request=None, command=None, **extra):
        action = dict(extra)
        if request is not None:
            action.update(request=request, command=command)
        atomic(self.control / 'test-next.json', action)

    def assert_terminal(self, code=78):
        try:
            result = self.process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            self.fail('桥接器没有在限时内退出\n' + self.logs_text())
        self.assertEqual(result, code, self.logs_text())

    def start_initial(self):
        self.launch()
        return self.running('A')

    def assert_rejected(self, request, command):
        self.start_initial()
        self.trigger(request, command)
        self.assert_terminal()
        self.assertIsNone(self.active_candidate())
        self.assertNotIn('B', [event['tag'] for event in self.event_list()])
        status = read_json(self.control / 'host-status.json')
        self.assertEqual(status['phase'], 'blocked')
        self.assertTrue(status.get('error'))

    def test_same_binary_switch_executes_frozen_supervisor_and_both_dependencies(self):
        request, command = self.candidate()
        initial = self.start_initial()
        self.assertEqual(file_digest(Path(command[0])), file_digest(self.binary))
        self.trigger(request, command)
        replacement = self.running('B')
        self.wait_for(lambda: self.active_candidate() == request['candidate_sha256'])
        self.assertNotEqual(replacement['pid'], initial['pid'])
        self.assertFalse(alive(initial['pid']))
        self.assertFalse(alive(initial['child_pid']))
        self.assertEqual(replacement['dependencies'], ['B-build', 'B-source'])
        self.assertEqual(replacement['sid'], replacement['pid'])
        self.assertNotEqual(replacement['child_pgid'], replacement['pgid'])
        self.assertTrue(alive(replacement['child_pid']))
        active = read_json(self.control / 'host-active.json')
        self.assertEqual(active['descriptor']['command'], command)
        for name, digest in active['scripts'].items():
            self.assertTrue(Path(name).is_absolute())
            self.assertEqual(file_digest(Path(name)), digest)
        self.assertEqual(len(active['scripts']), 3)

    def test_candidate_source_is_frozen_before_other_source_edits(self):
        request, command = self.candidate()
        self.start_initial()
        # 修改可编辑源码不会改变已经冻结的候选包或它的依赖。
        editable = self.root / 'editable-B'
        for name, content in self.runtime_files('unfrozen').items():
            (editable / Path(name).name).write_bytes(content)
        self.trigger(request, command)
        replacement = self.running('B')
        self.wait_for(lambda: self.active_candidate() == request['candidate_sha256'])
        self.assertEqual(replacement['dependencies'], ['B-build', 'B-source'])
        self.assertNotIn('unfrozen', [event['tag'] for event in self.event_list()])

    def test_changed_frozen_script_is_rejected_before_execution(self):
        request, command = self.candidate()
        script = Path(request['candidate_directory']) / 'resources/scripts/self_upgrade_runtime.py'
        script.write_text(script.read_text() + '\n# altered after validation\n')
        self.assert_rejected(request, command)

    def test_symlinked_frozen_script_is_rejected_before_execution(self):
        request, command = self.candidate()
        script = Path(request['candidate_directory']) / 'resources/scripts/source_update.py'
        external = self.root / 'external.py'
        script.replace(external)
        script.symlink_to(external)
        self.assert_rejected(request, command)

    def test_symlinked_resource_root_is_rejected_before_execution(self):
        request, command = self.candidate()
        resources = Path(request['candidate_directory']) / 'resources'
        external = self.root / 'external-resources'
        resources.replace(external)
        resources.symlink_to(external, target_is_directory=True)
        self.assert_rejected(request, command)

    def test_unlisted_pycache_is_rejected_before_execution(self):
        request, command = self.candidate()
        directory = Path(request['candidate_directory']) / 'resources/scripts/__pycache__'
        directory.mkdir()
        (directory / 'source_update.cpython-test.pyc').write_bytes(b'not declared')
        self.assert_rejected(request, command)

    def test_missing_runtime_dependency_is_rejected_before_execution(self):
        request, command = self.candidate()
        (Path(request['candidate_directory']) / 'resources/scripts/build_resources.py').unlink()
        self.assert_rejected(request, command)

    def assert_rollback(self, mode):
        request, command = self.candidate(mode=mode)
        initial = self.start_initial()
        count = len(self.event_list())
        self.trigger(request, command)
        self.wait_for(lambda: any(event['tag'] == 'B' for event in self.event_list()[count:]))
        restored = self.running('A', after=count)
        self.wait_for(lambda: (self.control / 'host-handoff.json').exists()
                      and read_json(self.control / 'host-handoff.json')['phase'] == 'rolled_back')
        self.assertNotEqual(restored['pid'], initial['pid'])
        self.assertIsNone(self.active_candidate())
        self.assertIsNone(self.process.poll(), self.logs_text())
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'B', 'A'])
        for event in self.event_list():
            if event['tag'] == 'B':
                self.assertFalse(alive(event['pid']))
                if event['child_pid']:
                    self.assertFalse(alive(event['child_pid']))
        # 重启宿主后隔离仍有效，同一失败候选不能反复试启。
        self.process.terminate()
        self.assert_terminal(code=0)
        count = len(self.event_list())
        self.launch()
        self.running('A', after=count)
        self.trigger(request, command)
        self.assert_terminal()
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'B', 'A', 'A'])

    def test_failed_supervisor_startup_rolls_back_without_losing_new_state(self):
        self.assert_rollback('startup_failure')

    def test_missing_readiness_rolls_back_without_losing_new_state(self):
        self.assert_rollback('never_ready')

    def test_failure_in_health_window_rolls_back_without_losing_new_state(self):
        self.assert_rollback('ready_failure')

    def test_dependency_import_failure_rolls_back_without_executing_candidate_main(self):
        payload = self.runtime_files('B')
        payload['scripts/source_update.py'] = b"raise RuntimeError('fixture import failure')\n"
        request, command = self.candidate(runtime_files=payload)
        self.start_initial()
        count = len(self.event_list())
        self.trigger(request, command)
        self.running('A', after=count)
        self.wait_for(lambda: (self.control / 'host-handoff.json').exists()
                      and read_json(self.control / 'host-handoff.json')['phase'] == 'rolled_back')
        self.assertIsNone(self.active_candidate())
        self.assertNotIn('B', [event['tag'] for event in self.event_list()])
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'A'])
        self.assertIn('EVE_HOST_RUNTIME_STOPPED RuntimeError', self.logs_text())

    def test_readiness_must_bind_pid_nonce_binary_and_candidate(self):
        for mode in ('wrong_pid', 'wrong_nonce', 'wrong_binary', 'wrong_candidate'):
            with self.subTest(field=mode):
                # 每次试启使用独立候选，不重用已隔离的身份。
                request, command = self.candidate(tag=mode, mode=mode)
                if self.process is None:
                    self.start_initial()
                count = len(self.event_list())
                self.trigger(request, command)
                restored = self.running('A', after=count)
                self.wait_for(lambda: read_json(self.control / 'host-handoff.json')['phase'] == 'rolled_back')
                self.assertIsNone(self.active_candidate())
                self.assertTrue(alive(restored['child_pid']))
                self.assertTrue((self.control / 'quarantine' / (request['candidate_sha256'] + '.json')).exists())

    def test_committed_runtime_is_restored_on_restart_without_replaying_handoff(self):
        request, command = self.candidate()
        self.start_initial()
        self.trigger(request, command)
        original = self.running('B')
        self.wait_for(lambda: self.active_candidate() == request['candidate_sha256'])
        self.process.terminate()
        self.assert_terminal(code=0)
        count = len(self.event_list())
        self.launch()
        restored = self.running('B', after=count)
        self.assertNotEqual(original['pid'], restored['pid'])
        self.assertEqual(restored['dependencies'], ['B-build', 'B-source'])
        self.assertEqual(self.active_candidate(), request['candidate_sha256'])
        self.assertFalse((self.control / 'handoff-request.json').exists())
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'B', 'B'])

    def install_crash_barrier(self, phase):
        # 包装器只在真实原子写入完成后暂停宿主，避免轮询错过微秒级 journal 阶段。
        # 被测试的桥接器、协议和引导子进程均使用原文件。
        self.bridge_script = self.root / 'crash-barrier.py'
        self.bridge_script.write_text(
            'import os,signal,sys\n'
            'from pathlib import Path\n'
            'sys.path.insert(0,' + repr(str(BRIDGE.parent)) + ')\n'
            'import host_runtime_bridge as bridge\n'
            'atomic=bridge.contract.atomic\n'
            'def barrier(path,value):\n'
            ' atomic(path,value)\n'
            ' marker=Path(' + repr(str(self.control / 'test-crash-phase')) + ')\n'
            ' if marker.exists():\n'
            '  phase=marker.read_text()\n'
            '  journal=Path(path).name=="host-handoff.json" and value.get("phase")==phase\n'
            '  if journal:\n'
            '   marker.unlink()\n'
            '   os.kill(os.getpid(),signal.SIGSTOP)\n'
            'bridge.contract.atomic=barrier\n'
            'raise SystemExit(bridge.main())\n', encoding='utf8')
        (self.control / 'test-crash-phase').write_text(phase)

    def stop_owned_session(self, session):
        groups = set()
        for event in self.event_list():
            if event.get('sid') != session:
                continue
            for key in ('pgid', 'child_pgid'):
                if event.get(key):
                    groups.add(event[key])
        for group in groups:
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.wait_for(lambda: not any(alive(event['pid'])
                      or event.get('child_pid') and alive(event['child_pid'])
                      for event in self.event_list() if event.get('sid') == session))

    def journal_crash(self, phase):
        request, command = self.candidate()
        if phase != 'trial_started':
            self.install_crash_barrier(phase)
        else:
            self.health_seconds = '.5'
        self.start_initial()
        self.trigger(request, command)
        if phase == 'trial_started':
            self.wait_for(lambda: any(event['tag'] == 'B' and event['phase'] == 'running'
                                     for event in self.event_list()))
            self.process.send_signal(signal.SIGSTOP)
        def paused():
            if self.process.poll() is not None:
                return False
            fields = Path('/proc') / str(self.process.pid) / 'stat'
            return fields.read_text().split(') ', 1)[1].split()[0] == 'T'
        self.wait_for(paused)
        self.assertTrue((self.control / 'host-handoff.json').exists())
        if phase in ('committed', 'trial_started'):
            self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'],
                             'trial' if phase == 'trial_started' else 'committed')
            self.wait_for(lambda: any(event['tag'] == 'B' and event['phase'] == 'running'
                                     for event in self.event_list()))
        self.process.kill()
        self.process.wait(timeout=3)
        # 模拟整个服务 cgroup 在宿主重启时消失；仅终止本测试拥有的会话。
        for session in {event['sid'] for event in self.event_list()}:
            self.stop_owned_session(session)
        self.bridge_script = BRIDGE
        count = len(self.event_list())
        self.launch()
        expected_tag = 'B' if phase == 'committed' else 'A'
        self.running(expected_tag, after=count)
        expected_sha = request['candidate_sha256'] if phase == 'committed' else None
        self.assertEqual(self.active_candidate(), expected_sha)
        self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'],
                         'committed' if phase == 'committed' else 'rolled_back')
        self.assertFalse((self.control / 'handoff-request.json').exists())
        visits = read_json(self.state / 'sentinel.json')['visits']
        self.assertEqual(visits, ['A', 'B', expected_tag] if phase in ('committed', 'trial_started') else ['A', 'A'])
        boots = [event for event in self.event_list()[count:] if event['phase'] == 'boot']
        self.assertEqual([event['tag'] for event in boots], [expected_tag])
        if phase != 'committed':
            self.assertTrue((self.control / 'quarantine' / (request['candidate_sha256'] + '.json')).exists())

    def test_kill_after_prepared_recovers_without_replaying_candidate(self):
        self.journal_crash('prepared')

    def test_kill_after_old_closed_recovers_without_replaying_candidate(self):
        self.journal_crash('old_closed')

    def test_kill_after_trial_journal_recovers_without_replaying_candidate(self):
        self.journal_crash('trial')

    def test_kill_during_live_trial_rolls_back_and_preserves_candidate_state(self):
        self.journal_crash('trial_started')

    def test_kill_after_commit_recovers_committed_runtime_without_replay(self):
        self.journal_crash('committed')

    def test_source_diff_contract_rejects_unreviewable_or_unbound_patches(self):
        spec = importlib.util.spec_from_file_location('host_contract_fixture', BRIDGE.with_name('host_runtime_contract.py'))
        contract = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(contract)
        invalid = [
            ('utf8', b'\xff', {}, None),
            ('size', b'a' * (128 * 1024 + 1), {}, None),
            ('git-binary', b'GIT binary patch\n', {}, None),
            ('binary-summary', b'Binary files a/image and b/image differ\n', {}, None),
            ('submodule-summary', b'Submodule src 1111111..2222222:\n', {}, None),
            ('submodule-mode', b'new file mode 160000\n', {}, None),
            ('submodule-index', b'index 1111111..2222222 160000\n', {}, None),
            ('path', b'diff', {'path': '../source-diff.patch'}, None),
            ('hash', b'diff', {'sha256': 'f' * 64}, None),
            ('target', b'diff', {'target_revision': 'b' * 40}, None),
            ('invalid-base', b'diff', {}, 'invalid'),
        ]
        for name, patch, overrides, base in invalid:
            with self.subTest(patch=name):
                request, _ = self.candidate(tag=name, diff_bytes=patch,
                                            source_diff_overrides=overrides, base_revision=base)
                with self.assertRaises((ValueError, UnicodeDecodeError)):
                    contract.validate_candidate(request, self.candidates, require_runtime=True)
        request, _ = self.candidate(tag='missing-diff')
        manifest_path = Path(request['candidate_directory']) / 'manifest.json'
        manifest = read_json(manifest_path)
        del manifest['source_diff']
        atomic(manifest_path, manifest)
        for required in (False, True):
            with self.subTest(runtime_required=required):
                with self.assertRaisesRegex(ValueError, 'host_source_diff_required'):
                    contract.validate_candidate(request, self.candidates, require_runtime=required)
        self.assertFalse(self.events.exists(), '纯宿主验证不得执行任何候选代码')

    def test_source_diff_from_another_active_revision_cannot_authorize_handoff(self):
        request, command = self.candidate(base_revision='b' * 40)
        self.assert_rejected(request, command)
        self.assertEqual(read_json(self.control / 'host-status.json')['error'], 'host_source_diff_base_changed')

    def test_committed_runtime_failure_restores_previous_scripts_and_keeps_state(self):
        request, command = self.candidate()
        self.start_initial()
        self.trigger(request, command)
        failed = self.running('B')
        self.wait_for(lambda: self.active_candidate() == request['candidate_sha256'])
        count = len(self.event_list())
        self.trigger(action='runtime_failure')
        restored = self.running('A', after=count)
        self.assertIsNone(self.active_candidate())
        self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'], 'rolled_back')
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'B', 'A'])
        self.assertEqual(restored['dependencies'], ['A-build', 'A-source'])
        self.assertFalse(alive(failed['pid']))
        self.assertFalse(alive(failed['child_pid']))

    def test_kill_during_rollback_recovers_previous_scripts_without_retrying_failed_runtime(self):
        request, command = self.candidate()
        self.install_crash_barrier('rolling_back')
        self.start_initial()
        self.trigger(request, command)
        self.running('B')
        self.wait_for(lambda: self.active_candidate() == request['candidate_sha256'])
        self.trigger(action='runtime_failure')
        self.wait_for(lambda: Path('/proc', str(self.process.pid), 'stat').read_text().split(') ', 1)[1].split()[0] == 'T')
        self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'], 'rolling_back')
        self.assertEqual(self.active_candidate(), request['candidate_sha256'])
        self.process.kill()
        self.process.wait(timeout=3)
        for session in {event['sid'] for event in self.event_list()}:
            self.stop_owned_session(session)
        self.bridge_script = BRIDGE
        count = len(self.event_list())
        self.launch()
        self.running('A', after=count)
        self.assertIsNone(self.active_candidate())
        self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'], 'rolled_back')
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'B', 'A'])
        self.assertTrue((self.control / 'quarantine' / (request['candidate_sha256'] + '.json')).exists())
        self.assertEqual([event['tag'] for event in self.event_list()[count:] if event['phase'] == 'boot'], ['A'])

    def prepare_failed_trial_with_committed_fallback(self):
        request_b, command_b = self.candidate()
        request_c, command_c = self.candidate(tag='C', mode='startup_failure',
                                              base_revision='1' * 40, source_revision='2' * 40)
        self.start_initial()
        self.trigger(request_b, command_b)
        self.running('B')
        self.wait_for(lambda: self.active_candidate() == request_b['candidate_sha256'])
        count = len(self.event_list())
        self.trigger(request_c, command_c)
        self.running('B', after=count)
        self.wait_for(lambda: read_json(self.control / 'host-handoff.json')['phase'] == 'rolled_back')
        self.assertEqual(self.active_candidate(), request_b['candidate_sha256'])
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], ['A', 'B', 'C', 'B'])
        self.assertTrue((self.control / 'quarantine' / (request_c['candidate_sha256'] + '.json')).exists())
        return request_b

    def assert_failed_trial_preserves_committed_fallback(self, restart=False):
        request_b = self.prepare_failed_trial_with_committed_fallback()
        if restart:
            self.process.terminate()
            self.assert_terminal(code=0)
            count = len(self.event_list())
            self.launch()
            self.running('B', after=count)
        count = len(self.event_list())
        self.trigger(action='runtime_failure')
        restored = self.running('A', after=count)
        self.assertIsNone(self.active_candidate())
        self.assertEqual(restored['dependencies'], ['A-build', 'A-source'])
        self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'], 'rolled_back')
        expected = ['A', 'B', 'C', 'B', 'B', 'A'] if restart else ['A', 'B', 'C', 'B', 'A']
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], expected)
        self.assertTrue((self.control / 'quarantine' / (request_b['candidate_sha256'] + '.json')).exists())
        self.assertEqual([event['tag'] for event in self.event_list()[count:] if event['phase'] == 'boot'], ['A'])

    def test_failed_c_trial_preserves_a_as_fallback_for_committed_b(self):
        self.assert_failed_trial_preserves_committed_fallback()

    def test_failed_c_trial_preserves_b_fallback_across_bridge_restart(self):
        self.assert_failed_trial_preserves_committed_fallback(restart=True)

    def test_anchor_capacity_rejection_restores_running_a_without_executing_b(self):
        request, command = self.candidate()
        initial = self.start_initial()
        active = self.control / 'host-active.json'
        before_active = active.read_bytes()
        before_state = read_json(self.state / 'sentinel.json')
        anchors = self.control / 'host-anchors'
        # 原有 A 锚点加 15 个独立且形状合法的条目，正好占满 16 个名额。
        for index in range(15):
            key = value_digest({'unrelated_fixture': index})
            atomic(anchors / (key + '.json'), dict(format_version=1, active_sha256=key, previous=None))
        before_anchors = {path.name: path.read_bytes() for path in anchors.iterdir()}
        self.assertEqual(len(before_anchors), 17)  # 包括 version.json。
        count = len(self.event_list())
        self.trigger(request, command)
        restored = self.running('A', after=count)
        self.wait_for(lambda: read_json(self.control / 'host-status.json').get('result') == 'rejected')
        status = read_json(self.control / 'host-status.json')
        self.assertEqual(status['phase'], 'running')
        self.assertEqual(status['error'], 'host_anchor_storage_limit')
        self.assertNotEqual(restored['pid'], initial['pid'])
        self.assertTrue(alive(restored['child_pid']))
        self.assertFalse(alive(initial['pid']))
        self.assertFalse(alive(initial['child_pid']))
        self.assertEqual(active.read_bytes(), before_active)
        self.assertIsNone(self.active_candidate())
        self.assertEqual(read_json(self.state / 'sentinel.json')['visits'], before_state['visits'] + ['A'])
        self.assertNotIn('B', [event['tag'] for event in self.event_list()])
        self.assertEqual({path.name: path.read_bytes() for path in anchors.iterdir()}, before_anchors)
        self.assertEqual(read_json(self.control / 'host-handoff.json')['phase'], 'rolled_back')
        self.assertFalse((self.control / 'handoff-request.json').exists())
        self.assertIsNone(self.process.poll(), self.logs_text())

    def assert_missing_or_corrupt_active_anchor_blocks_restart(self, corrupt=False):
        request_b = self.prepare_failed_trial_with_committed_fallback()
        self.process.terminate()
        self.assert_terminal(code=0)
        active = self.control / 'host-active.json'
        before_active = active.read_bytes()
        anchor = self.control / 'host-anchors' / (value_digest(read_json(active)) + '.json')
        self.assertTrue(anchor.exists())
        damaged = b'{"format_version":1,"active_sha256":"corrupt","previous":null}\n'
        if corrupt:
            anchor.write_bytes(damaged)
        else:
            anchor.unlink()
        before_events = self.event_list()
        before_state = (self.state / 'sentinel.json').read_bytes()
        self.launch()
        self.assert_terminal()
        self.assertEqual(self.event_list(), before_events)
        self.assertEqual((self.state / 'sentinel.json').read_bytes(), before_state)
        self.assertEqual(active.read_bytes(), before_active)
        self.assertEqual(self.active_candidate(), request_b['candidate_sha256'])
        if corrupt:
            self.assertEqual(anchor.read_bytes(), damaged)
            self.assertIn('host_invalid_anchor_preserved', self.logs_text())
        else:
            self.assertFalse(anchor.exists(), '缺失的回退锚点不能被自动重建成无前版')

    def test_missing_b_anchor_after_failed_c_blocks_restart_without_executing_b(self):
        self.assert_missing_or_corrupt_active_anchor_blocks_restart()

    def test_corrupt_b_anchor_after_failed_c_blocks_restart_preserving_evidence(self):
        self.assert_missing_or_corrupt_active_anchor_blocks_restart(corrupt=True)

    def test_real_supervisor_cli_hands_off_and_restores_frozen_runtime(self):
        runtime_files = {name: BRIDGE.with_name(Path(name).name).read_bytes() for name in RUNTIME_SCRIPTS}
        for name, contents in runtime_files.items():
            (self.trusted / Path(name).name).write_bytes(contents)
        (self.initial / 'connectors/qqbot/node_modules').mkdir()
        self.business.write_text(
            'import json,os,signal,sys,time\n'
            'from pathlib import Path\n'
            'signal.signal(signal.SIGINT,lambda *_:exit(0))\n'
            'signal.signal(signal.SIGTERM,lambda *_:exit(0))\n'
            'events=Path(sys.argv[sys.argv.index("--events")+1])\n'
            'with events.open("a") as stream:stream.write(json.dumps(dict('
            'tag="business",phase="running",pid=os.getpid(),pgid=os.getpgrp(),'
            'sid=os.getsid(0),child_pid=None,child_pgid=None,exe=sys.argv[0],'
            'agent=sys.argv[sys.argv.index("--agent")+1]))+"\\n")\n'
            'print("EVE_QQBOT_READY",file=sys.stderr,flush=True)\n'
            'while True:time.sleep(.02)\n', encoding='utf8')
        message = dict(scope='c2c', target_id='person', user_id='person', text='升级')
        session = 'qq:' + hashlib.sha256(json.dumps(
            ['app', 'c2c', 'person', 'person'], separators=(',', ':')).encode()).hexdigest()
        turn = dict(id=1, input='升级', status=dict(state='Completed', messages=[
            dict(role='assistant', text='已提交升级')]))
        sessions = dict(format_version=1, sessions={session: dict(
            key=dict(user_id=session, session_id=session), turns=[turn])})
        receipts = dict(version=2, entries=[dict(app_id='app', state='Sent', reply='已提交升级', message=message)])
        atomic(self.state / 'state.json', dict(version=1, entries={
            'eve.session': {'sessions.v1': list(json.dumps(sessions).encode())},
            'eve.channel.qqbot': {'receipts.v1': list(json.dumps(receipts).encode())}}))
        request, _ = self.candidate(runtime_files=runtime_files)
        request.update(session_id=session, requested_by=session)
        self.launch()
        self.wait_for(lambda: (self.control / 'host-status.json').exists()
                      and read_json(self.control / 'host-status.json')['phase'] == 'running')
        initial = self.wait_for(lambda: self.event_list() and self.event_list()[-1])
        initial_supervisor = read_json(self.control / 'supervisor.json')['pid']
        atomic(self.control / 'request.json', request)
        self.wait_for(lambda: self.active_candidate() == request['candidate_sha256'], timeout=9)
        self.wait_for(lambda: read_json(self.control / 'host-status.json')['phase'] == 'running')
        self.wait_for(lambda: read_json(self.control / 'supervisor.json').get('result') == 'applied')
        current = read_json(self.control / 'supervisor.json')
        self.assertNotEqual(current['pid'], initial_supervisor)
        self.assertEqual(current['active_candidate_sha256'], request['candidate_sha256'])
        self.assertEqual(current['candidate_sha256'], request['candidate_sha256'])
        self.assertEqual(current['result'], 'applied')
        self.assertIsNone(current['error'])
        self.assertEqual(current['active_candidate_sha256'], self.active_candidate())
        self.assertFalse(alive(initial['pid']))
        self.assertFalse(alive(initial_supervisor))
        self.assertEqual(len(self.event_list()), 2)
        business = self.event_list()[-1]
        self.assertTrue(alive(business['pid']))
        self.assertEqual(business['sid'], current['pid'])
        self.assertNotEqual(business['pgid'], current['pid'])
        self.assertEqual(business['agent'], str(Path(request['candidate_directory']) / 'resources/AGENT.md'))
        self.assertFalse((self.control / 'handoff-request.json').exists())
        self.assertFalse((self.control / 'processing.json').exists())
        self.assertEqual(read_json(self.state / 'state.json')['entries']['eve.session']['sessions.v1'],
                         list(json.dumps(sessions).encode()))
        self.process.terminate()
        self.assert_terminal(code=0)
        self.assertFalse(alive(business['pid']))

    def test_stale_nonce_cannot_authorize_a_candidate(self):
        request, command = self.candidate()
        self.start_initial()
        self.trigger(request, command, nonce='expired-supervisor-nonce')
        self.assert_terminal()
        self.assertIsNone(self.active_candidate())
        self.assertNotIn('B', [event['tag'] for event in self.event_list()])

    def test_wrong_base_candidate_cannot_authorize_a_candidate(self):
        request, command = self.candidate()
        self.start_initial()
        self.trigger(request, command, base_candidate_sha256='f' * 64)
        self.assert_terminal()
        self.assertIsNone(self.active_candidate())
        self.assertNotIn('B', [event['tag'] for event in self.event_list()])

    def test_exit_75_without_handoff_is_terminal_and_does_not_apply(self):
        self.start_initial()
        self.trigger(action='forged_exit')
        self.assert_terminal()
        self.assertIsNone(self.active_candidate())
        self.assertEqual([event['tag'] for event in self.event_list() if event['phase'] == 'boot'], ['A'])


    def test_normal_stop_closes_orphaned_worker_in_separate_process_group(self):
        self.business.write_text(
            'import json,os,signal,subprocess,sys,time\n'
            'from pathlib import Path\n'
            'signal.signal(signal.SIGTERM,lambda *_:exit(0))\n'
            'signal.signal(signal.SIGINT,lambda *_:exit(0))\n'
            'worker=subprocess.Popen([sys.executable,"-c",'
            '"import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(90)"],process_group=0)\n'
            'events=Path(sys.argv[sys.argv.index("--events")+1])\n'
            'with events.open("a") as stream:stream.write(json.dumps(dict('
            'tag="worker",phase="spawned",pid=worker.pid,pgid=os.getpgid(worker.pid),'
            'sid=os.getsid(worker.pid),child_pid=None,child_pgid=None))+"\\n")\n'
            'while True:time.sleep(.02)\n', encoding='utf8')
        initial = self.start_initial()
        worker = self.wait_for(lambda: next((event for event in self.event_list()
                                           if event['tag'] == 'worker'), None))
        self.assertEqual(worker['sid'], initial['sid'])
        self.assertNotIn(worker['pgid'], (initial['pgid'], initial['child_pgid']))
        self.process.terminate()
        self.assert_terminal(code=0)
        self.assertFalse(alive(initial['pid']))
        self.assertFalse(alive(initial['child_pid']))
        self.assertFalse(alive(worker['pid']))

    def test_normal_stop_closes_both_processes_without_restart(self):
        initial = self.start_initial()
        self.process.terminate()
        self.assert_terminal(code=0)
        self.assertFalse(alive(initial['pid']))
        self.assertFalse(alive(initial['child_pid']))
        self.assertEqual([event['tag'] for event in self.event_list() if event['phase'] == 'boot'], ['A'])

    def test_bridge_kill_and_restart_refuses_duplicate_live_supervisor(self):
        initial = self.start_initial()
        self.process.kill()
        self.process.wait(timeout=3)
        self.assertTrue(alive(initial['pid']))
        self.assertTrue(alive(initial['child_pid']))
        self.launch()
        self.assert_terminal()
        self.assertTrue(alive(initial['pid']))
        self.assertTrue(alive(initial['child_pid']))
        self.assertEqual([event['tag'] for event in self.event_list() if event['phase'] == 'boot'], ['A'])

    def test_unexpected_crash_budget_and_blocked_state_survive_bridge_restart(self):
        initial = self.start_initial()
        count = len(self.event_list())
        self.trigger(action='crash')
        replacement = self.running('A', after=count)
        self.assertNotEqual(replacement['pid'], initial['pid'])
        self.trigger(action='crash')
        self.assert_terminal()
        recovery = read_json(self.control / 'host-recovery.json')
        self.assertEqual(recovery['format_version'], 1)
        self.assertEqual(len(recovery['crashes']), 2)
        self.assertTrue(all(type(value) is int for value in recovery['crashes']))
        before = self.event_list()
        self.launch()
        self.assert_terminal()
        self.assertEqual(self.event_list(), before)
        self.assertEqual(read_json(self.control / 'host-recovery.json'), recovery)


if __name__ == '__main__':
    unittest.main()
