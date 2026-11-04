"""真实 Eve 进程：迁移旧会话和群自主交流均可读取固定源码，QQ/模型仅用本地替身。"""
from contextlib import closing
import json
import sqlite3
import unittest

import dialogue_tools_test as dialogue
import group_chat_test as group
import live_dialogue_test as live


def reference(case):
    root = case.work / 'reference'
    root.mkdir()
    (root / '.git').mkdir()
    (root / '.git/HEAD').write_text('0123456789abcdef0123456789abcdef01234567\n')
    (root / 'PowerGraph.java').write_text('class PowerGraph {\n    public Seq<Building> all = new Seq<>();\n}\n')
    return ['--reference', 'mindustry=' + str(root)]


def legacy_receipt(case):
    message = {'id': 'legacy', 'scope': 'c2c', 'target_id': 'user-1',
               'user_id': 'user-1', 'text': '旧版本的加工正文，不能当成新授权'}
    receipt = {'version': 1, 'entries': [{'app_id': '1904159860', 'message': message,
                                          'state': 'Sent', 'reply': '旧回复'}]}
    state = case.work / 'state'
    state.mkdir(exist_ok=True)
    (state / 'state.json').write_text(json.dumps({'version': 1, 'entries': {
        'eve.channel.qqbot': {'receipts.v1': list(json.dumps(receipt, ensure_ascii=False).encode())}}}))


def lookup(case, body):
    names = {t['function']['name'] for t in body.get('tools', [])}
    case.assertLessEqual({'search_reference', 'read_reference', 'read_source'}, names)
    case.assertFalse({'run_command', 'write_file', 'run_tool_code', 'dispatch_background_task'} & names)
    results = [json.loads(m['content']) for m in body['messages'] if m['role'] == 'tool']
    if not results:
        return dialogue.tool_response([('search_reference', {'root': 'mindustry', 'query': 'Seq<Building>'})])
    if len(results) == 1:
        hit = results[0]['hits'][0]
        response = dialogue.tool_response([('read_reference', {'root': hit['root'], 'path': hit['path'],
                                                               'start_line': hit['line'], 'max_lines': 1})])
        response['choices'][0]['message']['tool_calls'][0]['id'] = 'read-source'
        return response
    case.assertEqual(results[1]['text'], '2:     public Seq<Building> all = new Seq<>();\n')
    return None


class LegacySourceLookupAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def test_first_new_message_in_legacy_scope_can_search_read_and_reply_once(self):
        references = reference(self)
        legacy_receipt(self)

        def respond(body):
            response = lookup(self, body)
            if response is not None:
                return response
            if len(self.all_results(body)) == 2:
                return self.choose('send_message', {'text': '已查到集合 all，见 mindustry/PowerGraph.java:2'})
            return dialogue.text_response('{"action":"silence"}')

        self.callback = respond
        self.run_live(self.send(self.message('lookup', '查一下电网的建筑集合字段')), more=references)
        self.assertEqual([e['text'] for e in self.progress('lookup')],
                         ['已查到集合 all，见 mindustry/PowerGraph.java:2'])
        with closing(sqlite3.connect(self.work / 'state/qq-receipts.sqlite3')) as db:
            self.assertEqual(db.execute('SELECT count(*) FROM qq_originals').fetchone()[0], 1)
        turn = self.turn('查一下电网的建筑集合字段')
        self.assertEqual([c['name'] for m in turn['status']['messages'] for c in m.get('tool_calls', [])],
                         ['search_reference', 'read_reference', 'send_message'])
        before = self.chat_rounds
        self.run_live([{'send': self.message('lookup', '查一下电网的建筑集合字段')},
                       self.wait_command('lookup', 'finish')], more=references)
        self.assertEqual(self.chat_rounds, before)
        self.assertFalse(self.progress('lookup'))

    def test_legacy_scope_can_inspect_its_current_raw_message(self):
        legacy_receipt(self)

        def respond(body):
            self.assertIn('read_raw_message', {t['function']['name'] for t in body['tools']})
            results = self.all_results(body)
            if not results:
                return self.choose('read_raw_message', {})
            if len(results) == 1:
                self.assertIn('raw-check', json.dumps(results[0]))
                return self.choose('send_message', {'text': '已读到这条消息的原始事件'})
            return dialogue.text_response('{"action":"silence"}')

        self.callback = respond
        message = self.message('raw-check', '读取当前原始消息')
        message['raw'] = {'id': 'raw-check', 'content': '读取当前原始消息'}
        self.run_live(self.send(message))
        self.assertEqual([e['text'] for e in self.progress('raw-check')], ['已读到这条消息的原始事件'])


class GroupSourceLookupAcceptance(group.GroupChatAcceptance):
    for name in dir(group.GroupChatAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def test_unmentioned_group_question_can_read_source_before_choosing_its_reply(self):
        references = reference(self)
        original = self.provider_response
        source_requests = []

        def respond(body, kind, fallback):
            # 后续请求末尾是工具回执；仍按本轮用户输入核对群交流身份。
            current = next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')
            if current.startswith('这是 Eve 自己的一次群聊交流决定。'):
                source_requests.append(body)
                response = lookup(self, body)
                return response or dialogue.text_response(json.dumps({
                    'action': 'speak', 'text': '源码里的集合是 all，见 mindustry/PowerGraph.java:2', 'after_ms': 0}, ensure_ascii=False))
            return original(body, kind, fallback)

        self.provider_response = respond
        first = self.observation('group-source', '电网的建筑集合字段是什么，Eve查一下', user='member-2')
        self.run_eve([first, *self.checkpoint('sent')], more=references,
                     checkpoints={'sent': self.wait_outbound})
        [sent] = self.pushes()
        self.assertEqual(sent['text'], '源码里的集合是 all，见 mindustry/PowerGraph.java:2')
        self.assertEqual(sent['based_on_message_id'], 'group-source')
        self.assertEqual(len(source_requests), 3)
        self.assertEqual(len(self.requests), 3)


if __name__ == '__main__':
    unittest.main()
