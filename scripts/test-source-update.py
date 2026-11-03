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


class BuildStopProbe:
    """只缩短检查子进程的观测间隔，取消状态仍由真实 Event 决定。"""
    def __init__(self, event): self.event = event
    def is_set(self): return self.event.is_set()
    def wait(self, timeout=None): return self.event.wait(min(timeout, .01) if timeout is not None else timeout)


class SourceUpdateTests(unittest.TestCase):
    def setUp(self):
        command = update.GitBuilder.command
        def fast_observation(builder, *args, **kwargs):
            stopped = builder.stopped
            builder.stopped = BuildStopProbe(stopped)
            try:
                return command(builder, *args, **kwargs)
            finally:
                builder.stopped = stopped
        polling = patch.object(update.GitBuilder, 'command', fast_observation)
        polling.start()
        self.addCleanup(polling.stop)
        fixture.UpgradeRuntimeTests.setUpClass()
        self.fixture = fixture.UpgradeRuntimeTests(); self.fixture.setUp()
        self.root = self.fixture.root
        self.original_env = dict(os.environ)
        os.environ.update(self.fixture.env)
        self.remote = self.root / 'upstream'; self.remote.mkdir()
        self.git('init', '-b', 'main')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 'commit', '--allow-empty', '-m', 'trusted deployed base')
        self.base_head = self.git('rev-parse', 'HEAD')
        for name in ['AGENT.md', 'connectors/qqbot/bridge.mjs', 'connectors/qqbot/bridge-core.mjs',
                     'connectors/qqbot/package.json', 'connectors/qqbot/package-lock.json']:
            p = self.remote / name; p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text('new' if name == 'AGENT.md' else '{}')
        scripts = self.remote / 'scripts'; scripts.mkdir()
        for name in update.RUNTIME_SCRIPTS:
            (self.remote/name).write_text('# fixture runtime resource\n')
        (scripts / 'verify-self-upgrade.py').write_text('''import json,os
from pathlib import Path
root=Path.cwd().parents[1]
with (root/'runtime-contract-calls.jsonl').open('a') as stream:
 stream.write(json.dumps({'jobs':os.environ.get('CARGO_BUILD_JOBS'),'threads':os.environ.get('RUST_TEST_THREADS')})+'\\n')
if (root/'fail-runtime-contract').exists():raise SystemExit(9)
print('runtime contracts passed')
''')
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
            builder_factory=self.legacy_builder)
        self.supervisor = None

    def legacy_builder(self, *args):
        # 旧格式的真实二进制切换回归仍保留；新格式单独走宿主桥完整验收。
        candidates = self.fixture.candidates
        class LegacyBuilder(update.GitBuilder):
            def freeze(builder, *arguments):
                result = super().freeze(*arguments)
                request = result['request']
                directory = Path(request['candidate_directory'])
                manifest = runtime.read_json(directory/'manifest.json')
                manifest.pop('runtime')
                for name in update.RUNTIME_SCRIPTS:
                    manifest['resources'].pop(name)
                key = runtime.value_digest({name:manifest[name] for name in
                    ['source', 'binary_sha256', 'resources', 'source_diff']})
                target = candidates/key
                if not target.exists():
                    shutil.copytree(directory, target)
                    manifest['candidate_sha256'] = key
                    manifest['candidate_path'] = str(target/Path(manifest['candidate_path']).name)
                    runtime.atomic(target/'manifest.json', manifest)
                request = dict(request, candidate_sha256=key, candidate_directory=str(target))
                request.pop('lease')
                _, _, frozen = runtime.validate_candidate(request, candidates)
                request['lease'] = runtime.prepare_lease(target, frozen)
                return dict(result, request=request)
        return LegacyBuilder(*args, remote=str(self.remote), planner=BuildPlanner(self.probe))

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
                                 remote=str(self.remote), planner=BuildPlanner(self.probe)).prepare(active or self.base_head)

    def supervisor_start(self, managed=False):
        options = SimpleNamespace(directory=self.fixture.queue, candidates=self.fixture.candidates,
            command=self.fixture.command, ready_timeout=3, stop_timeout=3, quiet_seconds=.05,
            health_seconds=.05, source_revision=self.base_head)
        if managed:
            launch = self.fixture.queue/'fixture-launch.json'
            runtime.atomic(launch, dict(format_version=1, command=self.fixture.command,
                                       candidate_request=None, source_revision=self.base_head))
            options.host_bridge = self.fixture.queue
            options.host_nonce = 'a'*64
            options.launch_descriptor = launch
        self.supervisor = runtime.Supervisor(options)
        self.supervisor.child = runtime.Child(self.fixture.command)
        self.supervisor.wait_ready(self.supervisor.child)
        return self.supervisor

    def ticks(self, predicate, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            # 测试推进每个真实状态转换；生产轮询节流不属于这些流程断言。
            self.updater.last_tick = 0
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

    def test_global_autocrlf_cannot_change_verified_candidate_resource_bytes(self):
        config = self.root / 'global-git-config'
        config.write_text('[core]\n\tautocrlf = true\n\teol = crlf\n')
        with patch.dict(os.environ, {'GIT_CONFIG_GLOBAL': str(config)}):
            prepared = self.prepare()
        _, resources, manifest = runtime.validate_candidate(prepared['request'], self.fixture.candidates)
        for path in manifest['resources']:
            expected = subprocess.run(['git', 'show', self.head + ':' + path], cwd=self.remote,
                                      check=True, capture_output=True).stdout
            self.assertEqual((resources / path).read_bytes(), expected)

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
            remote=str(self.remote),planner=ChangingPlanner(probe)).prepare(self.base_head)
        calls = [json.loads(line) for line in (self.root/'cargo-calls.jsonl').read_text().splitlines()]
        stages = calls[1:]
        self.assertEqual([p['stage'] for p in observed],['format','libraries','clippy','test_compile','tests','runtime_contracts','binary','binary'])
        self.assertEqual([int(c['jobs']) for c in stages],[3,3,3,2,2,2,2])
        self.assertTrue(all(c['jobs']==c['threads'] for c in stages))
        self.assertIn('--lib', stages[1]['args'])
        self.assertIn('--no-run', stages[3]['args'])
        self.assertNotIn('--no-run', stages[4]['args'])
        self.assertEqual([c['args'][c['args'].index('--bin')+1] for c in stages[5:]],['eve-helper','eve-qqbot'])
        _,_,manifest=runtime.validate_candidate(result['request'],self.fixture.candidates)
        self.assertEqual([len(c['steps']) for c in manifest['checks']],[1,2,3,2])
        self.assertIn('runtime contracts passed', manifest['checks'][2]['stdout'])
        contracts = [json.loads(line) for line in (self.root/'runtime-contract-calls.jsonl').read_text().splitlines()]
        self.assertEqual(len(contracts), 1)
        self.assertEqual(contracts[0]['jobs'], contracts[0]['threads'])

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
        with patch.object(update.time, 'time', return_value=time.time()+61):
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
        self.assertEqual([len(c['steps']) for c in manifest['checks']],[1,2,3,2])
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

    def test_source_diff_and_runtime_scripts_are_complete_immutable_candidate_inputs(self):
        prepared = self.prepare()
        request = prepared['request']
        directory = Path(request['candidate_directory'])
        _, resources, manifest = runtime.validate_candidate(request, self.fixture.candidates)
        expected = subprocess.check_output(['git', '--no-pager', 'diff', '--no-ext-diff', '--no-textconv',
                                            '--binary', self.base_head, '--'], cwd=self.remote)
        self.assertEqual((directory/'source-diff.patch').read_bytes(), expected)
        self.assertEqual(manifest['source_diff'], dict(path='source-diff.patch',
                         sha256=hashlib.sha256(expected).hexdigest(),
                         base_revision=self.base_head, target_revision=self.head))
        self.assertEqual(manifest['runtime'], dict(host_protocol=1, scripts=list(update.RUNTIME_SCRIPTS)))
        self.assertEqual({name for name in manifest['resources'] if name.startswith('scripts/')},
                         set(update.RUNTIME_SCRIPTS))
        for name in update.RUNTIME_SCRIPTS:
            self.assertEqual((resources/name).read_bytes(), (self.remote/name).read_bytes())
        identity = {name:manifest[name] for name in ['source', 'binary_sha256', 'resources', 'source_diff', 'runtime']}
        self.assertEqual(runtime.value_digest(identity), request['candidate_sha256'])
        (directory/'source-diff.patch').write_bytes(expected + b'changed')
        with self.assertRaisesRegex(ValueError, 'source_diff_changed'):
            runtime.validate_candidate(request, self.fixture.candidates)

    def test_source_diff_above_limit_is_rejected_before_build_without_truncation(self):
        (self.remote/'large.txt').write_text('full-line\n' * 20000)
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'large patch')
        head = self.git('rev-parse', 'HEAD')
        with self.assertRaisesRegex(ValueError, 'auto_update_source_diff_too_large'):
            self.prepare()
        artifact = self.root/'update/logs'/f'{head}-source-diff.patch'
        self.assertGreater(artifact.stat().st_size, update.MAX_SOURCE_DIFF_BYTES)
        expected = subprocess.check_output(['git', '--no-pager', 'diff', '--no-ext-diff', '--no-textconv',
                                            '--binary', self.base_head, '--'], cwd=self.remote)
        self.assertEqual(artifact.read_bytes(), expected)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])

    def test_binary_and_submodule_source_changes_are_not_reviewable_candidates(self):
        (self.remote/'binary.dat').write_bytes(bytes(range(256)))
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'binary')
        with self.assertRaisesRegex(ValueError, 'auto_update_source_diff_not_reviewable'):
            self.prepare()
        self.git('rm', 'binary.dat')
        self.git('update-index', '--add', '--cacheinfo', '160000,' + self.base_head + ',submodule')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'submodule')
        with self.assertRaisesRegex(ValueError, 'auto_update_source_diff_not_reviewable'):
            self.prepare()
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])

    def test_hidden_index_flags_cannot_skip_source_verification(self):
        self.prepare()
        source = self.root/'update/source'
        calls = (self.root/'cargo-calls.jsonl').read_bytes()
        for flag, clear in [('--skip-worktree', '--no-skip-worktree'),
                            ('--assume-unchanged', '--no-assume-unchanged')]:
            subprocess.run(['git', 'update-index', flag, 'AGENT.md'], cwd=source, check=True)
            (source/'AGENT.md').write_text('hidden local change')
            with self.assertRaisesRegex(ValueError, 'auto_update_hidden_index_state'):
                self.prepare()
            self.assertEqual((self.root/'cargo-calls.jsonl').read_bytes(), calls)
            (source/'AGENT.md').write_text('new')
            subprocess.run(['git', 'update-index', clear, 'AGENT.md'], cwd=source, check=True)

    def test_submodule_ignore_configuration_cannot_hide_source_diff(self):
        settings = update.configuration(self.config)
        builder = update.GitBuilder(settings, self.fixture.candidates, threading.Event(), lambda **_:None,
                                    remote=str(self.remote), planner=BuildPlanner(self.probe))
        self.git('update-index', '--add', '--cacheinfo', '160000,' + self.base_head + ',submodule')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'submodule')
        original = builder.git
        def hide_submodule(args, cwd, log):
            result = original(args, cwd, log)
            if args[:1] == ['checkout']:
                original(['config', 'diff.ignoreSubmodules', 'all'], cwd, log)
                original(['config', 'submodule.submodule.ignore', 'all'], cwd, log)
            return result
        with patch.object(builder, 'git', side_effect=hide_submodule):
            with self.assertRaisesRegex(ValueError, 'auto_update_source_diff_not_reviewable'):
                builder.prepare(self.base_head)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())

    def test_missing_trusted_base_cannot_create_new_candidate(self):
        settings = update.configuration(self.config)
        builder = update.GitBuilder(settings, self.fixture.candidates, threading.Event(), lambda **_:None,
                                    remote=str(self.remote), planner=BuildPlanner(self.probe))
        for base in (None, '', 'HEAD', 'a'*39, 'a'*41):
            with self.assertRaisesRegex(ValueError, 'auto_update_trusted_base_missing'):
                builder.prepare(base)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])

    def test_missing_runtime_script_cannot_freeze_candidate(self):
        (self.remote/'scripts/source_update.py').unlink()
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'missing updater')
        with self.assertRaisesRegex(ValueError, 'auto_update_runtime_scripts_missing'):
            self.prepare()
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])

    def test_resource_or_binary_changed_after_validation_cannot_freeze(self):
        settings = update.configuration(self.config)
        for resource in (True, False):
            builder = update.GitBuilder(settings, self.fixture.candidates, threading.Event(), lambda **_:None,
                                        remote=str(self.remote), planner=BuildPlanner(self.probe))
            freeze = builder.freeze
            def late_change(source, head, binary, *rest):
                target = source/'scripts/self_upgrade_runtime.py' if resource else binary
                with target.open('ab') as stream:
                    stream.write(b'\n# late modification\n')
                return freeze(source, head, binary, *rest)
            with patch.object(builder, 'freeze', side_effect=late_change):
                with self.assertRaisesRegex(ValueError, 'auto_update_(source|binary)_changed_during_freeze'):
                    builder.prepare(self.base_head)
            self.assertEqual(list(self.fixture.candidates.iterdir()), [])
            if resource:
                (self.root/'update/source/scripts/self_upgrade_runtime.py').write_bytes(
                    (self.remote/'scripts/self_upgrade_runtime.py').read_bytes())

    def test_new_runtime_candidate_hands_off_without_claiming_applied_or_failed(self):
        self.updater.builder_factory = lambda *args:update.GitBuilder(*args, remote=str(self.remote),
                                                                       planner=BuildPlanner(self.probe))
        supervisor = self.supervisor_start(managed=True)
        self.ticks(lambda:self.updater.status.get('result') == 'handed_off')
        status = runtime.read_json(self.fixture.queue/'auto-update.json')
        handoff = runtime.read_json(self.fixture.queue/'handoff-request.json')
        self.assertEqual(status['phase'], 'handed_off')
        self.assertEqual(status['current_head'], self.base_head)
        self.assertEqual(status['handed_off_head'], self.head)
        self.assertIsNone(status.get('failed_head'))
        self.assertIsNone(status['applied_head'])
        self.assertEqual(handoff['nonce'], 'a'*64)
        self.assertEqual(handoff['request'], runtime.read_json(self.fixture.queue/'processing.json'))
        self.assertIsNone(handoff['base_candidate_sha256'])
        self.assertTrue(supervisor.stopping)
        self.assertIsNotNone(supervisor.child.process.poll())
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])

    def test_new_runtime_candidate_requires_host_bridge_before_old_process_stops(self):
        self.updater.builder_factory = lambda *args:update.GitBuilder(*args, remote=str(self.remote),
                                                                       planner=BuildPlanner(self.probe))
        supervisor = self.supervisor_start()
        self.ticks(lambda:self.updater.status.get('error') == 'runtime_handoff_requires_host_bridge')
        self.assertIsNone(supervisor.child.process.poll())
        self.assertFalse((self.fixture.queue/'processing.json').exists())
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])

    def test_invalid_host_receipt_keeps_handoff_blocked_and_does_not_start_another_build(self):
        supervisor = self.supervisor_start()
        self.updater.publish(phase='handed_off', result='handed_off', handed_off_head=self.head,
                             handed_off_candidate_sha256='b'*64)
        runtime.atomic(self.fixture.queue/'host-handoff.json', dict(format_version=1, phase='committed',
            previous={}, next={'descriptor': {'candidate_request': {'candidate_sha256': 'c'*64}}},
            error=None, updated_at_ms=1))
        self.updater.tick(supervisor)
        self.assertEqual(self.updater.status['result'], 'handed_off')
        self.assertEqual(self.updater.status['phase'], 'blocked')
        self.assertEqual(self.updater.status['error'], 'host_handoff_receipt_mismatch')
        self.updater.last_tick = 0
        self.updater.tick(supervisor)
        self.assertIsNone(self.updater.thread)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())

    @unittest.skipUnless(sys.platform.startswith('linux'), '固定宿主桥使用 Linux 进程归属')
    def test_real_source_update_handoff_restarts_frozen_runtime_and_reports_host_commit(self):
        self.run_real_source_handoff(False)

    @unittest.skipUnless(sys.platform.startswith('linux'), '固定宿主桥使用 Linux 进程归属')
    def test_real_source_update_failed_runtime_recovers_old_source_and_host_receipt(self):
        self.run_real_source_handoff(True)

    def run_real_source_handoff(self, fail):
        # 子进程使用本文件所在真实 runtime 三模块；只有受信任的初始副本注入本地 Git 地址。
        # 新候选不含注入。交接前关闭测试自动检查，避免新实例访问外网。
        for name in update.RUNTIME_SCRIPTS:
            (self.remote/name).write_bytes((Path(__file__).parent.parent/name).read_bytes())
        if fail:
            (self.remote/'AGENT.md').write_text('fail')
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'actual runtime bundle')
        self.head = self.git('rev-parse', 'HEAD')
        trusted = self.root/'trusted-runtime'; trusted.mkdir()
        for name in update.RUNTIME_SCRIPTS:
            shutil.copy2(self.remote/name, trusted/Path(name).name)
        wrapper = trusted/'self_upgrade_runtime.py'
        injection = '''
if __name__ == '__main__':
    import source_update
    import build_resources
    _git_builder_init = source_update.GitBuilder.__init__
    class _FixtureResources:
        def snapshot(self):
            return build_resources.Resources(4, 16 * build_resources.GIB, 12 * build_resources.GIB, 0)
    def _local_git(self, *args, **kwargs):
        kwargs.update(remote=__REMOTE__, planner=build_resources.BuildPlanner(_FixtureResources()))
        _git_builder_init(self, *args, **kwargs)
    source_update.GitBuilder.__init__ = _local_git
    _switch = Supervisor.switch
    def _closing_switch(self, *args, **kwargs):
        config = Path(__CONFIG__)
        settings = read_json(config)
        atomic(config, dict(settings, enabled=False))
        return _switch(self, *args, **kwargs)
    Supervisor.switch = _closing_switch
'''.replace('__REMOTE__', repr(str(self.remote))).replace('__CONFIG__', repr(str(self.config)))
        original = wrapper.read_text()
        wrapper.write_text(original.replace("if __name__ == '__main__':", injection + "\nif __name__ == '__main__':", 1))
        bridge = Path(__file__).with_name('host_runtime_bridge.py').resolve()
        self.fixture.log = (self.root/'host-combination.log').open('wb')
        self.fixture.process = subprocess.Popen([sys.executable, '-B', str(bridge),
            '--supervisor', str(wrapper), '--directory', str(self.fixture.queue),
            '--candidates', str(self.fixture.candidates), '--auto-update-config', str(self.config),
            '--source-revision', self.base_head, '--ready-timeout', '3', '--stop-timeout', '3',
            '--health-seconds', '.1', '--quiet-seconds', '.05', '--', *self.fixture.command],
            env=dict(self.fixture.env, PYTHONDONTWRITEBYTECODE='1'),
            stdout=self.fixture.log, stderr=subprocess.STDOUT)
        deadline = time.monotonic()+30
        status = {}
        while time.monotonic() < deadline:
            if self.fixture.process.poll() is not None:
                self.fail('host bridge exited: ' + (self.root/'host-combination.log').read_text(errors='replace')[-4000:])
            path = self.fixture.queue/'auto-update.json'
            if path.exists():
                status = runtime.read_json(path)
                if status.get('result') == ('rolled_back_runtime_state_preserved' if fail else 'applied'):
                    break
            time.sleep(.05)
        self.assertEqual(status.get('result'), 'rolled_back_runtime_state_preserved' if fail else 'applied', repr(status))
        self.assertEqual(status['current_head'], self.base_head if fail else self.head)
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'fail', 'old'] if fail else ['old', 'new'])
        receipt = runtime.read_json(self.fixture.queue/'host-handoff.json')
        self.assertEqual(receipt['phase'], 'rolled_back' if fail else 'committed')
        active = runtime.read_json(self.fixture.queue/'host-active.json')
        if fail:
            self.assertEqual(receipt['previous'], active)
            self.assertIsNone(active['descriptor']['candidate_request'])
            self.assertEqual(status['failed_head'], self.head)
            self.assertIn('synthetic.candidate', runtime.read_json(self.fixture.state/'state.json')['entries'])
            self.assertTrue((self.fixture.queue/'quarantine'/(status['handed_off_candidate_sha256'] + '.json')).exists())
            return
        request = active['descriptor']['candidate_request']
        self.assertEqual(request['candidate_sha256'], status['handed_off_candidate_sha256'])
        self.assertEqual(receipt['next'], active)
        _, resources, manifest = runtime.validate_candidate(request, self.fixture.candidates)
        self.assertEqual(set(active['scripts']), {str(resources/name) for name in update.RUNTIME_SCRIPTS})
        for name in update.RUNTIME_SCRIPTS:
            self.assertEqual(active['scripts'][str(resources/name)], manifest['resources'][name])

    def test_check_failure_does_not_publish_candidate_or_touch_old_instance(self):
        (self.root / 'fail-check').touch()
        self.supervisor_start()
        self.ticks(lambda: self.updater.status.get('result') == 'failed')
        self.assertEqual(self.updater.status['error'], 'auto_update_check_failed')
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])
        self.assertFalse((self.fixture.queue / 'request.json').exists())

    def test_runtime_contract_failure_prevents_candidate_freezing(self):
        (self.root / 'fail-runtime-contract').touch()
        with self.assertRaisesRegex(ValueError, 'auto_update_check_failed'):
            self.prepare()
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])
        calls = [json.loads(line) for line in (self.root/'cargo-calls.jsonl').read_text().splitlines()]
        self.assertEqual(calls[-1]['args'][0], 'test')
        self.assertFalse(any('--bin' in call['args'] for call in calls))
        self.assertEqual(len((self.root/'runtime-contract-calls.jsonl').read_text().splitlines()), 1)

    def test_transient_timeout_retry_survives_restart_and_respects_backoff(self):
        self.supervisor_start()
        factory = self.updater.builder_factory
        attempts = []
        head = self.head
        class TimeoutBuild:
            def __init__(self, settings, candidates, stopped, publish): self.publish = publish
            def prepare(self, *args):
                self.publish(phase='verifying', target_head=head)
                attempts.append(head)
                raise ValueError('auto_update_command_timeout')
        self.updater.builder_factory = TimeoutBuild
        self.ticks(lambda:self.updater.status.get('result') == 'deferred')
        first = runtime.read_json(self.fixture.queue/'auto-update.json')['retry']
        self.assertEqual(first['attempts'], 1)
        self.assertEqual(first['head'], head)
        self.assertIsNone(self.updater.status.get('failed_head'))
        self.updater.stop()
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                            builder_factory=TimeoutBuild)
        self.assertEqual(self.updater.status['retry'], first)
        self.updater.tick(self.supervisor)
        self.assertIsNone(self.updater.thread)
        self.assertEqual(attempts, [head])
        with patch.object(update.time, 'time', return_value=first['next_attempt_at_ms']/1000 + 1):
            self.ticks(lambda:self.updater.status.get('retry', {}).get('attempts') == 2
                       and self.updater.status.get('result') == 'deferred')
        second = runtime.read_json(self.fixture.queue/'auto-update.json')['retry']
        self.assertEqual(attempts, [head, head])
        self.assertGreaterEqual(second['next_attempt_at_ms'] - first['next_attempt_at_ms'], 120000)
        self.updater.stop()
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                            builder_factory=factory)
        with patch.object(update.time, 'time', return_value=second['next_attempt_at_ms']/1000 + 1):
            self.ticks(lambda:self.updater.status.get('result') == 'applied')
        self.assertIsNone(self.updater.status['retry'])
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])

    def test_retry_exhaustion_blocks_same_revision_after_restart_and_new_revision_can_apply(self):
        self.supervisor_start()
        factory = self.updater.builder_factory
        attempts = []
        head = self.head
        class TimeoutBuild:
            def __init__(self, settings, candidates, stopped, publish): self.publish = publish
            def prepare(self, *args):
                self.publish(phase='verifying', target_head=head)
                attempts.append(head)
                raise ValueError('auto_update_command_timeout')
        self.updater.builder_factory = TimeoutBuild
        for number in range(1, update.RETRY_LIMIT + 1):
            now = max(time.time(), (self.updater.status.get('retry') or {}).get('next_attempt_at_ms', 0)/1000 + 1)
            self.updater.next_check = 0
            self.updater.last_tick = 0
            with patch.object(update.time, 'time', return_value=now):
                self.ticks(lambda:self.updater.status.get('retry', {}).get('attempts') == number
                           and self.updater.status.get('result') == ('retry_exhausted' if number == update.RETRY_LIMIT else 'deferred'))
                self.ticks(lambda:self.updater.thread is None)
        self.assertEqual(len(attempts), update.RETRY_LIMIT)
        retry = dict(self.updater.status['retry'])
        self.updater.stop()
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                            builder_factory=factory)
        with patch.object(update.time, 'time', return_value=retry['next_attempt_at_ms']/1000 + 1):
            self.ticks(lambda:self.updater.status.get('result') == 'previous_failure')
        self.assertEqual(self.updater.status['retry'], retry)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())
        (self.remote/'AGENT.md').write_text('new\n')
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'recoverable revision')
        self.updater.next_check = 0
        with patch.object(update.time, 'time', return_value=retry['next_attempt_at_ms']/1000 + 1):
            self.ticks(lambda:self.updater.status.get('result') == 'applied')
        self.assertIsNone(self.updater.status['retry'])

    def test_deterministic_check_failure_is_not_rebuilt_after_restart(self):
        (self.root/'fail-check').touch()
        self.supervisor_start()
        self.ticks(lambda:self.updater.status.get('result') == 'failed')
        self.assertEqual(self.updater.status['failed_head'], self.head)
        calls = (self.root/'cargo-calls.jsonl').read_bytes()
        self.updater.stop()
        factory = self.updater.builder_factory
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                            builder_factory=factory)
        self.ticks(lambda:self.updater.status.get('result') == 'previous_failure')
        self.assertEqual((self.root/'cargo-calls.jsonl').read_bytes(), calls)

    def test_corrupt_persistent_retry_fails_closed_without_overwriting_status(self):
        path = self.fixture.queue/'auto-update.json'
        for invalid in [{'attempts': 1}, dict(configuration='a'*64, head=self.head,
                        attempts=update.RETRY_LIMIT + 1, next_attempt_at_ms=0, error=None)]:
            runtime.atomic(path, {'format_version': 1, 'retry': invalid})
            before = path.read_bytes()
            with self.assertRaisesRegex(ValueError, 'invalid_auto_update_retry_state'):
                update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates)
            self.assertEqual(path.read_bytes(), before)

    def test_interrupted_last_attempt_does_not_rebuild_or_clear_budget_on_restart(self):
        self.supervisor_start()
        signature = hashlib.sha256(json.dumps(self.updater.settings, sort_keys=True).encode()).hexdigest()
        retry = dict(configuration=signature, head=self.head, attempts=update.RETRY_LIMIT,
                     next_attempt_at_ms=0, error=None)
        self.updater.publish(phase='verifying', retry=retry, target_head=self.head)
        factory = self.updater.builder_factory
        self.updater.stop()
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                            builder_factory=factory)
        self.ticks(lambda:self.updater.status.get('result') == 'previous_failure')
        self.assertEqual(self.updater.status['retry'], retry)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())
        self.updater.next_check = 0
        self.ticks(lambda:self.updater.thread is not None)
        self.ticks(lambda:self.updater.thread is None)
        self.assertEqual(self.updater.status['retry'], retry)
        self.assertFalse((self.root/'cargo-calls.jsonl').exists())

    def test_unknown_revision_timeout_has_finite_persistent_retry_budget(self):
        self.supervisor_start()
        attempts = []
        class TimeoutBuild:
            def __init__(self, *args): pass
            def prepare(self, *args):
                attempts.append(1)
                raise ValueError('auto_update_fetch_failed')
        self.updater.builder_factory = TimeoutBuild
        for number in range(1, update.RETRY_LIMIT + 1):
            now = max(time.time(), (self.updater.status.get('retry') or {}).get('next_attempt_at_ms', 0)/1000 + 1)
            self.updater.next_check = 0
            self.updater.last_tick = 0
            with patch.object(update.time, 'time', return_value=now):
                self.ticks(lambda:self.updater.status.get('retry', {}).get('attempts') == number
                           and self.updater.status.get('result') == ('retry_exhausted' if number == update.RETRY_LIMIT else 'deferred'))
                self.ticks(lambda:self.updater.thread is None)
        self.updater.stop()
        self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                            builder_factory=TimeoutBuild)
        self.updater.tick(self.supervisor)
        self.assertIsNone(self.updater.thread)
        self.assertEqual(len(attempts), update.RETRY_LIMIT)
        runtime.atomic(self.fixture.queue/'check-updates.json', {'format_version': 1, 'nonce': 'explicit-retry'})
        self.updater.last_tick = 0
        self.ticks(lambda:len(attempts) == update.RETRY_LIMIT + 1)
        self.assertEqual(self.updater.status['retry']['attempts'], 1)

    def test_invalid_configuration_during_build_preserves_budget_and_does_not_blame_source(self):
        self.supervisor_start()
        started, release = threading.Event(), threading.Event()
        head = self.head
        class DelayedBuild:
            def __init__(self, settings, candidates, stopped, publish): self.publish = publish
            def prepare(self, *args):
                self.publish(phase='verifying', target_head=head)
                started.set()
                release.wait(5)
                self.publish(phase='verifying', target_head=head, stage='tests')
                raise ValueError('auto_update_command_timeout')
        self.updater.builder_factory = DelayedBuild
        try:
            self.ticks(started.is_set)
            retry = dict(self.updater.status['retry'])
            runtime.atomic(self.config, dict(self.settings, jobs=0))
            self.ticks(lambda:self.updater.status.get('error') == 'invalid_auto_update_configuration')
            self.assertEqual(self.updater.status['retry'], retry)
            self.assertIsNone(self.updater.status.get('failed_head'))
            runtime.atomic(self.config, self.settings)
            release.set()
            self.ticks(lambda:self.updater.status.get('error') == 'auto_update_command_timeout')
            self.assertEqual(self.updater.status['result'], 'deferred')
            self.assertEqual(self.updater.status['retry']['attempts'], 1)
            self.assertEqual(self.updater.status['retry']['head'], head)
            self.assertIsNone(self.updater.status.get('failed_head'))
        finally:
            release.set()

    def test_changed_build_configuration_retries_same_revision_after_persisted_failure(self):
        self.supervisor_start()
        factory = self.updater.builder_factory
        started, release = threading.Event(), threading.Event()
        head = self.head
        class DelayedFailure:
            def __init__(self, settings, candidates, stopped, publish): self.publish = publish
            def prepare(self, *args):
                self.publish(phase='verifying', target_head=head)
                started.set()
                release.wait(5)
                raise ValueError('auto_update_check_failed')
        self.updater.builder_factory = DelayedFailure
        try:
            self.ticks(started.is_set)
            runtime.atomic(self.config, dict(self.settings, jobs=1))
            release.set()
            self.ticks(lambda:self.updater.status.get('failed_head') == head)
            self.updater.stop()
            self.updater = update.SourceUpdater(self.config, self.fixture.queue, self.fixture.candidates,
                                                builder_factory=factory)
            self.ticks(lambda:self.updater.status.get('result') == 'applied')
            self.assertIsNone(self.updater.status['failed_head'])
            self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])
        finally:
            release.set()

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

    def test_ancestry_probe_timeout_remains_transient(self):
        settings = update.configuration(self.config)
        builder = update.GitBuilder(settings, self.fixture.candidates, threading.Event(), lambda **_:None,
                                    remote=str(self.remote), planner=BuildPlanner(self.probe))
        original = builder.git
        def timed_out(args, *rest):
            if args[:1] == ['merge-base']:
                raise ValueError('auto_update_command_timeout')
            return original(args, *rest)
        (self.remote/'AGENT.md').write_text('new\n')
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'next revision')
        with patch.object(builder, 'git', side_effect=timed_out):
            with self.assertRaisesRegex(ValueError, 'auto_update_command_timeout'):
                builder.prepare(self.head)
        self.assertEqual(list(self.fixture.candidates.iterdir()), [])

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
        retry = dict(self.updater.status['retry'])
        self.assertEqual(retry['error'], 'auto_update_insufficient_space_for_backup')
        self.assertEqual(retry['head'], self.head)
        self.assertIsNone(self.updater.status.get('failed_head'))
        self.updater.next_check = 0
        with patch.object(update.time, 'time', return_value=retry['next_attempt_at_ms']/1000 + 1):
            self.ticks(lambda:self.updater.status.get('result') == 'applied')
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old', 'new'])

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
        task = {'format_version': 2, 'tasks': [{'id': 'task-1', 'status': 'Running'}]}
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
