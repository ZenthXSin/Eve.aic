"""实际 QQ 宿主进程：Eve 在群里被叫到时，附最近 5 条原话，并由 Jev 从更早的全量群消息里挑出相关原话。

Jev、主模型与 QQ 都是本地替身；挑选质量需用真实 Jev 另行评估。
"""
import json
import unittest

import group_relation_test
from dialogue_tools_test import text_response


def group_context(body):
    for message in body['messages']:
        content = message.get('content')
        if isinstance(content, str) and '当前群的真实消息观察摘录' in content:
            return json.loads(content[content.index('{"group_messages"'):])
    return None


class GroupRecallAcceptance(group_relation_test.GroupRelationAcceptance):
    for _name in dir(group_relation_test.GroupRelationAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.contexts = []

        def respond(body):
            context = group_context(body)
            if context is not None:
                self.contexts.append(context)
            related = (context or {}).get('related_earlier', [])
            if any('mdt.example.com' in m['text'] for m in related):
                return text_response('地址是 mdt.example.com')
            return text_response('没翻到前面的地址')
        self.callback = respond

    def ask(self, id, text):
        return [self.observation(id, text, mentioned=True),
                *self.send(self.message(id, text, contains=['地址'], scope='group'))]

    def test_mentioned_turn_gets_recent_five_and_jev_selected_earlier_messages(self):
        self.recall_score = lambda c: 0.95 if '服务器地址' in c['text'] else 0.1
        old = [self.observation('addr', '服务器地址是 mdt.example.com 记一下')]
        filler = [self.observation(f'chat-{i}', f'闲聊第{i}句') for i in range(8)]
        self.run_eve([*old, *filler, *self.checkpoint('quiet-1'), *self.ask('ask-1', '刚才那个服务器地址是多少')],
                     checkpoints={'quiet-1': self.quiet})
        [request] = self.recall_requests
        state = request['state']
        self.assertEqual(state['current']['text'], '刚才那个服务器地址是多少')
        self.assertEqual(len(state['recent']), 5)
        candidates = [c['text'] for c in state['candidates']]
        self.assertIn('服务器地址是 mdt.example.com 记一下', candidates)
        self.assertNotIn('刚才那个服务器地址是多少', candidates)
        self.assertFalse(set(candidates) & {m['text'] for m in state['recent']})
        [context] = self.contexts
        self.assertEqual(len(context['group_messages']), 5)
        self.assertEqual([m['text'] for m in context['related_earlier']], ['服务器地址是 mdt.example.com 记一下'])
        self.assertIn('地址是 mdt.example.com', json.dumps(self.receipt('ask-1'), ensure_ascii=False))

    def test_jev_failure_keeps_recent_messages_only(self):
        self.jev_status = 503
        filler = [self.observation(f'chat-{i}', f'闲聊第{i}句') for i in range(8)]
        self.run_eve([*filler, *self.checkpoint('quiet-1'), *self.ask('ask-1', '地址发一下')],
                     checkpoints={'quiet-1': self.quiet})
        self.assertEqual(len(self.recall_requests), 1)
        [context] = self.contexts
        self.assertEqual(len(context['group_messages']), 5)
        self.assertNotIn('related_earlier', context)
        self.assertIn('没翻到前面的地址', json.dumps(self.receipt('ask-1'), ensure_ascii=False))


if __name__ == '__main__':
    unittest.main()
