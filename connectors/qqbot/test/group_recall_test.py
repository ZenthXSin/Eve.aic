"""真实 QQ 宿主进程：最近 20 条直接注入，当前群原话按需召回。

Jev、主模型和 QQ 均为本地替身，不向真实群发测试消息。
"""
import json
import unittest

import group_relation_test
from dialogue_tools_test import text_response, tool_response


def group_context(body):
    for message in body['messages']:
        content = message.get('content')
        if isinstance(content, str) and '当前群的真实消息观察摘录' in content:
            start = content.find('{"current_message"')
            if start < 0:
                start = content.index('{"group_messages"')
            return json.loads(content[start:])
    return None


class GroupRecallAcceptance(group_relation_test.GroupRelationAcceptance):
    for _name in dir(group_relation_test.GroupRelationAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.contexts = []
        self.callback = lambda body: text_response('收到')

    def stored(self, count):
        return lambda: self.wait_for(lambda: len(self.rows('observations')) >= count,
                                     '群消息没有及时写入观察库', 20)

    def ask(self, id, text, expected='收到', **options):
        user = options.pop('user', 'member-lookup')
        return [self.observation(id, text, user=user, mentioned=True),
                *self.send(self.message(id, text, expected=expected, scope='group', user=user, **options))]

    def test_exact_last_twenty_are_injected_without_jev_even_when_long(self):
        def respond(body):
            self.contexts.append(group_context(body))
            return text_response('收到')
        self.callback = respond
        long = '群消息长正文' * 110
        old = [self.observation(f'chat-{i}', f'第{i}句 ' + long, user=f'member-{i}') for i in range(25)]
        self.run_eve([*old, *self.checkpoint('stored'), *self.ask('ask', 'eve看看最近消息')],
                     checkpoints={'stored': self.stored(25)})
        [context] = self.contexts
        messages = context['group_messages']
        self.assertEqual(len(messages), 20)
        self.assertEqual([m['text'].split()[0] for m in messages[:-1]], [f'第{i}句' for i in range(6, 25)])
        self.assertTrue(all(m['truncated'] for m in messages[:-1]))
        self.assertEqual(messages[-1]['text'], 'eve看看最近消息')
        self.assertTrue(messages[-1]['current'])
        self.assertTrue(context['current_message']['text'], '当前消息锚点为空')
        self.assertEqual(context['current_message']['text'], 'eve看看最近消息')
        self.assertIn('mentions_me', messages[-1])
        self.assertIn('replies_to_me', messages[-1])
        self.assertIn('at_ms', messages[0])
        self.assertNotIn('related_earlier', context)
        self.assertEqual(self.recall_requests, [], '回答前不能再调用 Jev 挑原话')

    def test_jev_failure_does_not_remove_recent_context_and_private_has_none(self):
        self.jev_status = 503
        def respond(body):
            self.contexts.append(group_context(body))
            return text_response('收到')
        self.callback = respond
        self.run_eve([self.observation('context', '服务器地址 mdt.example.com'),
                      *self.checkpoint('stored'), *self.ask('ask', 'eve刚才的地址'),
                      *self.send(self.message('private', '你好', expected='收到'))],
                     checkpoints={'stored': self.stored(1)})
        self.assertIn('服务器地址 mdt.example.com', json.dumps(self.contexts[0], ensure_ascii=False))
        self.assertIsNone(self.contexts[1])
        self.assertEqual(self.recall_requests, [])

    def test_member_recalls_beyond_recent_hundred_and_pages_across_restart(self):
        old = [self.observation('addr', '服务器地址 mdt.example.com')]
        old += [self.observation(f'chat-{i}', f'闲聊第{i}句') for i in range(105)]
        old += [self.observation('foreign', '服务器地址 FOREIGN_SECRET', target='group-2')]
        self.run_eve([*old, *self.checkpoint('stored')], checkpoints={'stored': self.stored(107)})
        captured = []
        def respond(body):
            result = self.results(body)
            if not result:
                context = group_context(body)
                self.assertNotIn('mdt.example.com', json.dumps(context))
                return tool_response([('recall_group_messages', {'query': '服务器地址 ', 'limit': 1})])
            captured.append(result[-1])
            self.assertEqual([m['text'] for m in result[-1]['messages']], ['服务器地址 mdt.example.com'])
            self.assertFalse(result[-1]['has_more'])
            self.assertNotIn('FOREIGN_SECRET', json.dumps(body))
            return text_response('地址 mdt.example.com')
        self.callback = respond
        self.run_eve(self.ask('ask', 'eve查更早的服务器地址', expected='地址 mdt.example.com'))
        self.assertEqual(len(captured), 1)
        self.assertEqual(self.recall_requests, [])
        pages = []
        def page(body):
            results = self.results(body)
            if not results:
                return tool_response([('recall_group_messages', {'query': '闲聊第', 'limit': 2})])
            if len(results) == 1:
                first = results[0]
                self.assertTrue(first['has_more'])
                response = tool_response([('recall_group_messages', {'query': '闲聊第', 'limit': 2,
                                          'before_seq': first['next_before_seq']})])
                response['choices'][0]['message']['tool_calls'][0]['id'] += '-page2'
                return response
            pages.extend(results)
            return text_response('翻页完成')
        self.callback = page
        self.run_eve(self.ask('pages', 'eve往前翻', expected='翻页完成'))
        self.assertEqual([[m['text'] for m in p['messages']] for p in pages],
                         [['闲聊第104句', '闲聊第103句'], ['闲聊第102句', '闲聊第101句']])
        self.assertEqual(self.recall_requests, [])

    def test_private_and_foreign_scope_arguments_cannot_read_group(self):
        def respond(body):
            results = self.results(body)
            if not results:
                return tool_response([('recall_group_messages', {})])
            self.assertIn('当前不是群聊', json.dumps(results, ensure_ascii=False))
            self.assertNotIn('GROUP_SECRET', json.dumps(results))
            return text_response('私聊不能读群')
        self.callback = respond
        self.run_eve([self.observation('secret', 'GROUP_SECRET'), *self.checkpoint('stored'),
                      *self.send(self.message('private', '查看群记录', expected='私聊不能读群'))],
                     checkpoints={'stored': self.stored(1)})


if __name__ == '__main__':
    unittest.main()
