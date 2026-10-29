"""本地真实 Git、检查子进程与升级子进程：不用 QQ 或外部模型。生产 Cargo 验收另行执行。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import source_update as update
from build_resources import BuildPlanner, Resources, GIB

spec = importlib.util.spec_from_file_location('update_fixture', Path(__file__).with_name('test-self-upgrade-runtime.py'))
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)
runtime = fixture.runtime


class Probe:
    available = 12 * GIB
    def snapshot(self): return Resources(4, 16 * GIB, self.available, 0)


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
        cargo.write_text('''import sys,os,json
from pathlib import Path
root=Path(__file__).parent
with (root/'cargo-calls.jsonl').open('a') as f:
 f.write(json.dumps({'args':sys.argv[1:],'jobs':os.environ.get('CARGO_BUILD_JOBS'),'threads':os.environ.get('RUST_TEST_THREADS')})+'\\n')
if (root/'fail-check').exists():raise SystemExit(3)
if sys.argv[1]=='metadata':
 print(json.dumps({'packages':[{'name':'eve-app','targets':[{'name':name,'kind':['bin']} for name in ['eve-qqbot','eve-helper']]}]}))
 raise SystemExit(0)
if sys.argv[1]=='build' and '--bin' in sys.argv:
 target=Path(sys.argv[sys.argv.index('--target-dir')+1])/'release'
 target.mkdir(parents=True,exist_ok=True)
 name=sys.argv[sys.argv.index('--bin')+1]
 binary=target/(name+'.exe' if os.name=='nt' else name)
 binary.write_bytes(Path(sys.executable).read_bytes()+b'new');binary.chmod(0o700)
print('fixture check passed')
''')
        self.probe = Probe()
        self.config = self.root / 'auto-update-config.json'
        self.settings = dict(version=1, data_directory=str(self.root / 'update'), cargo_program=sys.executable,
                             cargo_arguments=[str(cargo)], minimum_free_bytes=256*1024**2)
        runtime.atomic(self.config, self.settings)
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
            builder_factory=lambda *args: update.GitBuilder(*args, remote=str(self.remote), planner=BuildPlanner(self.probe)))
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
                                 remote=str(self.remote), planner=BuildPlanner(self.probe)).prepare(active)

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

    def test_auto_request_directory_alias_still_uses_the_host_idle_gate(self):
        # Windows TEMP 可能是 8.3 短名，监督器 resolve 后为长名；用 .. 在所有平台重现目录别名。
        self.updater.control = self.fixture.queue / '..' / self.fixture.queue.name
        self.supervisor_start()
        self.ticks(lambda: self.updater.status.get('result') in ('applied', 'failed'))
        self.assertEqual(self.updater.status.get('result'), 'applied', json.dumps(self.updater.status))
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])

    def test_stages_recompute_jobs_and_children_receive_actual_limits_in_order(self):
        observed = []
        probe = self.probe
        class ChangingPlanner(BuildPlanner):
            def choose(inner, stage, maximum='auto'):
                probe.available = (12 if stage in ('format', 'libraries', 'clippy') else 6) * GIB
                plan = super().choose(stage, maximum); observed.append(plan)
                return plan
        settings = update.configuration(self.config)
        result = update.GitBuilder(settings,self.fixture.candidates,threading.Event(),lambda **_:None,
            remote=str(self.remote),planner=ChangingPlanner(probe)).prepare(None)
        calls = [json.loads(line) for line in (self.root/'cargo-calls.jsonl').read_text().splitlines()]
        stages = calls[1:]
        self.assertEqual([p['stage'] for p in observed],['format','libraries','clippy','test_compile','tests','binary','binary'])
        self.assertEqual([int(c['jobs']) for c in stages],[3,3,3,2,2,2,2])
        self.assertTrue(all(c['jobs']==c['threads'] for c in stages))
        self.assertIn('--lib', stages[1]['args'])
        self.assertIn('--no-run', stages[3]['args'])
        self.assertNotIn('--no-run', stages[4]['args'])
        self.assertEqual([c['args'][c['args'].index('--bin')+1] for c in stages[5:]],['eve-helper','eve-qqbot'])
        _,_,manifest=runtime.validate_candidate(result['request'],self.fixture.candidates)
        self.assertEqual([len(c['steps']) for c in manifest['checks']],[1,2,2,2])

    def test_memory_pressure_defers_without_marking_revision_failed_then_resumes(self):
        self.probe.available = 128 * 1024**2
        self.supervisor_start()
        self.ticks(lambda:self.updater.status.get('result')=='deferred')
        self.assertEqual(self.updater.status['error'],'auto_update_resources_busy')
        self.assertIsNone(self.updater.status.get('failed_head'))
        self.assertLessEqual(self.updater.next_check,time.monotonic()+60)
        self.assertEqual(list(self.fixture.candidates.iterdir()),[])
        self.assertEqual(self.fixture.events.read_text().splitlines(),['old'])
        calls=[json.loads(line) for line in (self.root/'cargo-calls.jsonl').read_text().splitlines()]
        self.assertEqual([c['args'][0] for c in calls],['metadata'])
        self.probe.available=12*GIB; self.updater.next_check=0
        self.ticks(lambda:self.updater.status.get('result')=='applied')
        self.assertEqual(self.fixture.events.read_text().splitlines(),['old','new'])

    def test_critical_memory_stops_only_owned_build_subprocess(self):
        settings=update.configuration(self.config)
        planner=BuildPlanner(self.probe)
        builder=update.GitBuilder(settings,self.fixture.candidates,threading.Event(),lambda **_:None,planner=planner)
        children=[]; popen=subprocess.Popen
        def capture(*args,**kwargs):
            child=popen(*args,**kwargs);children.append(child);return child
        with patch.object(update.subprocess,'Popen',side_effect=capture),patch.object(planner,'critical',return_value=True):
            with self.assertRaisesRegex(ValueError,'auto_update_resources_busy'):
                builder.command([sys.executable,'-c','import time; time.sleep(30)'],self.root,self.root/'pressure.log',plan=planner.choose('libraries'))
        self.assertIsNotNone(children[0].poll())

    @unittest.skipUnless(os.environ.get('EVE_TEST_REAL_CARGO'), '显式启用小型真实 Rust 工作区验收')
    def test_real_rust_workspace_builds_libraries_tests_and_binaries_in_stages(self):
        cargo=os.environ['EVE_TEST_REAL_CARGO']
        args=['+'+os.environ['EVE_TEST_REAL_TOOLCHAIN']] if os.environ.get('EVE_TEST_REAL_TOOLCHAIN') else []
        (self.remote/'Cargo.toml').write_text('[workspace]\n[package]\nname="eve-app"\nversion="0.0.1"\nedition="2021"\n[[bin]]\nname="eve-qqbot"\npath="src/main.rs"\n[[bin]]\nname="eve-helper"\npath="src/helper.rs"\n')
        (self.remote/'Cargo.lock').write_text('version = 3\n[[package]]\nname = "eve-app"\nversion = "0.0.1"\n')
        src=self.remote/'src';src.mkdir()
        (src/'lib.rs').write_text('pub fn answer() -> u32 { 42 }\n#[cfg(test)] mod tests { #[test] fn answer_matches() { assert_eq!(super::answer(),42); } }\n')
        (src/'main.rs').write_text('fn main() { println!("{}",eve_app::answer()); }\n')
        (src/'helper.rs').write_text('fn main() { println!("helper {}",eve_app::answer()); }\n')
        subprocess.run([cargo,*args,'fmt','--all'],cwd=self.remote,check=True,capture_output=True)
        self.git('add','.');self.git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-m','real Rust workspace')
        runtime.atomic(self.config,dict(self.settings,cargo_program=cargo,cargo_arguments=args))
        prepared=self.prepare()
        binary,_,manifest=runtime.validate_candidate(prepared['request'],self.fixture.candidates)
        self.assertEqual(subprocess.check_output([str(binary)]).decode().strip(),'42')
        self.assertEqual([len(c['steps']) for c in manifest['checks']],[1,2,2,2])
        self.assertIn('1 passed',manifest['checks'][2]['stdout'])

    def test_large_log_tail_is_bounded_and_never_uses_read_bytes(self):
        log=self.root/'large.log'
        with log.open('wb') as stream:
            stream.seek(128*1024*1024);stream.write(b'final-line')
        with patch.object(Path,'read_bytes',side_effect=AssertionError('unbounded read')):
            tail,truncated=update.log_tail(log)
        self.assertTrue(truncated)
        self.assertEqual(len(tail),16384)
        self.assertTrue(tail.endswith('final-line'))

    def test_repeated_prepare_reuses_immutable_candidate_and_renews_only_lease(self):
        first=self.prepare();directory=Path(first['request']['candidate_directory'])
        manifest=(directory/'manifest.json').read_bytes();stamp=(directory/'manifest.json').stat().st_mtime_ns
        second=self.prepare()
        self.assertEqual(first['request']['candidate_sha256'],second['request']['candidate_sha256'])
        self.assertEqual((directory/'manifest.json').read_bytes(),manifest)
        self.assertEqual((directory/'manifest.json').stat().st_mtime_ns,stamp)
        self.assertGreaterEqual(second['request']['lease']['created_at_ms'],first['request']['lease']['created_at_ms'])
        runtime.validate_lease(second['request'],directory,runtime.read_json(directory/'manifest.json'))
        (directory/'resources/AGENT.md').write_text('tampered')
        with self.assertRaisesRegex(ValueError,'resource_changed'):self.prepare()
        self.assertEqual((directory/'resources/AGENT.md').read_text(),'tampered')

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

    def test_reenable_checks_immediately_instead_of_waiting_for_previous_interval(self):
        self.supervisor_start()
        self.ticks(lambda: self.updater.pending is not None)
        runtime.atomic(self.config, dict(self.settings, enabled=False))
        self.ticks(lambda: self.updater.status.get('phase') == 'disabled')
        self.updater.next_check = time.monotonic() + 3600
        runtime.atomic(self.config, dict(self.settings, enabled=True))
        self.ticks(lambda: self.updater.status.get('result') == 'applied')
        self.assertTrue(self.updater.status['enabled'])
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])

    def test_reenable_discards_cancelled_build_result_and_restarts_verification(self):
        self.supervisor_start()
        factory = self.updater.builder_factory
        started = threading.Event()
        class CancelledBuild:
            def __init__(self, settings, candidates, stopped, publish): self.stopped = stopped
            def prepare(self, *args):
                started.set()
                self.stopped.wait(5)
                raise ValueError('auto_update_cancelled')
        self.updater.builder_factory = CancelledBuild
        self.ticks(started.is_set)
        runtime.atomic(self.config, dict(self.settings, enabled=False))
        self.ticks(lambda: self.updater.status.get('phase') == 'disabled')
        self.updater.builder_factory = factory
        runtime.atomic(self.config, dict(self.settings, enabled=True))
        self.ticks(lambda: self.updater.status.get('result') == 'applied')
        self.assertIsNone(self.updater.status.get('failed_head'))
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])

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
