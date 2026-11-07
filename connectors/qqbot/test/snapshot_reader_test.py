"""验收观察器遇到 Windows 快照替换竞争时有界等待，真实损坏仍失败。"""
import copy
import ctypes
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import memory_test


class FixtureDatabaseTest(unittest.TestCase):
    def test_whitelist_commit_closes_connections_before_fixture_cleanup(self):
        import sqlite3
        from contextlib import closing
        from fixture_auth import bootstrap_fixture_group_ids
        original = sqlite3.connect
        connections = []
        class Connection:
            def __init__(self, *args, **kwargs):
                self.inner = original(*args, **kwargs)
                self.closed = False
                connections.append(self)
            def __enter__(self):
                self.inner.__enter__()
                return self
            def __exit__(self, *args):
                return self.inner.__exit__(*args)
            def __getattr__(self, name):
                return getattr(self.inner, name)
            def close(self):
                self.inner.close()
                self.closed = True
        with tempfile.TemporaryDirectory() as root:
            try:
                with patch('fixture_auth.sqlite3.connect', Connection):
                    bootstrap_fixture_group_ids(root, ['group-1', 'group-2'])
                    bootstrap_fixture_group_ids(root, ['group-1'])
                self.assertEqual(len(connections), 2)
                self.assertTrue(all(c.closed for c in connections), '事务结束不代表 SQLite 连接已关闭')
                with closing(original(Path(root) / 'qq-group-whitelist.sqlite3')) as connection:
                    self.assertEqual(connection.execute('SELECT group_id FROM groups ORDER BY group_id').fetchall(), [('group-1',), ('group-2',)])
                    self.assertEqual(connection.execute('PRAGMA user_version').fetchone()[0], 1)
            finally:
                for connection in connections:
                    if not connection.closed:
                        connection.close()

    def test_invalid_test_group_cannot_create_a_database_or_expand_permissions(self):
        from fixture_auth import bootstrap_fixture_group_ids
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(ValueError):
                bootstrap_fixture_group_ids(root, ['../outside'])
            self.assertEqual(list(Path(root).iterdir()), [])


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

    def task_archive(self, version, archived=False, task=None):
        if task is None:
            task = dict(id='task-fixture', status='Cancelled', created_at_ms=10, finished_at_ms=20,
                        spec=dict(title='历史任务', context={'messages_json': 'x' * (4 * 1024**2 + 1)}),
                        result=None, failure=None)
        body = json.dumps(task, ensure_ascii=False).encode()
        digest = hashlib.sha256(body).hexdigest()
        archive = self.path.parent / 'task-archive'
        archive.mkdir(exist_ok=True)
        record = archive / (digest + '.json')
        record.write_bytes(body)
        directory = copy.deepcopy(task)
        directory['spec'].pop('context', None)
        if directory.get('execution') is not None:
            directory['execution']['checkpoint'] = None
            directory['execution'].pop('review_messages', None)
        entry = dict(directory=directory, record=dict(hash=digest, bytes=len(body)))
        if version == 5:
            entry['archived'] = archived
        index = dict(format_version=version, tasks=[entry])
        state = dict(version=1, entries={'eve.task': {'tasks.v1': list(json.dumps(index).encode())}})
        self.path.write_text(json.dumps(state), encoding='utf8')
        return record, task

    def test_v4_and_v5_task_reader_keep_complete_bodies_above_old_documented_four_mib(self):
        for version, archived in ((4, False), (5, False), (5, True)):
            record, task = self.task_archive(version, archived)
            before = self.path.read_bytes(), record.read_bytes()
            self.assertEqual(self.reader.documents()['eve.task']['tasks.v1'],
                             dict(format_version=version, tasks=[task]))
            self.assertEqual((self.path.read_bytes(), record.read_bytes()), before)

    def test_archived_task_reader_still_rejects_changed_body_without_rewriting_state(self):
        record, _ = self.task_archive(5, True)
        before = self.path.read_bytes()
        record.write_bytes(b'corrupt')
        with self.assertRaises(AssertionError):
            self.reader.documents()
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(record.read_bytes(), b'corrupt')

    def test_v5_reader_does_not_accept_archived_active_unconfirmed_or_undelivered_tasks(self):
        for change in (dict(status='Running'), dict(notice={'status': 'Pending'}),
                       dict(execution={'kind': 'exploration'}),
                       dict(execution={'progress_notices': [{'attempt': {'result': None}}]})):
            task = dict(id='task-fixture', status='Cancelled', created_at_ms=10, finished_at_ms=20,
                        spec={}, result=None, failure=None)
            task.update(change)
            record, _ = self.task_archive(5, True, task)
            before = self.path.read_bytes(), record.read_bytes()
            with self.assertRaises(AssertionError):
                self.reader.documents()
            self.assertEqual((self.path.read_bytes(), record.read_bytes()), before)

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
