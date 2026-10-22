"""真实宿主的连发合并：同一人连发的纯文字在窗口内合成一轮，回复挂在最后一条上。"""
import json
import unittest

import dialogue_tools_test as dialogue
import memory_test


class DebounceAcceptance(dialogue.DialogueToolsAcceptance):
    for name in dir(dialogue.DialogueToolsAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name
    receipt = memory_test.MemoryAcceptance.receipt

    def run_debounce(self, script, window='1500', **options):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                     env_extra={'EVE_DEBOUNCE_MS': window}, **options)

    def chat_inputs(self):
        return [r['body']['messages'][-1]['content'] for r in self.requests if r['kind'] == 'chat']

    def ledger(self):
        return self.documents()['eve.channel.qqbot']['receipts.v1']

    def test_burst_of_plain_messages_becomes_one_turn_answered_on_the_last(self):
        self.callback = lambda body: dialogue.text_response('一起回答')
        first = self.message('b1', '我想问个事') | {'expected_type': 'finish'}
        second = self.message('b2', '就是上周说的那个', expected='一起回答')
        self.run_debounce([{'send': first}, {'send': second},
                           {'wait_command': {'id': 'b1', 'type': 'finish'}}, self.wait_reply('b2'),
                           self.wait_receipt('b2')])
        self.assertEqual(self.chat_inputs(), ['我想问个事\n就是上周说的那个'])
        merged = self.receipt('b1')
        self.assertEqual((merged['state'], merged['merged_into']), ('Merged', 'b2'))
        self.assertNotIn('reply', {k for k, v in merged.items() if v is not None})
        self.assertEqual(self.receipt('b2')['state'], 'Sent')
        self.assertEqual(self.ledger()['version'], 4)
        turns = [t for s in self.documents()['eve.session']['sessions.v1']['sessions'].values() for t in s['turns']]
        self.assertEqual([t['input'] for t in turns], ['我想问个事\n就是上周说的那个'])
        # 重启后合并记录保持原样，不补发、不重放。
        before = self.receipt('b1')
        self.callback = lambda body: dialogue.text_response('重启正常')
        self.run_debounce(self.send(self.message('later', '重启后', expected='重启正常')))
        self.assertEqual(self.receipt('b1'), before)
        self.assertEqual(len(self.chat_inputs()), 2)

    def test_disabled_window_and_other_users_stay_separate_turns(self):
        self.callback = lambda body: dialogue.text_response('各自回答')
        self.run_debounce([{'send': self.message('s1', '第一句', expected='各自回答')},
                           {'send': self.message('s2', '第二句', expected='各自回答')},
                           self.wait_reply('s1'), self.wait_reply('s2'), self.wait_receipt('s2')], window='0')
        self.assertEqual(self.chat_inputs(), ['第一句', '第二句'])
        self.assertEqual(self.receipt('s1')['state'], 'Sent')
        self.requests.clear()
        self.run_debounce([{'send': self.message('u1', '用户一说话', expected='各自回答')},
                           {'send': self.message('u2', '用户二说话', expected='各自回答', user='user-2')},
                           self.wait_reply('u1'), self.wait_reply('u2'), self.wait_receipt('u2')])
        self.assertEqual(sorted(self.chat_inputs()), ['用户一说话', '用户二说话'])

    def test_attachment_breaks_the_burst(self):
        self.callback = lambda body: dialogue.text_response('收到')
        image = {**self.message('img', '看图', expected='收到'),
                 'attachments': [{'content_type': 'image/png', 'url': 'https://media.example.invalid/a.png'}]}
        self.run_debounce([{'send': self.message('t1', '先说一句', expected='收到')}, {'send': image},
                           self.wait_reply('t1'), self.wait_reply('img'), self.wait_receipt('img')])
        inputs = self.chat_inputs()
        self.assertEqual(len(inputs), 2)
        self.assertEqual(inputs[0], '先说一句')
        self.assertEqual(self.receipt('t1')['state'], 'Sent')


if __name__ == '__main__':
    unittest.main()
