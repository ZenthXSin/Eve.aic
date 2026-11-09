"""真实隔离宿主验收历史重访、增量公平性和恢复；HTTP/QQ 都是本地替身。

超过 60 秒的装配验收仅在 EVE_TEST_SLOW_MAINTENANCE=1 时运行，不拖慢常规套件。
"""
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
        self.scope = SCOPE.copy()
        self.allow_incremental = False
        self.allow_chat = False
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
            maintenance = decoded['id'].startswith('conversation-revisit-v2-')
            self.assertTrue(maintenance or self.allow_incremental)
            request = self.kind(kind)[-1]['body']
            self.assertFalse(request.get('tools'))
            self.assertNotIn('temperature', request)
            self.assertEqual(request.get('response_format'), {'type': 'json_object'})
            self.assertLessEqual(len(request['messages'][-1]['content'].encode()), 65536)
            if maintenance:
                self.assertFalse(decoded.get('feedback_pairs'), '未知先后关系不能冒充真人纠正')
            return json.dumps(self.learner(decoded), ensure_ascii=False)
        if kind == 'chat' and self.allow_chat:
            return super().reply(kind, decoded, latest)
        self.fail('维护不得触发聊天、旁观或额外模型用途：' + kind)

    def database(self):
        return self.work / 'state/training-expressions.sqlite3'

    def checkpoint_data(self):
        if not self.database().exists():
            return None
        with closing(sqlite3.connect(self.database().resolve().as_uri() + '?mode=ro', uri=True)) as db:
            row = db.execute('SELECT checkpoint FROM conversation_scopes '
                             'WHERE session_id=? AND user_id=?', tuple(self.scope.values())).fetchone()
        return json.loads(row[0]) if row and row[0] else None

    def rows(self, table):
        self.assertIn(table, {'conversation_events', 'conversation_batches', 'conversation_revisions',
                             'conversation_seen', 'conversation_scopes'})
        with closing(sqlite3.connect(self.database().resolve().as_uri() + '?mode=ro', uri=True)) as db:
            return db.execute('SELECT * FROM ' + table + ' ORDER BY rowid').fetchall()

    def status(self):
        path = self.maintenance / 'status.json'
        return json.loads(path.read_text()) if path.exists() else None

    def wait_status(self, phase, timeout=20):
        deadline = time.monotonic() + timeout
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
        received = options.pop('received', 0)
        sent = options.pop('sent', 0)
        timeout = options.pop('timeout', 45)
        summary = self.run_eve(script, interest=False, memory=False, extra=['--training'],
                               timeout=timeout, **options)
        if summary is None:
            return None  # 显式 stop_at 用于真实进程中断；原夹具已核对非零退出。
        self.assertEqual(summary['received'], received, '维护不能创造额外聊天输入')
        self.assertEqual(summary['sent'], sent, '维护不能主动发送 QQ 消息')
        events = self.work / f'events-{self.runs}.jsonl'
        frames = [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []
        outgoing = [e for e in frames if e.get('direction') == 'out'
                    and e.get('type') in ('reply', 'progress', 'push', 'passive_push', 'segment')]
        self.assertEqual(len(outgoing), sent)
        self.assertTrue(all(e['type'] == 'reply' for e in outgoing),
                        '学习后台不能主动发送 QQ 消息')
        return summary

    def populate_history(self, count):
        self.records = [{'scope': self.scope, 'event': {'sequence': n, 'message': {
            'id': f'original-{n}', 'speaker': 'original-human', 'role': 'human',
            'text': f'历史交流原文第 {n} 条，需要保留原来的内容。',
            'metadata': {'ordering_uncertain': True},
        }}} for n in range(1, count + 1)]

    def assert_message_counts(self, processed, remaining=0, failed=0, unknown=0):
        progress = self.status()['progress']
        self.assertEqual(progress['processed'], processed)
        self.assertEqual(progress['remaining'], remaining)
        self.assertEqual(progress['message_counts'], {
            'failed': failed, 'unknown': unknown, 'unclassified': 0, 'legacy_processed': 0,
            'partial': 0, 'partial_failed': 0, 'partial_unknown': 0})
        self.assertEqual(progress['total'], processed + failed + unknown + remaining)

    def assert_no_replay(self, request_id):
        before = {table: self.rows(table) for table in (
            'conversation_events', 'conversation_batches', 'conversation_revisions',
            'conversation_seen', 'conversation_scopes')}
        status, requests = self.status(), len(self.requests)
        self.learner = lambda _: self.fail('重启不能重发已经消费或结果未知的维护批次')

        def unchanged():
            self.submit(request_id)
            # 负向观察覆盖两次正常 worker 扫描；不推进时钟或修改生产冷却规则。
            self.assertFalse(self.run_release.wait(2.5))
            self.assertEqual(len(self.requests), requests)
            self.assertEqual(self.status(), status)
            for table, rows in before.items():
                self.assertEqual(self.rows(table), rows)

        self.run_worker(self.checkpoint('no-replay-new'), checkpoints={'no-replay-new': unchanged})

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
            'interrupted_batches': 0, 'model_requests': 1, 'completed': True,
            'message_counts': {'failed': 0, 'unknown': 0, 'unclassified': 0, 'legacy_processed': 0,
                               'partial': 0, 'partial_failed': 0, 'partial_unknown': 0}})
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

    def test_same_scope_new_messages_learn_before_remaining_history_and_restart_does_not_replay(self):
        principal = 'qq:' + hashlib.sha256(json.dumps(
            ['1904159860', 'c2c', 'user-1', 'user-1'], separators=(',', ':')).encode()).hexdigest()
        self.scope = {'session_id': principal, 'user_id': principal}
        self.populate_history(260)  # 三批短历史，不靠修改批次上限或冷却加速。
        self.allow_incremental = self.allow_chat = True
        self.response_gates[('conversation_learning', 1)] = 'first-maintenance-release'
        fresh = [self.message(f'fresh-{n}', f'刚收到的新交流第 {n} 条。') for n in range(4)]
        order, normal = [], []

        def learn(batch):
            self.assertEqual(batch['scope'], self.scope)
            maintenance = batch['id'].startswith('conversation-revisit-v2-')
            order.append(maintenance)
            if not maintenance:
                normal.append(batch)
                self.assertEqual(self.status()['phase'], 'running')
                self.assertGreater(self.status()['progress']['remaining'], 0,
                                   '同范围新消息不能等历史全部重访后才学习')
                human_texts = {message['text'] for message in batch['messages']
                               if message['role'] == 'human'}
                self.assertTrue({message['text'] for message in fresh} <= human_texts)
            return {'lessons': []}

        self.learner = learn

        def before_release():
            requests = self.kind('conversation_learning')
            self.assertEqual(len(requests), 1, '单 worker 不能预取第二个同范围在途批次')
            first = json.loads(requests[0]['body']['messages'][-1]['content'])
            self.assertTrue(first['id'].startswith('conversation-revisit-v2-'))
            self.assertEqual(first['scope'], self.scope)
            self.assertEqual(len(first['messages']), 128)
            self.assertEqual(self.status()['phase'], 'running')
            self.assertEqual(len(self.rows('conversation_events')), 268,
                             '4 条新输入及 4 条真实回复必须先持久化，不能只靠模型假设')

        script = [*self.checkpoint('submit-fairness'),
                  {'wait_file': str(self.arrived('conversation_learning', 1))}]
        for message in fresh:
            script.extend(self.send(message))
        script.extend([*self.checkpoint('new-events-ready'),
                       {'touch': str(self.gate('first-maintenance-release'))},
                       *self.checkpoint('fairness-completed')])
        self.run_worker(script, received=4, sent=4, checkpoints={
            'submit-fairness': lambda: self.submit('history-fairness'),
            'new-events-ready': before_release,
            'fairness-completed': lambda: self.wait_status('completed')})
        self.assertEqual(order, [True, False, True, True],
                         '首维护批之后应立即领取已有增量，而不是继续耗尽维护队列')
        self.assertEqual(len(normal), 1)
        self.assertEqual(self.status()['progress']['model_requests'], 3)
        self.assert_message_counts(260)
        events_before = self.rows('conversation_events')
        self.assertEqual(len(events_before), 268)
        request_count = len(self.requests)
        self.learner = lambda _: self.fail('重复平台消息或重启不能重放原学习批次')
        duplicate = [{"send": {**message, 'expected_type': 'finish'}} for message in fresh]
        duplicate.extend({'wait_command': {'id': message['id'], 'type': 'finish'}} for message in fresh)

        def repeated_messages_unchanged():
            self.assertFalse(self.run_release.wait(2.5))
            self.assertEqual(self.rows('conversation_events'), events_before)
            self.assertEqual(len(self.requests), request_count)

        self.run_worker([*duplicate, *self.checkpoint('duplicate-quiet')],
                        checkpoints={'duplicate-quiet': repeated_messages_unchanged})
        self.assert_no_replay('history-fairness')

    def test_interrupted_maintenance_keeps_unknown_counts_and_original_events_without_replay(self):
        self.populate_history(260)
        self.response_gates[('conversation_learning', 2)] = 'unknown-maintenance-held'
        stop = self.arrived('conversation_learning', 2)
        self.run_worker([*self.checkpoint('submit-interrupted'),
                         {'wait_file': str(stop)},
                         {'wait_file': str(self.gate('never-release-interrupted'))}],
                        stop_at=stop,
                        checkpoints={'submit-interrupted': lambda: self.submit('history-interrupted')})
        sent = [json.loads(request['body']['messages'][-1]['content'])
                for request in self.kind('conversation_learning')]
        self.assertEqual([len(batch['messages']) for batch in sent], [128, 128])
        interrupted_id = sent[1]['id']
        events_before = self.rows('conversation_events')
        self.assertEqual(len(events_before), 260)
        resumed = []

        def learn_remaining(batch):
            self.assertNotEqual(batch['id'], interrupted_id)
            self.assertEqual([message['id'] for message in batch['messages']],
                             [f'original-{number}' for number in range(257, 261)])
            resumed.append(batch['id'])
            return {'lessons': []}

        self.learner = learn_remaining
        self.run_worker(self.checkpoint('interrupted-resumed'), checkpoints={
            'interrupted-resumed': lambda: self.wait_status('completed_with_failures')})
        self.assertEqual(len(resumed), 1, '未知批次不能自动再次调用，剩余历史仍须继续')
        self.assertEqual(self.rows('conversation_events'), events_before)
        self.assert_message_counts(132, unknown=128)
        progress = self.status()['progress']
        self.assertEqual(progress['model_requests'], 3)
        self.assertEqual(progress['failed_batches'], 0)
        self.assertEqual(progress['interrupted_batches'], 1)
        with closing(sqlite3.connect(self.database().resolve().as_uri() + '?mode=ro', uri=True)) as db:
            archives = [(json.loads(batch)['id'], outcome) for batch, outcome in db.execute(
                'SELECT batch_json,outcome FROM conversation_batches ORDER BY sequence')]
        self.assertEqual([outcome for batch_id, outcome in archives if batch_id == interrupted_id],
                         ['interrupted'])
        self.assert_no_replay('history-interrupted')

    def test_failed_attempt_remains_pending_instead_of_counting_messages_as_completed(self):
        self.learner = lambda _: {'lessons': '故意返回不合法结构，验收真实提供方错误路径'}

        def failed_attempt_saved():
            self.wait_until(lambda: bool((self.checkpoint_data() or {}).get('inflight', {})
                                         and self.checkpoint_data()['inflight']['retryable']),
                            '不合法模型输出没有保存真实失败回执与待重试状态')
            self.wait_until(lambda: ((self.status() or {}).get('progress') or {}).get('model_requests') == 1,
                            '失败请求计数未更新')
            self.assertEqual(self.status()['phase'], 'running')
            self.assert_message_counts(0, remaining=4)
            self.assertEqual(self.status()['progress']['failed_batches'], 0,
                             '一次失败尝试尚未耗尽原来的三次预算，不能记整批失败')

        self.run_worker([*self.checkpoint('submit-failed'), *self.checkpoint('failed-saved')],
                        checkpoints={'submit-failed': lambda: self.submit('history-failed'),
                                     'failed-saved': failed_attempt_saved})
        self.assertEqual(len(self.kind('conversation_learning')), 1)
        self.assertEqual(len(self.rows('conversation_events')), 4)
        with closing(sqlite3.connect(self.database().resolve().as_uri() + '?mode=ro', uri=True)) as db:
            outcomes = db.execute('SELECT outcome FROM conversation_batches ORDER BY sequence').fetchall()
        self.assertEqual(outcomes, [('failed',)])
        # 保留生产的 60 秒重试冷却，不为覆盖最终失败而改时钟或额外等三轮。
        self.assert_no_replay('history-failed')

    @unittest.skipUnless(os.environ.get('EVE_TEST_SLOW_MAINTENANCE') == '1',
                         '90 秒真实维护超时装配验收仅在显式启用时运行')
    def test_slow_maintenance_response_after_sixty_seconds_is_committed_once(self):
        def slow_response(batch):
            self.assertEqual(len(self.kind('conversation_learning')), 1)
            started = time.monotonic()
            self.assertFalse(self.run_release.wait(65), '宿主在 60 秒处提前结束维护')
            checkpoint = self.checkpoint_data()
            self.assertEqual(checkpoint['inflight']['batch']['id'], batch['id'])
            self.assertFalse(checkpoint['inflight']['retryable'], '维护不能被普通 60 秒时限中断')
            self.assertEqual(self.rows('conversation_batches'), [])
            self.assertFalse(self.run_release.wait(max(0, 90 - (time.monotonic() - started))))
            return {'lessons': []}

        self.learner = slow_response
        self.run_worker([*self.checkpoint('submit-slow'), *self.checkpoint('slow-completed')],
                        timeout=120, env_extra={'EVE_OPENAI_TIMEOUT_SECONDS': '120'},
                        checkpoints={'submit-slow': lambda: self.submit('history-slow'),
                                     'slow-completed': lambda: self.wait_status('completed', timeout=110)})
        self.assertEqual(len(self.kind('conversation_learning')), 1)
        self.assertEqual(self.status()['progress']['model_requests'], 1)
        self.assert_message_counts(4)


if __name__ == '__main__':
    unittest.main(verbosity=2)
