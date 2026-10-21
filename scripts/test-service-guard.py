"""外层守护的 Linux 真实进程验收；不连接 QQ，不影响其他进程。"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPT = Path(__file__).with_name('service_guard.py')
sys.path.insert(0, str(SCRIPT.parent))
spec = importlib.util.spec_from_file_location('service_guard', SCRIPT)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


@unittest.skipUnless(sys.platform.startswith('linux'), '外层守护仅用于 Linux')
class GuardTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='eve-guard-')
        self.root = Path(self.directory.name)
        self.helper = self.root / 'supervisor.py'
        self.helper.write_text('''import os,signal,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1]);mode=sys.argv[2]
with (root/'starts').open('a') as f:f.write(str(os.getpid())+'\\n')
if mode=='exit':raise SystemExit(3)
if mode=='blocked':raise SystemExit(78)
if mode=='normal':raise SystemExit(0)
worker=subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)'],process_group=0)
(root/'worker').write_text(str(worker.pid))
def stop(*_):
 worker.terminate();worker.wait(timeout=3);raise SystemExit(0)
signal.signal(signal.SIGTERM,stop)
while True:time.sleep(.05)
''')
        self.process = None
        self.log = (self.root / 'guard.log').open('ab')

    def tearDown(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()
        self.directory.cleanup()

    def launch(self, mode):
        self.process = subprocess.Popen([sys.executable, str(SCRIPT), '--directory', str(self.root / 'guard'),
                                         '--restart-delay', '.05', '--stop-timeout', '2',
                                         '--restart-limit', '2', '--restart-window-seconds', '10',
                                         '--', sys.executable, str(self.helper), str(self.root), mode],
                                        stdout=self.log, stderr=self.log)

    def wait(self, predicate):
        end = time.monotonic() + 8
        while time.monotonic() < end:
            path = self.root / 'guard/guard.json'
            value = json.loads(path.read_text()) if path.exists() else {}
            if predicate(value):
                return value
            if self.process.poll() is not None:
                self.fail((self.root / 'guard.log').read_text())
            time.sleep(.02)
        self.fail('guard timeout: ' + (self.root / 'guard.log').read_text())

    def test_abrupt_supervisor_death_closes_its_group_before_restarting(self):
        self.launch('worker')
        def ready(value):
            return ((self.root / 'worker').exists()
                    and (self.root / 'worker').read_text() in value.get('members', {}))
        before = self.wait(ready)
        worker = int((self.root / 'worker').read_text())
        os.kill(before['child_pid'], signal.SIGKILL)
        self.wait(lambda v: v.get('phase') == 'running' and v.get('child_pid') != before['child_pid']
                  and len((self.root / 'starts').read_text().splitlines()) == 2)
        self.assertIsNone(guard.identity(worker))
        self.assertEqual(len((self.root / 'starts').read_text().splitlines()), 2)
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=8), 0)
        self.assertEqual(len((self.root / 'starts').read_text().splitlines()), 2)

    def test_unexpected_exit_has_a_persistent_bounded_restart_budget(self):
        self.launch('exit')
        self.assertEqual(self.process.wait(timeout=8), guard.RECOVERY_STOP)
        self.assertEqual(len((self.root / 'starts').read_text().splitlines()), 3)
        status = json.loads((self.root / 'guard/guard.json').read_text())
        self.assertEqual(status['phase'], 'blocked')
        self.assertEqual(status['error'], 'guard_restart_limit_exhausted')
        self.assertEqual(len(json.loads((self.root / 'guard/guard-recovery.json').read_text())['crashes']), 3)

    def test_normal_stop_and_explicit_recovery_block_do_not_restart(self):
        for mode, code in [('normal', 0), ('blocked', guard.RECOVERY_STOP)]:
            with self.subTest(mode=mode):
                self.launch(mode)
                self.assertEqual(self.process.wait(timeout=8), code)
        self.assertEqual(len((self.root / 'starts').read_text().splitlines()), 2)

    def test_live_supervisor_record_is_not_adopted_or_killed(self):
        root = self.root / 'guard'
        root.mkdir()
        (root / 'guard.json').write_text(json.dumps({'child_pid': os.getpid(),
                                                    'child_identity': guard.identity(os.getpid())}))
        self.launch('normal')
        self.assertEqual(self.process.wait(timeout=8), guard.RECOVERY_STOP)
        self.assertFalse((self.root / 'starts').exists())

    def test_real_supervisor_and_guard_preserve_state_across_both_crash_layers(self):
        spec = importlib.util.spec_from_file_location('upgrade_fixture', SCRIPT.with_name('test-self-upgrade-runtime.py'))
        fixture_module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture_module)
        fixture = fixture_module.UpgradeRuntimeTests()
        fixture.setUp()
        self.addCleanup(fixture.tearDown)
        fixture.command.append('--spawn-child')
        core = [sys.executable, str(SCRIPT.with_name('self_upgrade_runtime.py')),
                '--directory', str(fixture.queue), '--candidates', str(fixture.candidates),
                '--ready-timeout', '3', '--stop-timeout', '2', '--health-seconds', '.1',
                '--restart-delay', '.05', '--', *fixture.command]
        self.process = subprocess.Popen([sys.executable, str(SCRIPT), '--directory', str(self.root / 'guard'),
                                         '--restart-delay', '.05', '--stop-timeout', '3', '--', *core],
                                        env=fixture.env, stdout=self.log, stderr=self.log)
        fixture.process = self.process
        fixture.wait(lambda v:v['phase']=='running')
        (fixture.state / 'old.crash').touch()
        fixture.wait(lambda v:v['result']=='runtime_restarted_state_preserved' and v['phase']=='running')
        before = self.wait(lambda v:len(v.get('members', {})) >= 3)
        original = (fixture.state / 'state.json').read_bytes()
        workers = [int(p) for p in (fixture.state / 'workers').read_text().splitlines()]
        self.wait(lambda v:str(workers[-1]) in v.get('members', {}))
        os.kill(before['child_pid'], signal.SIGKILL)
        after = self.wait(lambda v:v.get('phase')=='running' and v.get('child_pid')!=before['child_pid'])
        fixture.wait(lambda v:v['phase']=='running' and v['pid']==after['child_pid'])
        self.assertTrue(all(guard.identity(p) is None for p in workers))
        self.assertEqual((fixture.state / 'state.json').read_bytes(), original)
        self.assertEqual(fixture.events.read_text().splitlines(), ['old', 'old', 'old'])
        self.process.terminate()
        self.assertEqual(self.process.wait(timeout=8), 0)
        fixture.process = None


if __name__ == '__main__':
    unittest.main()
