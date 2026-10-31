"""保持独立进程、原参数和真实失败，不在测试阶段重新触发编译。"""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('prebuilt', Path(__file__).with_name('run-prebuilt.py'))
prebuilt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prebuilt)


class PrebuiltTests(unittest.TestCase):
    def test_binary_and_example_paths_keep_space_unicode_and_flags_as_separate_arguments(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root) / '含空格 target'
            for kind, folder in [('bin', directory / 'debug'), ('example', directory / 'debug/examples')]:
                folder.mkdir(parents=True, exist_ok=True)
                path = folder / ('test-target' + ('.exe' if os.name == 'nt' else ''))
                path.write_bytes(b'owned test artifact')
                arguments = ['带空格的状态目录', '--stream', 'literal$(not-shell)']
                self.assertEqual(prebuilt.command(directory, kind, 'test-target', arguments), [str(path.resolve()), *arguments])

    def test_missing_or_traversing_target_is_rejected_without_invoking_cargo(self):
        with tempfile.TemporaryDirectory() as root:
            for kind, name in [('example', '../outside'), ('bin', 'bad/path'), ('other', 'test'), ('bin', ''), ('bin', 'missing')]:
                with self.subTest(kind=kind, name=name), self.assertRaises(ValueError):
                    prebuilt.command(root, kind, name, [])

    def test_child_failure_code_is_preserved_and_arguments_are_not_shell_text(self):
        with patch.object(prebuilt.sys, 'argv', ['run-prebuilt.py', '--example', 'target', '--', 'a b', '--mode']), \
             patch.object(prebuilt, 'command', return_value=['/owned/target', 'a b', '--mode']) as command, \
             patch.object(prebuilt.subprocess, 'run') as run:
            run.return_value.returncode = 23
            self.assertEqual(prebuilt.main(), 23)
            command.assert_called_once_with(os.environ.get('CARGO_TARGET_DIR', 'target'), 'example', 'target', ['a b', '--mode'])
            run.assert_called_once_with(['/owned/target', 'a b', '--mode'], check=False)


if __name__ == '__main__':
    unittest.main()
