"""逐条交流的真实通道验收；只使用本地模型/QQ替身及自有临时子进程。"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import unittest

# 旧夹具在导入时读取 binary；CI 参数和发布器环境变量必须指向同一候选。
if __name__ == '__main__':
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument('--binary')
    options, remaining = parser.parse_known_args()
    if options.binary:
        os.environ['EVE_QQBOT_BINARY'] = str(Path(options.binary).resolve())
    sys.argv = [sys.argv[0], *remaining]

import dialogue_tools_test as dialogue
import live_dialogue_test as live


def qq_id(role, platform_id):
    return 'qq-' + role + ':' + hashlib.sha256(platform_id.encode()).hexdigest()


def scope_key(scope='c2c', target='user-1', user='user-1'):
    parts = ['1904159860', scope, target]
    if user is not None:
        parts.append(user)
    return 'qq:' + hashlib.sha256(json.dumps(parts, separators=(',', ':')).encode()).hexdigest()


class MessageLearningFlow(live.LiveToolAcceptance):
    for _name in dir(live.LiveToolAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def run_eve(self, script, **options):
        options['more'] = ['--training', *options.pop('more', [])]
        return super().run_eve(script, **options)

    def recorded(self, session=None, user=None):
        session = session or scope_key()
        user = user or session
        path = self.work / 'state/training-expressions.sqlite3'
        if not path.exists():
            return []
        with closing(sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True)) as db:
            return [dict(json.loads(message), stored_sequence=sequence)
                    for sequence, message in db.execute(
                        'SELECT sequence,message_json FROM conversation_events '
                        'WHERE session_id=? AND user_id=? ORDER BY sequence', (session, user))]

    def require_records(self, count, **scope):
        self.wait_until(lambda: len(self.recorded(**scope)) >= count, '逐条交流事件未持久保存')
        return self.recorded(**scope)

    def test_sent_human_sent_are_saved_in_one_running_turn_before_final_and_not_replayed(self):
        original = '  原始长正文\n```rust\n' + 'let value = "完整保留";\n' * 130 + '```\n> 原文引用\n'
        first = self.message('flow-first', original.strip()) | {'original_text': original,
            'source_at_ms': 1794096550000,
            'quoted': {'text': '被引用者的话，不能冒充本条正文', 'attachments': []},
            'attachments': [{'content_type': 'text/plain', 'url': 'https://example.test/note.txt',
                             'filename': 'note.txt'}]}
        followup = self.message('flow-second', '先别急，按我刚补充的内容接着说')
        started = self.source / 'tool-started'
        release = self.source / 'tool-release'
        marker = self.source / 'executions'
        script = self.source / 'wait_once.py'
        script.write_text("from pathlib import Path\nimport time\np=Path('executions')\n"
                          "p.write_text(p.read_text()+'x' if p.exists() else 'x')\n"
                          "Path('tool-started').touch()\n"
                          "while not Path('tool-release').exists(): time.sleep(.01)\n"
                          "print('tool-result')\n", encoding='utf8')

        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('send_message', {'text': '第一条已发送的话'})
            if self.chat_rounds == 2:
                self.assertTrue(self.all_results(body)[-1]['sent'])
                return self.choose('run_command', {'program': sys.executable, 'args': [script.name]})
            if self.chat_rounds == 3:
                self.assertIn(followup['text'], self.users(body))
                self.assertTrue(self.all_results(body)[-1]['success'])
                return self.choose('send_message', {'text': '第二条按补充接上的话'})
            return dialogue.text_response('内部结束，不应成为可见发言')

        # 最后一轮模型请求被门控，保证检查发生在第二条 Sent 后、整轮结束前。
        self.response_gates[('chat', 4)] = 'release-final'
        self.callback = respond

        def inspect_during_turn():
            records = self.require_records(4)
            self.assertEqual([m['id'] for m in records], [qq_id('human', first['id']),
                qq_id('eve', 'progress-flow-first-0'), qq_id('human', followup['id']),
                qq_id('eve', 'progress-flow-first-1')])
            self.assertEqual([m['role'] for m in records], ['human', 'eve', 'human', 'eve'])
            self.assertEqual(records[0]['text'], original)
            self.assertGreater(len(original.encode()), 2048)
            self.assertEqual(records[0]['metadata']['source_at_ms'], first['source_at_ms'])
            context = records[0]['metadata']['context']
            self.assertIn('被引用者的话', context)
            self.assertIn('note.txt', context)
            self.assertNotIn('被引用者的话', records[0]['text'])
            self.assertEqual(records[2]['metadata']['activity']['phase'], 'awaiting_tool_results')
            self.assertEqual(records[2]['metadata']['activity']['tool_name'], 'run_command')
            self.assertGreaterEqual(records[2]['metadata']['activity']['elapsed_ms'], 0)
            turns = [m['metadata']['turn_id'] for m in records[1:]]
            self.assertTrue(turns[0])
            self.assertEqual(turns, [turns[0]] * 3)
            self.assertEqual(records[1]['metadata']['source_kind'], 'qq_sent')
            self.assertEqual(records[3]['metadata']['source_kind'], 'qq_sent')
            self.assertEqual(self.receipt(first['id'])['state'], 'Processing')
            self.assertEqual(marker.read_text(), 'x')

        self.run_live([{'send': first}, {'wait_file': str(started)}, {'send': followup},
            self.wait_receipt(followup['id'], 'Merged'), {'touch': str(release)},
            {'wait_file': str(self.arrived('chat', 4))}, *self.checkpoint('in-turn'),
            {'touch': str(self.gate('release-final'))}, self.wait_receipt(first['id']),
            self.wait_command(first['id'], 'finish')], checkpoints={'in-turn': inspect_during_turn},
            env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config)})
        before = self.recorded()
        self.assertEqual([event['text'] for event in self.progress(first['id'])],
                         ['第一条已发送的话', '第二条按补充接上的话'])
        self.assertEqual(self.chat_rounds, 4)
        self.assertEqual(len([turn for session in self.documents()['eve.session']['sessions.v1']['sessions'].values()
                              for turn in session['turns']]), 1)
        self.response_gates.clear()
        self.run_live([{'send': first}, self.wait_command(first['id'], 'finish'),
                       {'send': followup}, self.wait_command(followup['id'], 'finish')])
        self.assertEqual(self.chat_rounds, 4, '恢复不得重跑模型或执行器')
        self.assertEqual(marker.read_text(), 'x')
        self.assertFalse(self.progress(first['id']))
        self.assertEqual(self.recorded(), before, '重投不能更新首次时间或新增序号')

    def test_sent_survives_later_failure_without_replaying_the_command(self):
        self.silent_failure_preserves_delivery('Sent')
        [eve] = [record for record in self.recorded() if record['role'] == 'eve']
        self.assertEqual(eve['id'], qq_id('eve', 'progress-quiet-failure-0'))
        self.assertEqual(eve['text'], '核对到的一条事实')
        self.assertEqual(eve['metadata']['source_kind'], 'qq_sent')

    def test_unknown_delivery_never_becomes_eve_evidence_after_restart(self):
        self.silent_failure_preserves_delivery('Unknown')
        records = self.recorded()
        self.assertTrue(records, '真实用户输入仍须采集')
        self.assertFalse([record for record in records if record['role'] == 'eve'])

    def test_group_backlog_keeps_every_original_even_while_learning_is_busy(self):
        self.response_gates[('conversation_learning', 1)] = 'hold-distillation'
        group_scope = {'session': scope_key('group', 'group-1', None), 'user': 'eve-chime'}
        inputs = []
        for index in range(24):
            original = f'  群原文 {index}\n```text\n保留代码\n```\n> 保留引用  '
            inputs.append({'send': {'type': 'group_observation', 'id': f'observe-{index}',
                'message': {'id': f'ambient-{index}', 'scope': 'group', 'target_id': 'group-1',
                            'user_id': 'user-2', 'text': original.strip()},
                'original_text': original, 'source_at_ms': 1794096550000 + index,
                'sender_name': '测试群友', 'mentions_me': False}})

        def inspect_backlog():
            records = self.require_records(24, **group_scope)
            self.assertEqual(len(records), 24)
            self.assertEqual([m['text'] for m in records], [s['send']['original_text'] for s in inputs])
            self.assertEqual([m['metadata']['source_at_ms'] for m in records],
                             [s['send']['source_at_ms'] for s in inputs])
            self.assertEqual([m['metadata']['sequence'] for m in records],
                             [m['stored_sequence'] for m in records])

        self.run_live([*inputs[:4], {'wait_file': str(self.arrived('conversation_learning', 1))},
                       *inputs[4:], *self.checkpoint('backlog')], checkpoints={'backlog': inspect_backlog})
        before = self.recorded(**group_scope)
        self.response_gates.clear()
        self.run_live([*inputs, *self.checkpoint('restored')], checkpoints={'restored': inspect_backlog})
        self.assertEqual(self.recorded(**group_scope), before)
        self.assertEqual(self.chat_rounds, 0, '纯群旁观采集不调用前台模型')
        self.assertFalse([e for e in self.events if e.get('direction') == 'out' and
                          e.get('type') in ('push', 'progress', 'reply', 'segment')])


if __name__ == '__main__':
    unittest.main()
