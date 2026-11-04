"""实际宿主：Eve 自主选择阶段表达；长工具没有自动心跳，最终内容不会自动发送。"""
import json
import sys
import time
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


class StageFeedbackAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def run_stage(self, script, **options):
        cfg = json.loads(self.developer_config.read_text())
        cfg['max_tool_rounds'] = 64
        self.developer_config.write_text(json.dumps(cfg))
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config),
               'EVE_PROGRESS_STAGE_UPDATES': 'true', 'EVE_PROGRESS_MIN_INTERVAL_MS': '1000',
               'EVE_PROGRESS_SLOW_AFTER_MS': '1000', 'EVE_CONTEXT_COMPACT_AT_TOKENS': '0'}
        env.update(options.pop('env_extra', {}))
        return self.run_live(script, env_extra=env, **options)

    def test_six_chosen_messages_have_receipts_and_no_extra_final_or_restart_replay(self):
        def respond(body):
            done = len(self.all_results(body))
            if done == 6:
                self.assertTrue(all(r['sent'] for r in self.all_results(body)))
                return dialogue.text_response('内部结束，不补第七句')
            return self.choose('send_message', {'text': f'核对所得的第 {done+1} 条发现',
                                               'after_ms': 1100 if done > 1 else 0})
        self.callback = respond
        self.run_stage(self.send(self.message('stages', '核对这些发现')))
        self.assertEqual(len(self.progress('stages')), 6)
        self.assertEqual([p.get('proactive', False) for p in self.progress('stages')],
                         [False, False, True, True, True, True])
        before = self.receipt('stages')
        self.assertEqual(before['state'], 'Handled')
        self.assertTrue(all(p['state'] == 'Sent' and p['platform_message_id'] for p in before['progress']))
        self.assertEqual(self.documents()['eve.channel.qqbot']['receipts.v1']['version'], 9)
        self.callback = lambda _: dialogue.text_response('无需新消息')
        self.run_stage(self.send(self.message('after', '重启后继续想想，先别回')))
        self.assertEqual(self.receipt('stages'), before)
        self.assertFalse(self.progress('stages'))
        self.assertFalse(self.progress('after'))

    def test_group_expression_respects_platform_quota_without_generating_an_overflow_message(self):
        def respond(body):
            done = len(self.all_results(body))
            if done == 4:
                self.assertEqual([r['sent'] for r in self.all_results(body)], [True, True, True, False])
                return dialogue.text_response('内部结束，超额不补发')
            return self.choose('send_message', {'text': f'群里独立的第 {done+1} 条表达',
                'purpose': 'question' if done == 2 else 'conversation'})
        self.callback = respond
        self.run_stage(self.send(self.message('group-stages', 'eve 说说这些发现', scope='group')))
        sent = self.progress('group-stages')
        self.assertEqual(len(sent), 3)
        self.assertTrue(all(not p.get('proactive', False) for p in sent))
        self.assertEqual([p.get('necessary', False) for p in self.receipt('group-stages')['progress']],
                         [False, False, True])

    def test_long_command_runs_without_automatic_start_wait_or_finish_messages(self):
        marker = self.source / 'long-started'
        release = self.source / 'release-waiting'
        def respond(body):
            results = self.all_results(body)
            if not results:
                return self.choose('run_command', {'program': sys.executable,
                    'args': ['-c', "from pathlib import Path; import time\nPath('long-started').touch()\nwhile not Path('release-waiting').exists(): time.sleep(.01)\nprint('verified')"]})
            if len(results) == 1:
                self.assertIn('verified', results[0]['stdout'])
                return self.choose('send_message', {'text': '检查结果拿到了，这一步通过了'})
            return dialogue.text_response('内部结束')
        def while_waiting():
            self.wait_until(marker.exists, '实际命令没有启动')
            time.sleep(2.2)
            self.assertFalse(self.narrations)
            self.assertFalse(self.receipt('long').get('progress'))
            release.touch()
        self.callback = respond
        self.run_stage([{'send': self.message('long', '做一次检查，过程中不用播报')},
                        *self.checkpoint('waiting'), self.wait_receipt('long')],
                       checkpoints={'waiting': while_waiting})
        self.assertEqual([p['text'] for p in self.progress('long')], ['检查结果拿到了，这一步通过了'])

    def test_failed_expression_does_not_stop_work_or_generate_a_fallback(self):
        def respond(body):
            results = self.all_results(body)
            if not results:
                return self.choose('send_message', {'text': '先看这个文件'})
            if len(results) == 1:
                self.assertFalse(results[0]['sent'])
                return self.choose('echo', {'text': '实际执行的结果'})
            self.assertEqual(results[-1], {'echo': '实际执行的结果'})
            return dialogue.text_response('内部完成，未选择再说')
        self.callback = respond
        self.run_stage(self.send(self.message('failed', '看看这个') | {'fail_progress': True}))
        self.assertEqual(len(self.progress('failed')), 1)
        self.assertEqual(self.receipt('failed')['progress'][0]['state'], 'Failed')
        self.assertEqual(self.receipt('failed')['state'], 'Handled')

    def test_cancel_can_be_silent_without_later_automatic_waiting_or_final(self):
        self.response_gates[('chat', 2)] = 'never-release'
        self.callback = lambda _: self.choose('send_message', {'text': '先查这条线索'})
        self.run_stage([{'send': self.message('cancelled', '查一下')},
            self.wait_command('cancelled', 'progress'), {'wait_file': str(self.arrived('chat', 2))},
            *self.send(self.message('cancel', '/cancel')), self.wait_command('cancelled', 'finish'),
            {'delay_ms': 300}])
        self.assertEqual(len(self.progress('cancelled')), 1)
        self.assertEqual(self.receipt('cancelled')['state'], 'Failed')
        self.assertFalse([e for e in self.events if e.get('type') == 'push'])


if __name__ == '__main__':
    unittest.main()
