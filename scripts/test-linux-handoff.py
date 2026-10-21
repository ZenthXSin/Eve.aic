"""不连接 QQ 的迁移脚本回归：分卷、覆盖保护、路径和进程身份。"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch


def module(name):
    path = Path(__file__).parent / 'linux-handoff' / f'{name}.py'
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


restore = module('restore')
eve = module('eve')


class HandoffTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith('linux') and __import__('platform').machine() == 'x86_64', 'Linux ELF loader')
    def test_private_node_loader_preserves_fork_in_a_path_with_spaces(self):
        node = shutil.which('node')
        loader = next((p for p in [Path('/lib64/ld-linux-x86-64.so.2'),
                                  Path('/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2')] if p.exists()), None)
        if not node or not loader:
            self.skipTest('Node/ELF loader unavailable')
        with tempfile.TemporaryDirectory(prefix='Eve node ') as directory:
            root = Path(directory)
            for name in ('bin', 'node/bin', 'lib'):
                (root / 'runtime' / name).mkdir(parents=True)
            source = Path(__file__).parent / 'linux-handoff'
            wrapper = root / 'runtime/bin/node'
            shutil.copy2(source / 'node.sh', wrapper)
            wrapper.chmod(0o700)
            shutil.copy2(source / 'node-path.cjs', root / 'runtime/node-path.cjs')
            (root / 'runtime/node/bin/node').symlink_to(Path(node).resolve())
            (root / 'runtime/lib/ld-linux-x86-64.so.2').symlink_to(loader.resolve())
            child = root / 'child.cjs'
            child.write_text('process.send(process.execPath);')
            env = {k: v for k, v in os.environ.items() if k.lower() != 'https_proxy'}
            code = 'require("node:child_process").fork(process.argv[1]).on("message", value => console.log(value));'
            result = subprocess.run([str(wrapper), '-e', code, str(child)], env=env,
                                    capture_output=True, text=True, timeout=15, check=True)
            self.assertEqual(result.stdout.strip(), str(wrapper))

    def test_missing_and_corrupt_parts_are_rejected_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            part = {'name': 'part-000', 'size': 3, 'sha256': hashlib.sha256(b'abc').hexdigest()}
            bundle = [{'parts': [part]}]
            with self.assertRaisesRegex(RuntimeError, '缺失'):
                restore.verify(root, bundle)
            (root / part['name']).write_bytes(b'bad')
            with self.assertRaisesRegex(RuntimeError, 'SHA-256'):
                restore.verify(root, bundle)
            (root / part['name']).write_bytes(b'abc')
            with contextlib.redirect_stdout(io.StringIO()):
                restore.verify(root, bundle)

    def test_path_mapping_preserves_the_installation_path_with_spaces(self):
        with patch.object(eve, 'ROOT', Path('/tmp/Eve local')):
            value = eve.mapped({'workspace': '/workspace/Eve.aic', 'operator': 'qq:owner',
                                'args': ['--reference', 'src=/workspace/source']})
            self.assertEqual(value['workspace'], '/tmp/Eve local/payload/workspace/Eve.aic')
            self.assertEqual(value['operator'], 'qq:owner')
            self.assertEqual(value['args'][-1], 'src=/tmp/Eve local/payload/workspace/source')

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux hosted installation')
    def test_hosted_prepare_keeps_data_and_uses_separate_task_workspace(self):
        with tempfile.TemporaryDirectory(prefix='Eve local ') as directory:
            root = Path(directory)
            private = root / 'payload/workspace/eve-migration/private'
            private.mkdir(parents=True)
            spec = {'profile': 'hosted', 'binary': 'app/eve-qqbot',
                    'resources': {'--state-dir': 'data/state'}, 'references': {'mindustry': 'app/reference'}}
            (root / 'handoff.json').write_text(json.dumps(spec))
            (private / 'developer-features.json').write_text(json.dumps({
                'workspace': '/workspace/source', 'storage_directory': '/workspace/old-ledger',
                'self_upgrade': True, 'github': True, 'github_write_repositories': ['owner/repo'],
                'live_upgrade': '/workspace/control', 'cargo_arguments': ['+stable']}))
            (private / 'features-environment.json').write_text(json.dumps({
                'EVE_GITHUB_TOKEN': 'test-only', 'CARGO_HOME': '/workspace/old-cargo'}))
            (private / 'features-command.json').write_text(json.dumps([
                'old-binary', '--state-dir', '/workspace/old-state', '--reference', 'mindustry=/workspace/ref']))
            state = root / 'data/state/state.json'
            state.parent.mkdir(parents=True)
            state.write_bytes(b'original-evidence')
            with patch.object(eve, 'ROOT', root), patch.object(eve, 'RUN', root / 'run'), \
                    patch.object(eve.platform, 'machine', return_value='x86_64'), \
                    patch.object(eve.os, 'getuid', return_value=1000), \
                    patch.dict(os.environ, {'GH_TOKEN': 'host-only', 'CARGO_HOME': '/tmp/local-cargo'}):
                _, config, command, env = eve.prepare()
            self.assertEqual(config['workspace'], str(root / 'data/workspace'))
            self.assertTrue(Path(config['workspace']).is_dir())
            self.assertFalse(config['self_upgrade'])
            self.assertFalse(config['github'])
            self.assertNotIn('live_upgrade', config)
            self.assertNotIn('EVE_GITHUB_TOKEN', env)
            self.assertNotIn('GH_TOKEN', env)
            self.assertEqual(env['CARGO_HOME'], '/tmp/local-cargo')
            self.assertEqual(command[2], str(state.parent))
            self.assertEqual(command[-1], 'mindustry=' + str(root / 'app/reference'))
            self.assertEqual(state.read_bytes(), b'original-evidence')

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux process identity')
    def test_pid_reuse_or_stale_record_cannot_authorize_a_stop_signal(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(eve, 'RUN', Path(directory)):
            current = eve.identity(os.getpid())
            self.assertIsNotNone(current)
            eve.save(Path(directory) / 'process.json', {'pid': os.getpid(), 'identity': ['old-boot', current[1]]})
            self.assertIsNone(eve.running())
            eve.save(Path(directory) / 'process.json', {'pid': os.getpid(), 'identity': current})
            self.assertEqual(eve.running()['pid'], os.getpid())

    def test_split_compressed_archive_can_be_streamed_without_a_second_archive_copy(self):
        if not __import__('shutil').which('zstd'):
            self.skipTest('zstd unavailable')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = io.BytesIO()
            with tarfile.open(fileobj=raw, mode='w') as archive:
                item = tarfile.TarInfo('payload/原状态.txt')
                data = '证据保持原样'.encode()
                item.size = len(data)
                archive.addfile(item, io.BytesIO(data))
            result = subprocess.run(['zstd', '-q', '-c'], input=raw.getvalue(), stdout=subprocess.PIPE, check=True)
            names = []
            for index, offset in enumerate(range(0, len(result.stdout), 17)):
                path = root / f'part-{index:03}'
                path.write_bytes(result.stdout[offset:offset + 17])
                names.append({'name': path.name})
            target = root / 'new install'
            target.mkdir()
            restore.unpack(root, {'parts': names}, target, Path(__import__('shutil').which('zstd')))
            self.assertEqual((target / 'payload/原状态.txt').read_bytes(), data)


if __name__ == '__main__':
    unittest.main()
