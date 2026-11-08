"""root 本机入口经真实 QQ 宿主重访原消息；HTTP 和桥接均为本地替身，不发送聊天。"""
from contextlib import closing
import hashlib
import json
import os
import sqlite3
import time
import unittest

import interest_test


SCOPE = {'session_id': 'isolated-history-scope', 'user_id': 'isolated-history-user'}
OLD_RESPONSE = '对方分享近况时先用一句话接住话题。'
NEW_RESPONSE = '对方分享近况时顺着眼下经历接话，需详细讨论时继续展开。'


@unittest.skipUnless(os.name == 'posix' and hasattr(os, 'geteuid') and os.geteuid() == 0,
                     '完整维护入口要求 Unix root；不降低生产权限门槛')
class ConversationHistoryMaintenanceAcceptance(interest_test.InterestAcceptance):
    for name in dir(interest_test.InterestAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.maintenance = self.work / 'state/conversation-maintenance'
        self.learner = lambda _: {'lessons': []}
        self.records = [{'scope': SCOPE, 'event': {'sequence': n, 'message': {
            'id': f'original-{n}', 'speaker': 'original-human', 'role': 'human',
            'text': text, 'metadata': {'ordering_uncertain': True},
        }}} for n, text in enumerate([
            '今天只是想聊聊刚才路上看到的事情。', '嗯，顺着这个话题聊就好。',
            '要详细讨论时我也会接着展开。', '这个话题先放这里，之后还会接着说。',
        ], 1)]

    def reply(self, kind, decoded, latest):
        if kind == 'conversation_learning':
            self.assertTrue(decoded['id'].startswith('conversation-revisit-v2-'))
            request = self.kind(kind)[-1]['body']
            self.assertFalse(request.get('tools'))
            self.assertNotIn('temperature', request)
            self.assertEqual(request.get('response_format'), {'type': 'json_object'})
            self.assertLessEqual(len(request['messages'][-1]['content'].encode()), 65536)
            self.assertFalse(decoded.get('feedback_pairs'), '未知先后关系不能冒充真人纠正')
            return json.dumps(self.learner(decoded), ensure_ascii=False)
        self.fail('维护不得触发聊天、旁观或额外模型用途：' + kind)

    def database(self):
        return self.work / 'state/training-expressions.sqlite3'

    def checkpoint_data(self):
        if not self.database().exists():
            return None
        with closing(sqlite3.connect(self.database().resolve().as_uri() + '?mode=ro', uri=True)) as db:
            row = db.execute('SELECT checkpoint FROM conversation_scopes '
                             'WHERE session_id=? AND user_id=?', tuple(SCOPE.values())).fetchone()
        return json.loads(row[0]) if row and row[0] else None

    def rows(self, table):
        self.assertIn(table, {'conversation_events', 'conversation_batches', 'conversation_revisions',
                             'conversation_seen', 'conversation_scopes'})
        with closing(sqlite3.connect(self.database().resolve().as_uri() + '?mode=ro', uri=True)) as db:
            return db.execute('SELECT * FROM ' + table + ' ORDER BY rowid').fetchall()

    def status(self):
        path = self.maintenance / 'status.json'
        return json.loads(path.read_text()) if path.exists() else None

    def wait_status(self, phase):
        deadline = time.monotonic() + 20
        while not (self.status() or {}).get('phase') == phase:
            self.assertLess(time.monotonic(), deadline, str(self.status()))
            self.assertFalse(self.run_release.wait(.02))
        self.assertIsNone(self.status()['error'])

    def submit(self, request_id):
        self.maintenance.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.maintenance.chmod(0o700)
        content = ''.join(json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n'
                          for record in self.records).encode()
        source = self.maintenance / 'events.jsonl'
        source.write_bytes(content)
        source.chmod(0o600)
        request = {'version': 1, 'id': request_id, 'source': 'events.jsonl',
                   'sha256': hashlib.sha256(content).hexdigest(), 'records': len(self.records)}
        pending = self.maintenance / 'request.new'
        pending.write_text(json.dumps(request))
        pending.chmod(0o600)
        pending.replace(self.maintenance / 'request.json')
        for path in (self.maintenance, source, self.maintenance / 'request.json'):
            self.assertEqual(path.stat().st_uid, 0)
            self.assertEqual(path.stat().st_mode & 0o022, 0)

    def run_worker(self, script, **options):
        summary = self.run_eve(script, interest=False, memory=False, extra=['--training'],
                               timeout=45, **options)
        self.assertEqual(summary['received'], 0, '维护不能创造聊天输入')
        self.assertEqual(summary['sent'], 0, '维护不能主动发送 QQ 消息')
        events = self.work / f'events-{self.runs}.jsonl'
        frames = [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []
        self.assertFalse([e for e in frames if e.get('direction') == 'out'
                          and e.get('type') in ('reply', 'progress', 'push', 'passive_push', 'segment')])
        return summary

    def proposal(self, batch, response, lesson_id=None):
        return {'lessons': [{'lesson_id': lesson_id, 'situation': '对方分享近况的交流场景',
                            'response': response, 'avoid': '不强迫简短，也不自动追问。',
                            'assessment': 'observation', 'evidence': [
                                {'message_id': message['id'], 'quote': message['text']}
                                for message in batch['messages'][:2]]}]}

    def test_maintenance_revises_old_experience_then_restart_and_reposting_do_not_replay(self):
        self.learner = lambda batch: self.proposal(batch, OLD_RESPONSE)
        self.run_worker([*self.checkpoint('submit-seed'), *self.checkpoint('seed-completed')],
                        checkpoints={'submit-seed': lambda: self.submit('history-seed'),
                                     'seed-completed': lambda: self.wait_status('completed')})
        [old] = self.checkpoint_data()['lessons']
        self.assertEqual(old['response'], OLD_RESPONSE)
        self.assertEqual(old['observations'], 1)
        self.assertEqual(self.status()['progress']['model_requests'], 1)
        original_events = self.rows('conversation_events')
        self.assertEqual(len(original_events), 4)
        # 旧请求的整个账本移为保全归档；新请求在下一次独立宿主启动后投递。
        self.maintenance.rename(self.work / 'state/conversation-maintenance-seed-archive')

        def revise(batch):
            self.assertEqual([m['id'] for m in batch['messages']],
                             [record['event']['message']['id'] for record in self.records])
            self.assertTrue(any(lesson['id'] == old['id'] and lesson['response'] == OLD_RESPONSE
                                for lesson in batch['lessons']), '重访请求必须携带同范围旧经验')
            return self.proposal(batch, NEW_RESPONSE, old['id'])

        self.learner = revise
        self.run_worker([*self.checkpoint('submit-revisit'), *self.checkpoint('revisit-completed')],
                        checkpoints={'submit-revisit': lambda: self.submit('history-revisit'),
                                     'revisit-completed': lambda: self.wait_status('completed')})
        [revised] = self.checkpoint_data()['lessons']
        self.assertEqual(revised['id'], old['id'])
        self.assertEqual(revised['response'], NEW_RESPONSE)
        for key in ('observations', 'corrections', 'counterexamples', 'active', 'evaluated_message_ids'):
            self.assertEqual(revised[key], old[key], '重读旧证据不能刷新支持或反例：' + key)
        self.assertEqual(self.rows('conversation_events'), original_events)
        status = self.status()
        self.assertEqual(status['imported_lines'], 4)
        self.assertEqual(status['confirmed_new_events'], 0)
        self.assertEqual(status['progress'], {'id': 'history-revisit', 'scopes': 1, 'total': 4,
            'processed': 4, 'remaining': 0, 'updated': 1, 'failed_batches': 0,
            'interrupted_batches': 0, 'model_requests': 1, 'completed': True})
        self.assertTrue(self.rows('conversation_revisions'), '旧版本必须存档')
        self.assertEqual(len(self.kind('conversation_learning')), 2)
        headers = [request['session_headers'] for request in self.kind('conversation_learning')]
        self.assertEqual(headers[0]['session-id'], headers[1]['session-id'])
        self.assertNotEqual(headers[0]['x-client-request-id'], headers[1]['x-client-request-id'])
        before = {table: self.rows(table) for table in ('conversation_events', 'conversation_batches',
                                                        'conversation_revisions', 'conversation_scopes')}
        count = len(self.requests)
        self.learner = lambda _: self.fail('已完成的维护请求不能在重启后重发')

        def unchanged():
            self.submit('history-revisit')
            self.assertFalse(self.run_release.wait(2.5))
            self.assertEqual(self.status(), status)
            self.assertEqual(len(self.requests), count)
            for table, rows in before.items():
                self.assertEqual(self.rows(table), rows)

        self.run_worker(self.checkpoint('no-replay'), checkpoints={'no-replay': unchanged})

    def test_seen_without_body_is_backfilled_without_a_new_identity(self):
        self.run_worker(self.checkpoint('database-ready'), checkpoints={
            'database-ready': lambda: self.wait_until(self.database().exists, '训练数据库未初始化')})
        message = self.records[0]['event']['message']
        core = {key: message[key] for key in ('id', 'speaker', 'role', 'text')}
        id_sha = hashlib.sha256(message['id'].encode()).hexdigest()
        body_sha = hashlib.sha256(json.dumps(core, ensure_ascii=False,
                                             separators=(',', ':')).encode()).hexdigest()
        # 宿主已停止；模拟旧版暂停时真实持久层仅存seen摘要、无正文的恢复状态。
        with closing(sqlite3.connect(self.database())) as db, db:
            db.execute('INSERT INTO conversation_scopes(session_id,user_id,revision,next_sequence,checkpoint) '
                       'VALUES(?,?,0,1,NULL)', tuple(SCOPE.values()))
            db.execute('INSERT INTO conversation_seen(session_id,user_id,id_sha,body_sha) VALUES(?,?,?,?)',
                       (*SCOPE.values(), id_sha, body_sha))
        self.learner = lambda _: {'lessons': []}
        self.run_worker([*self.checkpoint('submit-backfill'), *self.checkpoint('backfill-completed')],
                        checkpoints={'submit-backfill': lambda: self.submit('history-backfill'),
                                     'backfill-completed': lambda: self.wait_status('completed')})
        self.assertEqual(len(self.rows('conversation_seen')), 4)
        self.assertEqual(len(self.rows('conversation_events')), 4)
        stored = [json.loads(row[-1]) for row in self.rows('conversation_events')]
        self.assertEqual([item['id'] for item in stored],
                         [record['event']['message']['id'] for record in self.records])
        self.assertEqual(stored[0]['text'], message['text'])
        self.assertEqual(self.status()['progress']['processed'], 4)
        self.assertEqual(self.status()['progress']['model_requests'], 1)
        self.assertEqual(len(self.requests), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
