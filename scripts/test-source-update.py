"""本地真实 Git、检查子进程与升级子进程：不用 QQ 或外部模型。生产 Cargo 验收另行执行。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import source_update as update

spec = importlib.util.spec_from_file_location('update_fixture', Path(__file__).with_name('test-self-upgrade-runtime.py'))
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)
runtime = fixture.runtime


class SourceUpdateTests(unittest.TestCase):
    def setUp(self):
        fixture.UpgradeRuntimeTests.setUpClass()
        self.fixture = fixture.UpgradeRuntimeTests(); self.fixture.setUp()
        self.root = self.fixture.root
        self.original_env = dict(os.environ)
        os.environ.update(self.fixture.env)
        self.remote = self.root / 'upstream'; self.remote.mkdir()
        self.git('init', '-b', 'main')
        for name in ['AGENT.md', 'connectors/qqbot/bridge.mjs', 'connectors/qqbot/bridge-core.mjs',
                     'connectors/qqbot/package.json', 'connectors/qqbot/package-lock.json']:
            p = self.remote / name; p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text('new' if name == 'AGENT.md' else '{}')
        self.git('add', '.'); self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'first')
        self.head = self.git('rev-parse', 'HEAD')
        cargo = self.root / 'cargo.py'
        cargo.write_text('''import sys,os
from pathlib import Path
marker=Path(__file__).parent/'fail-check'
if marker.exists():raise SystemExit(3)
if sys.argv[1]=='build':
 target=Path(sys.argv[sys.argv.index('--target-dir')+1])/'release'
 target.mkdir(parents=True,exist_ok=True)
 binary=target/('eve-qqbot.exe' if os.name=='nt' else 'eve-qqbot')
 binary.write_bytes(Path(sys.executable).read_bytes()+b'new');binary.chmod(0o700)
print('fixture check passed')
''')
        self.config = self.root / 'auto-update-config.json'
        self.settings = dict(version=1, data_directory=str(self.root / 'update'), cargo_program=sys.executable,
                             cargo_arguments=[str(cargo)], minimum_free_bytes=256*1024**2)
        runtime.atomic(self.config, self.settings)
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
            builder_factory=lambda *args: update.GitBuilder(*args, remote=str(self.remote)))
        self.supervisor = None

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.remote, stderr=subprocess.DEVNULL).decode().strip()

    def tearDown(self):
        self.updater.stop()
        if self.supervisor:
            self.supervisor.child.stop(3); self.supervisor.finished.set(); self.supervisor.lock.close()
        self.fixture.tearDown()
        os.environ.clear(); os.environ.update(self.original_env)

    def prepare(self, active=None):
        settings = update.configuration(self.config)
        return update.GitBuilder(settings, self.fixture.candidates, threading.Event(), lambda **_: None,
                                 remote=str(self.remote)).prepare(active)

    def supervisor_start(self):
        options = SimpleNamespace(directory=self.fixture.queue, candidates=self.fixture.candidates,
            command=self.fixture.command, ready_timeout=3, stop_timeout=3, quiet_seconds=.05,
            health_seconds=.05)
        self.supervisor = runtime.Supervisor(options)
        self.supervisor.child = runtime.Child(self.fixture.command)
        self.supervisor.wait_ready(self.supervisor.child)
        return self.supervisor

    def ticks(self, predicate, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.updater.tick(self.supervisor)
            if predicate(): return
            time.sleep(.05)
        self.fail(json.dumps(self.updater.status))

    def test_real_git_verification_waits_for_delivery_then_switches_and_second_check_is_noop(self):
        self.fixture.write_state('Pending', 'Processing')
        supervisor = self.supervisor_start()
        self.ticks(lambda: self.updater.pending is not None)
        self.ticks(lambda: self.updater.status.get('result') == 'waiting_for_idle')
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])
        self.fixture.write_state('Completed', 'Sent')
        before = (self.fixture.state / 'state.json').read_bytes()
        self.ticks(lambda: self.updater.status.get('result') == 'applied')
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])
        self.assertEqual((self.fixture.state / 'state.json').read_bytes(), before)
        self.assertEqual(self.updater.status['current_head'], self.head)
        self.assertEqual(len(list((self.fixture.queue / 'history').glob('*applied.json'))), 1)
        self.updater.next_check = 0
        self.ticks(lambda: self.updater.status.get('up_to_date'))
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])
        self.assertEqual(runtime.read_json(self.fixture.queue / 'supervisor.json')['active_sha256'], supervisor.status['active_sha256'])

    def test_check_failure_does_not_publish_candidate_or_touch_old_instance(self):
        (self.root / 'fail-check').touch()
        self.supervisor_start()
        self.ticks(lambda: self.updater.status.get('result') == 'failed')
        self.assertEqual(self.updater.status['error'], 'auto_update_check_failed')
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])
        self.assertFalse((self.fixture.queue / 'request.json').exists())

    def test_prepared_resources_are_frozen_and_local_checkout_changes_refuse_overwrite(self):
        result = self.prepare()
        _, resources, manifest = runtime.validate_candidate(result['request'], self.fixture.candidates)
        (self.root / 'update/source/AGENT.md').write_text('local edit')
        with self.assertRaisesRegex(ValueError, 'checkout_has_local_changes'): self.prepare()
        self.assertEqual((resources / 'AGENT.md').read_text(), 'new')
        self.assertEqual(len(manifest['checks']), 4)
        self.assertEqual((self.root / 'update/source/AGENT.md').read_text(), 'local edit')

    def test_no_downgrade_or_unrelated_activated_branch(self):
        self.prepare()
        (self.remote / 'AGENT.md').write_text('future')
        self.git('add', '.'); self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'future')
        future = self.git('rev-parse', 'HEAD')
        self.git('checkout', '--detach', self.head)
        self.git('branch', '-f', 'main', self.head)
        with self.assertRaisesRegex(ValueError, 'active_source_not_upstream'): self.prepare(future)

    def test_disable_discards_prepared_update_and_keeps_running_binary(self):
        self.fixture.write_state('Pending', 'Processing'); self.supervisor_start()
        self.ticks(lambda: self.updater.pending is not None)
        runtime.atomic(self.config, dict(self.settings, enabled=False))
        self.ticks(lambda: self.updater.status.get('phase') == 'disabled')
        self.assertIsNone(self.updater.pending)
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])

    def test_no_backup_space_keeps_old_process_online_before_any_switch_request(self):
        self.supervisor_start()
        self.ticks(lambda: self.updater.pending is not None)
        with patch.object(update.shutil, 'disk_usage', return_value=SimpleNamespace(free=1)):
            self.ticks(lambda: self.updater.status.get('error') == 'auto_update_insufficient_space_for_backup')
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])
        self.assertIsNone(self.supervisor.child.process.poll())
        self.assertFalse((self.fixture.queue / 'auto-request.json').exists())

    def test_failed_candidate_returns_to_old_binary_and_preserves_candidate_state(self):
        (self.remote / 'AGENT.md').write_text('fail')
        self.git('add', '.'); self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'bad runtime')
        self.supervisor_start()
        self.ticks(lambda: self.updater.status.get('result') == 'rolled_back_binary_state_preserved')
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'fail', 'old'])
        self.assertIn('synthetic.candidate', runtime.read_json(self.fixture.state / 'state.json')['entries'])
        self.assertIsNotNone(self.updater.status['failed_head'])

    def test_restart_does_not_execute_the_previously_prepared_request(self):
        self.fixture.write_state('Pending', 'Processing'); self.supervisor_start()
        self.ticks(lambda: self.updater.pending is not None)
        self.updater.stop()
        resumed = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates)
        try:
            self.assertIsNone(resumed.pending)
            self.assertTrue(resumed.status['interrupted'])
            self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])
        finally:
            resumed.stop()

    def test_insufficient_space_and_changed_source_do_not_publish_candidate(self):
        runtime.atomic(self.config, dict(self.settings, minimum_free_bytes=2**60))
        with self.assertRaisesRegex(ValueError, 'insufficient_space'): self.prepare()
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])

    def test_configuration_and_build_environment_cannot_supply_remote_credentials(self):
        for change in [dict(repository='https://evil.invalid'), dict(branch='--upload-pack=evil'),
                       dict(branch='../main'), dict(jobs=0), dict(interval_seconds=0), dict(token='secret')]:
            runtime.atomic(self.config, dict(self.settings, **change))
            with self.assertRaises(ValueError): update.configuration(self.config)
        old = os.environ.get('EVE_OPENAI_API_KEY')
        try:
            os.environ['EVE_OPENAI_API_KEY'] = 'synthetic-secret'
            self.assertNotIn('EVE_OPENAI_API_KEY', update.build_environment(self.updater.settings))
        finally:
            if old is None: os.environ.pop('EVE_OPENAI_API_KEY')
            else: os.environ['EVE_OPENAI_API_KEY'] = old

    def test_idle_gate_holds_background_tasks_files_and_live_delivery_without_changing_state(self):
        path = self.fixture.state / 'state.json'
        state = runtime.read_json(path)
        task = {'tasks': [{'status': 'Running'}]}
        state['entries']['eve.task'] = {'tasks.v1': list(json.dumps(task).encode())}
        runtime.atomic(path, state)
        self.assertFalse(runtime.runtime_idle_gate(path))
        task['tasks'][0]['status'] = 'Completed'
        state['entries']['eve.task']['tasks.v1'] = list(json.dumps(task).encode())
        runtime.atomic(path, state)
        self.assertTrue(runtime.runtime_idle_gate(path))

        files = path.parent / 'qq-outbox'; files.mkdir()
        runtime.atomic(files / 'ledger.json', {'format_version': 1, 'records': [{'state': 'Sending'}]})
        before = path.read_bytes()
        self.assertFalse(runtime.runtime_idle_gate(path))
        self.assertEqual(path.read_bytes(), before)
        runtime.atomic(files / 'ledger.json', {'format_version': 1, 'records': [{'state': 'Sent'}]})
        self.assertTrue(runtime.runtime_idle_gate(path))

    def test_format_seven_accepts_real_backlinks_and_keeps_inherited_unknown_delivery(self):
        state, ledger = self.fixture.merged_state()
        ledger['version'] = 7
        ledger['entries'] = list(reversed(ledger['entries']))
        state['entries']['eve.channel.qqbot']['receipts.v1'] = list(json.dumps(ledger).encode())
        runtime.atomic(self.fixture.state / 'state.json', state)
        self.assertTrue(runtime.runtime_idle_gate(self.fixture.state / 'state.json'))
        self.fixture.add_receipt()
        path = self.fixture.state / 'state.json'
        self.assertFalse(runtime.runtime_idle_gate(path))
        inherited = runtime.inherited_receipts(path)
        self.assertTrue(runtime.runtime_idle_gate(path, inherited))



if __name__ == '__main__':
    unittest.main()
