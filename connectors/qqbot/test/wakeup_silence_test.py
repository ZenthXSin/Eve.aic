"""真实 Eve 进程：本轮唤醒静默一次结算，原工具回执与旧正文在重启后保持。"""
import json
import sys
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


PREFIX = '【宿主条件触发事件；不是用户新消息】'
INITIAL_SILENCE = '{"action":"silence"}'
RAW_SILENCE = ' \n{"action": "silence"}\t'


class WakeupSilenceAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.marker = self.source / 'wake-executions.txt'
        self.wake_result = None

    def triggers(self):
        return self.documents().get('eve.trigger', {}).get('triggers.v1', {}).get('triggers', [])

    @staticmethod
    def text(body):
        return next(message['content'] for message in reversed(body['messages'])
                    if message['role'] == 'user')

    def run_wake(self, script, **options):
        return self.run_live(script, more=['--triggers', '--max-tool-rounds', '5'],
                             env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config)}, **options)

    def configure_wake(self, result):
        program = ("from pathlib import Path; p=Path('wake-executions.txt'); "
                   "p.write_text(p.read_text()+'x' if p.exists() else 'x'); "
                   "print('wake-command-complete')")

        def respond(body):
            text = self.text(body)
            results = self.results(body)
            if text.startswith(PREFIX):
                self.assertNotIn('send_message', self.definitions(body),
                                 '委派唤醒只使用本次真实工具清单')
                self.assertIn('本轮是否表达由你决定', text)
                self.assertNotIn('不超过500字', text)
                if not results:
                    return self.choose('run_command', {'program': sys.executable,
                                                        'args': ['-c', program]})
                self.assertEqual(len(results), 1)
                self.assertTrue(results[0]['success'], results[0])
                self.assertIn('wake-command-complete', results[0]['stdout'])
                self.wake_result = result(body) if callable(result) else result
                return dialogue.text_response(self.wake_result)
            self.assertEqual(text, '稍后检查，没有需要说的就保持安静')
            if not results:
                return self.choose('set_wakeup_trigger', {
                    'title': '稍后核查', 'instructions': '实际核查后自行决定是否表达',
                    'conditions': [{'type': 'time', 'after_seconds': 1}]})
            self.assertEqual(len(results), 1)
            self.assertTrue(results[0]['created'], results[0])
            return dialogue.text_response(INITIAL_SILENCE)

        self.callback = respond

    def create_and_wait(self, delivery):
        def finished():
            self.wait_until(lambda: any(trigger['status'] == 'Completed' and
                                       trigger['delivery'] == delivery for trigger in self.triggers()),
                            '唤醒未完成并结算为 ' + delivery)
        summary = self.run_wake([
            *self.send(self.message('create-wake', '稍后检查，没有需要说的就保持安静')),
            *self.checkpoint('wake-finished'), {'delay_ms': 2300}],
            checkpoints={'wake-finished': finished})
        [trigger] = self.triggers()
        self.assertEqual(trigger['runs'], 1)
        self.assertEqual(trigger['result'], self.wake_result)
        self.assertEqual(self.marker.read_text(), 'x', '实际命令只能执行一次')
        turns = [turn for session in self.documents()['eve.session']['sessions.v1']['sessions'].values()
                 for turn in session['turns'] if turn['input'].startswith(PREFIX)]
        self.assertEqual(len(turns), 1)
        turn = turns[0]
        self.assertEqual(turn['status']['messages'][-1]['text'], self.wake_result)
        messages = turn['status']['messages']
        calls = [call for message in messages for call in message.get('tool_calls', [])]
        receipts = [receipt for message in messages for receipt in message.get('tool_results', [])]
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]['name'], 'run_command')
        self.assertEqual(len(receipts), 1)
        self.assertEqual(receipts[0]['call_id'], calls[0]['id'])
        self.assertTrue(receipts[0]['output']['Success']['success'])
        self.assertIn('wake-command-complete', receipts[0]['output']['Success']['stdout'])
        return summary, trigger, turn

    def restart_without_input(self, trigger, turn):
        before = len(self.requests)
        speech = len(self.speech_requests)
        rows = self.speech_rows()
        self.run_wake([{'delay_ms': 2300}])
        self.assertEqual(len(self.requests), before, '完成的唤醒不得在重启后重新请求模型')
        self.assertEqual(len(self.speech_requests), speech)
        self.assertEqual(self.speech_rows(), rows)
        self.assertEqual(self.triggers(), [trigger])
        self.assertEqual(self.turn(turn['input']), turn, '原始工具调用和回执必须逐字段保持')
        self.assertEqual(self.marker.read_text(), 'x', '重启不得重放已经执行的命令')
        self.assert_no_expression()

    def assert_no_expression(self):
        self.assertFalse([event for event in self.events if event.get('direction') == 'out' and
                          event.get('type') in ('push', 'progress', 'reply', 'segment', 'sticker',
                                                'send_file', 'push_file')])

    def test_committed_silence_and_receipts_survive_restart_without_speech_or_replay(self):
        self.configure_wake(RAW_SILENCE)
        summary, trigger, turn = self.create_and_wait('Silenced')
        self.assertEqual(summary['sent'], 0)
        self.assertEqual(self.speech_requests, [], '本轮明确静默不能再向发言模型征求一次决定')
        self.assertEqual(self.speech_rows(), [], '静默完成不进入发言队列')
        self.assert_no_expression()
        self.restart_without_input(trigger, turn)

    def ordinary_result_is_reviewed_once(self, result):
        self.configure_wake(result)
        expressed = '已看完实际结果，这条需要告诉你'

        def decide(facts):
            self.assertEqual(facts['host_event']['kind'], 'wakeup_result')
            self.assertEqual(facts['host_event']['result'], self.wake_result)
            return {'action': 'speak', 'text': expressed, 'after_ms': 0}

        self.speech_callback = decide
        _, trigger, turn = self.create_and_wait('Sent')
        self.assertEqual(len(self.speech_requests), 1, '相似正文仍需由 Eve 自己决定是否表达')
        [push] = [event for event in self.events if event.get('direction') == 'out' and
                  event.get('type') == 'push']
        self.assertEqual(push['text'], expressed)
        self.assertEqual(push['id'], trigger['id'] + ':run-1')
        [event] = [event for event, state, _ in self.speech_rows() if state == 'sent']
        self.assertEqual(event['facts']['result'], self.wake_result)
        self.restart_without_input(trigger, turn)

    def test_replayed_internal_wrapper_is_preserved_and_passed_to_speech(self):
        def replay(body):
            records = [message['content'] for message in body['messages']
                       if message['role'] == 'assistant' and
                       (message.get('content') or '').startswith('【宿主内部记录，未发送】')]
            [record] = [record for record in records
                        if json.loads(record.split('\n', 1)[1]).get('text') == INITIAL_SILENCE]
            return record
        self.ordinary_result_is_reviewed_once(replay)

    def test_silence_substring_in_business_result_is_preserved_and_passed_to_speech(self):
        self.ordinary_result_is_reviewed_once(
            '配置样例包含 {"action":"silence"}；实际检查结果仍需完整说明。')


if __name__ == '__main__':
    unittest.main()
