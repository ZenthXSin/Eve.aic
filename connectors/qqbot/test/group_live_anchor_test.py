"""真实 Eve 子进程的群回复锚点、失败原因和重启；模型与 JSONL 桥均为本地替身。"""
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


class GroupLiveAnchorAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def message(self, *args, **kwargs):
        kwargs.setdefault('scope', 'group')
        return super().message(*args, **kwargs)

    def finish_frame(self, message_id):
        [frame] = [event for event in self.events if event.get('direction') == 'out'
                   and event.get('type') == 'finish' and event.get('id') == message_id]
        return frame

    def assert_retained_window(self, child, parent):
        frame = self.finish_frame(child)
        self.assertIs(frame.get('keep_live_reply'), True)
        self.assertEqual(frame.get('parent_id'), parent)
        self.assertNotIn('keep_foreground', frame)
        receipt = self.receipt(child)
        self.assertEqual(receipt['state'], 'Merged')
        self.assertEqual(receipt['merged_into'], parent)
        self.assertFalse(receipt.get('progress'))

    def test_each_merged_input_has_its_own_three_slots_and_restart_preserves_receipts(self):
        def respond(body):
            step = self.chat_rounds
            results = self.all_results(body)
            if step <= 4:
                return self.choose('send_message', {'text': f'原消息窗口第{step}条'})
            if step == 5:
                self.assertEqual([item['sent'] for item in results], [True, True, True, False])
                self.assertEqual(results[-1]['failure_kind'], 'passive_quota_exhausted')
                self.gate('parent-quota-known').touch()
                return self.choose('wait_for_input', {'timeout_ms': 10_000})
            if 6 <= step <= 9:
                if step == 6:
                    self.assertTrue(results[-1]['new_input'])
                    self.assertIn('接着说第一部分', self.users(body))
                return self.choose('send_message', {'text': f'第一条新消息窗口第{step - 5}条'})
            if step == 10:
                self.assertFalse(results[-1]['sent'])
                self.assertEqual(results[-1]['failure_kind'], 'passive_quota_exhausted')
                self.gate('child-quota-known').touch()
                return self.choose('wait_for_input', {'timeout_ms': 10_000})
            if step == 11:
                self.assertTrue(results[-1]['new_input'])
                self.assertIn('接着说第二部分', self.users(body))
                return self.choose('send_message', {'text': '第二条新消息窗口的结果'})
            self.assertEqual(step, 12)
            sends = [item for item in results if 'sent' in item]
            self.assertEqual([item['sent'] for item in sends],
                             [True, True, True, False, True, True, True, False, True])
            return dialogue.text_response('内部处理完成')

        self.callback = respond
        parent = self.message('quota-parent', '分几次说明')
        first = self.message('quota-child-1', '接着说第一部分')
        second = self.message('quota-child-2', '接着说第二部分')
        summary = self.run_live([
            {'send': parent}, self.wait_command(parent['id'], 'progress', 3),
            {'wait_file': str(self.gate('parent-quota-known'))}, {'delay_ms': 150},
            {'send': first}, self.wait_receipt(first['id'], 'Merged'),
            self.wait_command(first['id'], 'finish'), self.wait_command(parent['id'], 'progress', 6),
            {'wait_file': str(self.gate('child-quota-known'))}, {'delay_ms': 150},
            {'send': second}, self.wait_receipt(second['id'], 'Merged'),
            self.wait_command(second['id'], 'finish'), self.wait_receipt(parent['id']),
            self.wait_command(parent['id'], 'finish'),
        ])
        frames = self.progress(parent['id'])
        self.assertEqual(len(frames), 7)
        self.assertEqual([frame['index'] for frame in frames], list(range(7)))
        self.assertEqual([frame['based_on_message_id'] for frame in frames],
                         [parent['id']] * 3 + [first['id']] * 3 + [second['id']])
        self.assertTrue(all(frame['explicit'] and not frame['proactive'] for frame in frames))
        self.assert_retained_window(first['id'], parent['id'])
        self.assert_retained_window(second['id'], parent['id'])
        parts = self.receipt(parent['id'])['progress']
        self.assertEqual([part.get('reply_anchor') for part in parts],
                         [None] * 3 + [first['id']] * 3 + [second['id']])
        self.assertTrue(all(part['state'] == 'Sent' and part.get('platform_message_id')
                            and not part.get('proactive', False) for part in parts))
        self.assertEqual(summary['sent'], 7)
        self.assertEqual(self.documents()['eve.channel.qqbot']['receipts.v1']['version'], 10)
        turns = next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))['turns']
        self.assertEqual(len(turns), 1)
        self.assertEqual([item['text'] for item in turns[0]['status']['messages']
                          if item['role'] == 'User'],
                         [parent['text'], first['text'], second['text']])

        before = {message['id']: self.receipt(message['id']) for message in (parent, first, second)}
        rounds = self.chat_rounds
        self.callback = lambda _: dialogue.text_response('重启后内部结束')
        self.run_live([
            {'send': parent}, self.wait_command(parent['id'], 'finish'),
            {'send': first}, self.wait_command(first['id'], 'finish'),
            {'send': second}, self.wait_command(second['id'], 'finish'),
            *self.send(self.message('quota-restart', '恢复后保持安静')),
        ])
        self.assertEqual(self.chat_rounds, rounds + 1)
        for message_id, receipt in before.items():
            self.assertEqual(self.receipt(message_id), receipt)
            self.assertFalse(self.progress(message_id), '重启和重复输入不能重放旧发言')

    def same_text_after_new_input(self, diagnostic, status, state, allowed):
        text = '已经核对过的处理结果'

        def respond(body):
            results = self.all_results(body)
            if self.chat_rounds == 1:
                return self.choose('send_message', {'text': text})
            if self.chat_rounds == 2:
                self.assertEqual(results[-1]['delivery_status'], status)
                if status == 'not_sent':
                    self.assertEqual(results[-1]['failure_kind'], diagnostic['failure_kind'])
                return self.choose('send_message', {'text': text})
            if self.chat_rounds == 3:
                self.assertFalse(results[-1]['sent'])
                self.assertEqual(results[-1]['failure_kind'], 'duplicate_text')
                self.gate('waiting-for-new-anchor').touch()
                return self.choose('wait_for_input', {'timeout_ms': 10_000})
            if self.chat_rounds == 4:
                self.assertTrue(results[-1]['new_input'])
                self.assertIn('继续刚才的结果', self.users(body))
                return self.choose('send_message', {'text': text})
            self.assertEqual(self.chat_rounds, 5)
            self.assertEqual(results[-1]['sent'], allowed)
            self.assertEqual(results[-1]['delivery_status'], 'sent' if allowed else 'not_sent')
            if not allowed:
                self.assertEqual(results[-1]['failure_kind'], 'duplicate_text')
            return dialogue.text_response('内部结束，不自动替换正文')

        self.callback = respond
        parent = self.message('same-text-parent', '检查一下这次结果') | {
            'progress_diagnostics': {'0': diagnostic}}
        child = self.message('same-text-child', '继续刚才的结果')
        self.run_live([
            {'send': parent}, self.wait_command(parent['id'], 'progress'),
            {'wait_file': str(self.gate('waiting-for-new-anchor'))}, {'delay_ms': 150},
            {'send': child}, self.wait_receipt(child['id'], 'Merged'),
            self.wait_command(child['id'], 'finish'), self.wait_receipt(parent['id']),
            self.wait_command(parent['id'], 'finish'),
        ])
        self.assert_retained_window(child['id'], parent['id'])
        frames = self.progress(parent['id'])
        self.assertEqual([frame['text'] for frame in frames], [text] * (2 if allowed else 1))
        parts = self.receipt(parent['id'])['progress']
        self.assertEqual(parts[0]['state'], state)
        self.assertNotIn('reply_anchor', parts[0])
        if allowed:
            self.assertEqual([frame['based_on_message_id'] for frame in frames],
                             [parent['id'], child['id']])
            self.assertEqual(parts[1]['state'], 'Sent')
            self.assertEqual(parts[1]['reply_anchor'], child['id'])
            self.assertNotEqual(parts[0]['decision_id'], parts[1]['decision_id'])
            self.assertTrue(parts[1].get('platform_message_id'))
        else:
            self.assertEqual(len(parts), 1)

    def test_expired_window_allows_new_call_with_same_text_only_after_new_group_input(self):
        self.same_text_after_new_input(
            {'ok': False, 'message_id': None, 'failure_kind': 'passive_window_elapsed',
             'elapsed_ms': 240_001}, 'not_sent', 'Failed', True)

    def test_http_failure_does_not_become_safe_to_repeat_in_a_new_window(self):
        self.same_text_after_new_input(
            {'ok': False, 'message_id': None, 'failure_kind': 'http', 'http_status': 500},
            'not_sent', 'Failed', False)

    def test_unknown_delivery_is_not_repeated_in_a_new_window(self):
        self.same_text_after_new_input(
            {'ok': False, 'message_id': None, 'failure_kind': 'timeout'},
            'unknown', 'Unknown', False)

    def test_sent_text_is_not_repeated_in_a_new_window(self):
        self.same_text_after_new_input({}, 'sent', 'Sent', False)

    def test_pending_child_window_delivery_survives_restart_without_replay(self):
        self.response_gates[('chat', 1)] = 'release-old-draft'
        text = '新窗口中尚未收到平台回执的消息'

        def respond(body):
            if self.chat_rounds == 1:
                return dialogue.text_response('新输入到来前的内部草稿')
            self.assertEqual(self.chat_rounds, 2)
            self.assertIn('这条消息继续当前轮', self.users(body))
            return self.choose('send_message', {'text': text})

        self.callback = respond
        parent = self.message('pending-parent', '开始核对') | {
            'progress_diagnostics': {'0': {'hold': True}}}
        child = self.message('pending-child', '这条消息继续当前轮')
        stop = self.gate('stop-after-child-send')
        self.run_live([
            {'send': parent}, {'wait_file': str(self.arrived('chat', 1))},
            {'send': child}, self.wait_receipt(child['id'], 'Merged'),
            self.wait_command(child['id'], 'finish'), {'touch': str(self.gate('release-old-draft'))},
            self.wait_command(parent['id'], 'progress'), {'touch': str(stop)},
            {'wait_file': str(self.gate('never-release'))},
        ], stop_at=stop)
        self.assert_retained_window(child['id'], parent['id'])
        before = self.receipt(parent['id'])
        before_child = self.receipt(child['id'])
        [part] = before['progress']
        self.assertEqual(part['state'], 'Sending')
        self.assertEqual(part['reply_anchor'], child['id'])
        self.assertFalse(part.get('platform_message_id'))
        [frame] = self.progress(parent['id'])
        self.assertEqual(frame['based_on_message_id'], child['id'])
        self.assertFalse(frame['proactive'])

        self.response_gates.clear()

        def resumed(body):
            context = '\n'.join(message.get('content') or '' for message in body['messages']
                                if message['role'] == 'system')
            self.assertIn(text, context)
            self.assertIn('unknown', context)
            return dialogue.text_response('保留未知事实，内部结束')

        self.callback = resumed
        self.run_live(self.send(self.message('pending-restart', '恢复以后先不要发消息')))
        self.assertEqual(self.receipt(parent['id']), before)
        self.assertEqual(self.receipt(child['id']), before_child)
        self.assertFalse(self.progress(parent['id']))
        self.assertFalse(self.progress(child['id']))
        self.assertTrue(any(state == 'unknown' for _, state, _ in self.speech_rows()))


if __name__ == '__main__':
    unittest.main()
