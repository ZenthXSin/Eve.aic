"""Linux 真实进程验收：启动接管、显式停止、旧 boot 元数据与重复启动保护。"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
import socket


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@unittest.skipUnless(sys.platform.startswith('linux'), 'Linux 服务启动入口')
class ServiceTests(unittest.TestCase):
    def setUp(self):
        fixture = load('upgrade_service_fixture', 'test-self-upgrade-runtime.py')
        fixture.UpgradeRuntimeTests.setUpClass()
        self.fixture = fixture.UpgradeRuntimeTests()
        self.fixture.setUp()
        self.service_module = load('eve_service_fixture', 'eve-service.py')
        self.runtime = fixture.runtime
        self.root = self.fixture.root
        self.private = self.root / 'private'
        self.private.mkdir()
        self.guard_dir = self.root / 'guard'
        self.guard_dir.mkdir()
        self.old_environment = dict(os.environ)
        os.environ.update(self.fixture.env)
        with socket.socket() as address:
            address.bind(('127.0.0.1', 0))
            self.port = address.getsockname()[1]
        helper = self.fixture.helper.read_text()
        helper = helper.replace("print('EVE_QQBOT_READY',file=sys.stderr,flush=True)", '''from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import threading
class Probe(BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def do_GET(self):
  data=json.dumps({'channel':{'connected':True},'tools_registered':1}).encode()
  self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
port=int(sys.argv[sys.argv.index('--web-listen')+1].rsplit(':',1)[1])
server=ThreadingHTTPServer(('127.0.0.1',port),Probe)
threading.Thread(target=server.serve_forever,daemon=True).start()
print('EVE_QQBOT_READY',file=sys.stderr,flush=True)''')
        self.fixture.helper.write_text(helper)
        self.request = self.fixture.candidate('old')
        candidate = Path(self.request['candidate_directory'])
        manifest = self.runtime.read_json(candidate / 'manifest.json')
        self.command = [manifest['candidate_path'], *self.fixture.command[1:],
                        '--web-listen', f'127.0.0.1:{self.port}']
        index = self.command.index('--agent')
        self.command[index + 1] = str(candidate / 'resources/AGENT.md')
        self.runtime.atomic(self.fixture.queue / 'active-command.json', {
            'command': self.command, 'binary_sha256': manifest['binary_sha256'], 'candidate_request': self.request})
        self.runtime.atomic(self.private / 'developer-features.json', {
            'workspace': str(self.root), 'storage_directory': str(self.root)})
        self.runtime.atomic(self.private / 'features-environment.json', {'EVE_WEB_TOKEN': 'synthetic-service-token'})
        self.record = self.root / 'service.json'
        scripts = Path(__file__).parent.resolve()
        self.runtime.atomic(self.record, {
            'guard_script': str(scripts / 'service_guard.py'),
            'supervisor_script': str(scripts / 'self_upgrade_runtime.py'),
            'guard_directory': str(self.guard_dir), 'control_directory': str(self.fixture.queue),
            'state_directory': str(self.fixture.state), 'guard_pid': 999999999,
            'supervisor_pid': 999999998, 'guard_identity': ['previous-boot', '1']})
        self.service = self.service_module.Service(self.record, self.private)
        self.service.set_enabled(True)

    def tearDown(self):
        try:
            self.service.stop()
        finally:
            os.environ.clear()
            os.environ.update(self.old_environment)
            self.fixture.tearDown()

    def test_start_and_repeated_ensure_reuse_one_process_and_preserve_state(self):
        before = (self.fixture.state / 'state.json').read_bytes()
        self.assertTrue(self.service.start()['guard_running'])
        first = self.service.data['guard_pid']
        self.assertTrue(self.service.start()['guard_running'])
        self.assertEqual(self.service.data['guard_pid'], first)
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])
        self.assertEqual((self.fixture.state / 'state.json').read_bytes(), before)
        self.assertFalse(self.service.stop()['guard_running'])
        self.assertFalse(self.service.enabled())
        command = [sys.executable, str(Path(__file__).with_name('eve-service.py')),
                   '--service-record', str(self.record), '--private-directory', str(self.private), 'ensure']
        self.assertEqual(subprocess.run(command, capture_output=True).returncode, 0)
        self.assertEqual(self.fixture.events.read_text().splitlines(), ['old'])

    def test_old_boot_metadata_is_archived_without_signalling_reused_pid(self):
        self.runtime.atomic(self.guard_dir / 'guard.json', {
            'phase': 'running', 'identity': ['previous-boot', '1'],
            'child_pid': os.getsid(0), 'child_identity': ['previous-boot', '1']})
        self.assertTrue(self.service.start()['guard_running'])
        self.assertEqual(len(list((self.guard_dir / 'history').glob('*previous-boot.json'))), 1)
        self.assertTrue(Path(f'/proc/{os.getpid()}').exists())

    def test_source_updater_configuration_survives_service_start(self):
        config = self.private / 'auto-update.json'
        self.runtime.atomic(config, {'version': 1, 'enabled': False,
                                    'data_directory': str(self.root / 'source-updates')})
        self.service.data['auto_update_config'] = str(config)
        self.runtime.atomic(self.record, self.service.data)
        self.assertTrue(self.service.start()['qq_connected'])
        status = self.runtime.read_json(self.fixture.queue / 'auto-update.json')
        self.assertFalse(status['enabled'])
        self.assertEqual(self.runtime.read_json(self.record)['auto_update_config'], str(config))

    def test_blocked_recovery_and_corrupt_settings_are_preserved(self):
        self.runtime.atomic(self.guard_dir / 'guard.json', {
            'phase': 'blocked', 'identity': ['previous-boot', '1']})
        with self.assertRaisesRegex(ValueError, 'recovery_block_preserved'):
            self.service.start()
        self.service.settings.write_text('{invalid')
        before = self.service.settings.read_bytes()
        with self.assertRaises(ValueError):
            self.service.enabled()
        self.assertEqual(self.service.settings.read_bytes(), before)
        self.assertFalse(self.fixture.events.exists())

    def test_unrelated_listener_is_not_adopted_or_killed(self):
        self.runtime.atomic(self.record, self.service.data)
        with socket.socket() as other:
            other.bind(('127.0.0.1', self.port))
            other.listen()
            with self.assertRaisesRegex(ValueError, 'service_port_in_use'):
                self.service.start()
        self.assertFalse(self.fixture.events.exists())


if __name__ == '__main__':
    unittest.main()
