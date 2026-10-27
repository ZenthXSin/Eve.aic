"""真实 QQ 进程的产物入库、重名与成员隔离验收；平台仅使用本地替身。"""
import json
import sys
import unittest
from pathlib import Path

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response


class FileImportAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def ledger(self):
        path = self.work / "state/qq-outbox/ledger.json"
        return json.loads(path.read_text())['records'] if path.exists() else []

    def wait_files(self, count):
        self.wait_for(lambda: len(self.ledger()) == count and all(
            record['state'] == 'Sent' for record in self.ledger()), '文件未收到发送回执', 15)

    def run_files(self, script, **options):
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                            env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config)}, **options)

    def test_owner_artifacts_auto_import_collision_and_restart_do_not_replay(self):
        first = self.source / 'build/报告.txt'
        first.parent.mkdir()
        first.write_text('FIRST_ARTIFACT')
        second = self.work / 'second.txt'
        second.write_text('SECOND_ARTIFACT')
        files = self.work / 'state/qq-outbox/files'
        files.mkdir(parents=True)
        (files / '报告.txt').write_text('EXISTING_ARTIFACT')
        def respond(body):
            if not self.results(body):
                return tool_response([
                    ('send_file', {'path': 'build/报告.txt'}),
                    ('send_file', {'path': str(second), 'name': '报告.txt'})])
            results = self.results(body)
            self.assertTrue(all(r.get('queued') and r.get('imported') for r in results), results)
            self.assertEqual([r['name'] for r in results], ['报告_1.txt', '报告_2.txt'])
            return text_response('两份文件已排队')
        self.callback = respond
        self.run_files([*self.send(self.message('import', '发送这两份报告', expected='两份文件已排队')),
                        *self.checkpoint('sent')], checkpoints={'sent': lambda: self.wait_files(2)})
        self.assertEqual((files / '报告.txt').read_text(), 'EXISTING_ARTIFACT')
        self.assertEqual((files / '报告_1.txt').read_text(), 'FIRST_ARTIFACT')
        self.assertEqual((files / '报告_2.txt').read_text(), 'SECOND_ARTIFACT')
        self.assertEqual(first.read_text(), 'FIRST_ARTIFACT')
        self.assertEqual(second.read_text(), 'SECOND_ARTIFACT')
        self.callback = lambda _: text_response('重启后记录仍在')
        self.run_files(self.send(self.message('back', '查看上次记录', expected='重启后记录仍在')))
        events = [json.loads(line) for line in (self.work / f'events-{self.runs}.jsonl').read_text().splitlines()]
        self.assertFalse(any(e.get('type') == 'file' for e in events))
        self.assertEqual([r['state'] for r in self.ledger()], ['Sent', 'Sent'])

    @unittest.skipUnless(sys.platform == 'linux' and Path('/usr/bin/bwrap').is_file(), '需要 Linux 成员沙盒')
    def test_member_root_artifact_import_and_other_member_denial(self):
        def respond(body):
            latest = next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')
            results = self.results(body)
            if latest == '另一位成员取文件':
                if not results:
                    return tool_response([('send_file', {'path': '/work/产物.txt'})])
                self.assertIn('error', results[-1])
                return text_response('不能读取他人文件')
            if not results:
                return tool_response([('write_file', {'path': '产物.txt', 'content': 'MEMBER_ARTIFACT', 'expected_sha256': ''})])
            if len(results) == 1:
                self.assertTrue(results[0]['success'])
                return tool_response([('send_file', {'path': '产物.txt'})])
            self.assertTrue(results[-1].get('queued'), results[-1])
            return text_response('成员产物已排队')
        self.callback = respond
        route = {'scope': 'group', 'target': 'group-1'}
        self.run_files([
            *self.send(self.message('member-file', '生成后直接发送', user='user-2', expected='成员产物已排队', **route)),
            *self.checkpoint('sent'),
            *self.send(self.message('foreign-file', '另一位成员取文件', user='user-3', expected='不能读取他人文件', **route))],
            checkpoints={'sent': lambda: self.wait_files(1)})
        [record] = self.ledger()
        self.assertEqual(Path(record['path']).read_text(), 'MEMBER_ARTIFACT')
        self.assertEqual(record['name'], '产物.txt')
        events = [json.loads(line) for line in (self.work / f'events-{self.runs}.jsonl').read_text().splitlines()]
        [file] = [e for e in events if e.get('type') == 'file']
        self.assertEqual((file['scope'], file['target_id'], file['mention_user_id']), ('group', 'group-1', 'user-2'))


if __name__ == '__main__':
    unittest.main()
