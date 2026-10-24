"""实际 Eve 进程验收群消息按需增量整理：群里攒下的全量消息只在 Eve 调用 group_digest 时整理，
已经整理过的不再交给模型；摘要库跨重启保留游标，私聊不能读取群记录。"""
import json
import unittest
from contextlib import closing
import sqlite3

import group_chat_test
from dialogue_tools_test import text_response, tool_response


class GroupDigestAcceptance(group_chat_test.GroupChatAcceptance):
    for _name in dir(group_chat_test.GroupChatAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.batches = []
        self.calls = 0

        def respond(body):
            results = self.results(body)
            if not results:
                self.calls += 1
                response = tool_response([('group_digest', {})])
                response['choices'][0]['message']['tool_calls'][0]['id'] += f'-{self.calls}'
                return response
            [result] = results
            if 'error' in result and 'digests' not in result:
                return text_response('私聊看不到群记录')
            return text_response('回顾 ' + result['digests'][-1]['summary'])
        self.callback = respond

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and set(decoded) == {'persona', 'previous', 'messages'}:
            return 'group_digest'
        return super().classify(decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == 'group_digest':
            texts = [m['text'] for m in decoded['messages']]
            self.batches.append({'previous': decoded['previous'], 'texts': texts})
            return json.dumps({'summary': '整理' + '+'.join(texts)}, ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def ask(self, id, text):
        return [self.observation(id, text, mentioned=True),
                *self.send(self.message(id, text, contains=['回顾 整理'], scope='group'))]

    def digests(self):
        with closing(sqlite3.connect(self.work / 'state/group-digests.sqlite3')) as db:
            return [json.loads(row[0]) for row in db.execute('SELECT payload FROM digests ORDER BY to_seq')]

    def test_group_messages_are_digested_incrementally_on_demand_and_survive_restart(self):
        quiet = [self.observation(f'chat-{i}', text) for i, text in enumerate(['周末开黑吗', '我周六有空', '那就周六晚上'])]
        self.run_eve([*quiet, *self.checkpoint('quiet-1'), *self.ask('ask-1', '刚才聊了啥')],
                     checkpoints={'quiet-1': self.quiet})
        self.assertEqual(self.batches, [{'previous': None, 'texts': ['周末开黑吗', '我周六有空', '那就周六晚上', '刚才聊了啥']}])
        self.assertIn('整理周末开黑吗+我周六有空+那就周六晚上+刚才聊了啥',
                      json.dumps(self.receipt('ask-1'), ensure_ascii=False))

        # 第二次只整理新增消息，并把上一段摘要交给整理模型衔接。
        self.run_eve([self.observation('chat-3', '记得带耳机'), *self.checkpoint('quiet-2'), *self.ask('ask-2', '又聊了啥')],
                     checkpoints={'quiet-2': self.quiet})
        self.assertEqual(len(self.batches), 2)
        # Eve 自己送达的回复也在群观察里，作为 me=true 的消息一起整理。
        first_reply = '回顾 整理周末开黑吗+我周六有空+那就周六晚上+刚才聊了啥'
        self.assertEqual(self.batches[1]['texts'], [first_reply, '记得带耳机', '又聊了啥'])
        self.assertTrue(self.batches[1]['previous'].startswith('整理周末开黑吗'))

        # 重启后游标仍在，没有新消息之外的重复整理。
        self.run_eve([*self.ask('ask-3', '还有吗')])
        second_reply = '回顾 整理' + '+'.join(self.batches[1]['texts'])
        self.assertEqual([b['texts'] for b in self.batches[2:]], [[second_reply, '还有吗']])
        digests = self.digests()
        self.assertEqual([d['messages'] for d in digests], [4, 3, 2])
        self.assertTrue(all(a['to_seq'] < b['from_seq'] for a, b in zip(digests, digests[1:])))

        # 私聊没有群来源，工具拒绝，不读取群记录。
        before = len(self.batches)
        self.run_eve([*self.send(self.message('private', '群里聊了啥', expected='私聊看不到群记录'))])
        self.assertEqual(len(self.batches), before)


if __name__ == '__main__':
    unittest.main()
