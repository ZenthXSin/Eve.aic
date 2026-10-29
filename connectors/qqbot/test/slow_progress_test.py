"""真实宿主的慢步骤进度：等待模型或一批工具超过阈值仍未结束时补发一条真实进度。"""
import json
import sys
import time
import unittest

import progress_narration_test as progress
import dialogue_tools_test as dialogue
import memory_test


class SlowProgressAcceptance(progress.ProgressNarrationAcceptance):
    for name in dir(progress.ProgressNarrationAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name
    receipt = memory_test.MemoryAcceptance.receipt

    def run_slow(self, script, after_ms='1000', **options):
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config), 'EVE_PROGRESS_SLOW_AFTER_MS': after_ms,
               'EVE_CONTEXT_COMPACT_AT_TOKENS': '0', 'EVE_PROGRESS_STAGE_UPDATES': 'false'}
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     env_extra=env, timeout=60, **options)
        path = self.work / f'events-{self.runs}.jsonl'
        self.events = [json.loads(line) for line in path.read_text().splitlines()]

    def progress(self, id):
        return [e['text'] for e in self.events if e.get('type') == 'progress' and e.get('id') == id]

    def test_slow_model_step_gets_one_truthful_progress_before_the_final_reply(self):
        def respond(body):
            time.sleep(2.5)
            return dialogue.text_response('慢回复完成')
        self.callback = respond
        self.run_slow(self.send(self.message('slow', '慢慢想', expected='慢回复完成')))
        [text] = self.progress('slow')
        self.assertEqual(text, '这一步还在等待结果，我会继续核对。')
        self.assertTrue(any(f['waiting_seconds'] is not None for f in self.narrations))
        receipt = self.receipt('slow')
        self.assertEqual(receipt['state'], 'Sent')
        self.assertEqual([p['state'] for p in receipt['progress']], ['Sent'])
        order = [e.get('type') for e in self.events if e.get('id') == 'slow' and e.get('type') in ('progress', 'reply')]
        self.assertEqual(order, ['progress', 'reply'])

    def test_slow_tool_batch_names_the_activity_and_shares_the_three_progress_cap(self):
        def respond(body):
            if not self.results(body):
                return dialogue.tool_response([('run_command', {'program': sys.executable,
                                                                'args': ['-c', 'import time; time.sleep(2.5)']})])
            return dialogue.text_response('命令完成')
        self.callback = respond
        self.run_slow(self.send(self.message('tool', '跑个慢命令', expected='命令完成')))
        texts = self.progress('tool')
        self.assertTrue(any(t == '这一步还在等待结果，我会继续核对。' for t in texts), texts)
        self.assertLessEqual(len(texts), 3)
        self.assertTrue(all(p['state'] == 'Sent' for p in self.receipt('tool')['progress']))

    def test_fast_turns_and_disabled_threshold_send_no_slow_progress(self):
        self.callback = lambda body: dialogue.text_response('很快')
        self.run_slow(self.send(self.message('fast', '快问', expected='很快')))
        self.assertEqual(self.progress('fast'), [])
        def slow(body):
            time.sleep(2)
            return dialogue.text_response('关闭后')
        self.callback = slow
        self.run_slow(self.send(self.message('off', '关闭阈值', expected='关闭后')), after_ms='0')
        self.assertEqual(self.progress('off'), [])
        self.assertNotIn('progress', self.receipt('off'))

    def test_multiple_slow_steps_notify_only_once_per_turn(self):
        def respond(body):
            if len(self.results(body)) < 2:
                response = dialogue.tool_response([('run_command', {'program': sys.executable,
                    'args': ['-c', 'import time; time.sleep(1.6)']})])
                response['choices'][0]['message']['tool_calls'][0]['id'] += str(len(self.results(body)))
                return response
            return dialogue.text_response('两步完成')
        self.callback = respond
        self.run_slow(self.send(self.message('twice', '跑两步', expected='两步完成')))
        self.assertEqual(len(self.progress('twice')), 1)
        self.assertEqual(self.receipt('twice')['state'], 'Sent')

    def test_cancel_before_threshold_suppresses_the_pending_slow_progress(self):
        self.response_gates[('chat', 1)] = 'never-release'
        self.callback = lambda body: dialogue.text_response('不应送达')
        old = self.message('old', '等一个永远不回的模型') | {'expected_type': 'finish'}
        self.run_slow([{'send': old}, {'wait_file': str(self.arrived('chat', 1))},
                       *self.send(self.message('cancel', '/cancel', contains='已取消')),
                       {'wait_command': {'id': 'old', 'type': 'finish'}},
                       *self.checkpoint('after-threshold')],
                      after_ms='1500', checkpoints={'after-threshold': lambda: time.sleep(2.5)})
        self.assertEqual(self.progress('old'), [])
        self.assertEqual(self.receipt('old')['state'], 'Failed')


if __name__ == '__main__':
    unittest.main()
