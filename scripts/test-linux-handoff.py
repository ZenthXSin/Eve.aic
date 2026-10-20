"""不连接 QQ 的迁移脚本回归：分卷、覆盖保护、路径和进程身份。"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
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
