"""验收观察器遇到 Windows 快照替换竞争时有界等待，真实损坏仍失败。"""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import memory_test


class SnapshotReaderTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "state.json"
        self.reader = memory_test.MemoryAcceptance()
        self.reader.state_path = self.path
        self.payload = {"version": 1, "entries": {"test": {"value": list(b'{"n":1}')}}}
        self.path.write_text(json.dumps(self.payload), encoding="utf-8")

    def test_temporary_permission_failure_reads_committed_snapshot(self):
        original = Path.read_text
        attempts = []

        def read(path, *args, **kwargs):
            attempts.append(path)
            if len(attempts) == 1:
                raise PermissionError(13, "snapshot replacement")
            return original(path, *args, **kwargs)

        with patch.object(Path, "read_text", read):
            self.assertEqual(self.reader.documents(), {"test": {"value": {"n": 1}}})
        self.assertEqual(len(attempts), 2)

    def test_persistent_permission_failure_is_not_empty_state(self):
        with patch.object(Path, "read_text", side_effect=PermissionError(13, "denied")) as read:
            with self.assertRaises(PermissionError):
                self.reader.documents()
        self.assertGreater(read.call_count, 1)
        self.assertLessEqual(read.call_count, 20)
        self.assertEqual(json.loads(self.path.read_text(encoding="utf-8")), self.payload)

    def test_corrupt_snapshot_fails_without_retry_or_rewrite(self):
        self.path.write_text("broken", encoding="utf-8")
        with patch.object(Path, "read_text", autospec=True, return_value="broken") as read:
            with self.assertRaises(json.JSONDecodeError):
                self.reader.documents()
        self.assertEqual(read.call_count, 1)
        self.assertEqual(self.path.read_text(encoding="utf-8"), "broken")

    def test_absent_initial_snapshot_is_empty(self):
        self.path.unlink()
        self.assertEqual(self.reader.documents(), {})

    def test_cold_learning_reader_uses_snapshot_parent_without_optional_work_directory(self):
        body = json.dumps({"job": {"status": "Completed"}}).encode()
        digest = hashlib.sha256(body).hexdigest()
        archive = self.path.parent / "learning-archive"
        archive.mkdir()
        record = archive / (digest + ".json")
        record.write_bytes(body)
        index = {"format_version": 2, "kind": "jobs", "records": [{"hash": digest, "bytes": len(body)}]}
        self.path.write_text(json.dumps({"version": 1, "entries": {
            "eve.learning": {"learning.v1": list(json.dumps(index).encode())}}}), encoding="utf8")
        before = self.path.read_bytes()
        self.assertFalse(hasattr(self.reader, "work"))
        self.assertEqual(self.reader.documents()["eve.learning"]["learning.v1"],
                         {"format_version": 1, "jobs": [{"job": {"status": "Completed"}}]})
        record.write_bytes(b"corrupt")
        with self.assertRaises(AssertionError):
            self.reader.documents()
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(record.read_bytes(), b"corrupt")

    @unittest.skipUnless(os.name == "nt", "Windows 文件共享语义")
    def test_real_windows_exclusive_handle_is_released_before_retry(self):
        from ctypes import wintypes
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                      wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
        kernel.CreateFileW.restype = wintypes.HANDLE
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel.CloseHandle.restype = wintypes.BOOL
        handle = kernel.CreateFileW(str(self.path), 0x80000000, 0, None, 3, 0x80, None)
        self.assertNotEqual(handle, wintypes.HANDLE(-1).value)
        original = Path.read_text
        failures = []

        def read(path, *args, **kwargs):
            nonlocal handle
            try:
                return original(path, *args, **kwargs)
            except PermissionError as error:
                failures.append(error)
                self.assertTrue(kernel.CloseHandle(handle))
                handle = None
                raise

        try:
            with patch.object(Path, "read_text", read):
                self.assertEqual(self.reader.documents(), {"test": {"value": {"n": 1}}})
            self.assertEqual(len(failures), 1)
        finally:
            if handle is not None:
                kernel.CloseHandle(handle)


if __name__ == "__main__":
    unittest.main()
