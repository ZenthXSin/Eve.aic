"""同轮延后回复：真实 Eve 进程、临时故障和 QQ 替身，不发送真实 QQ 消息。"""
import copy
import json
import sys
import time
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


class DeferredReplyAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def run_deferred(self, script, **options):
        environment = {
            'EVE_DEFERRED_REPLY_ENABLED': 'true',
            'EVE_DEFERRED_REPLY_MAX_RETRIES': '3',
            'EVE_DEFERRED_REPLY_INITIAL_DELAY_MS': '40',
            'EVE_DEFERRED_REPLY_MAX_DELAY_MS': '80',
            'EVE_DEFERRED_REPLY_WINDOW_MS': '4000',
            'EVE_OPENAI_TRANSIENT_RETRIES': 'false',
            'EVE_OPENAI_TIMEOUT_SECONDS': '1',
            'EVE_MODELS_PRIMARY_TIMEOUT_MS': '1000',
            'EVE_DEVELOPER_CONFIG': str(self.developer_config),
        }
        environment.update(options.pop('env_extra', {}))
        return self.run_live(script, panel=True, env_extra=environment, **options)

    def test_unavailable_service_then_full_reply_without_acknowledgement(self):
        self.http_status = lambda kind, number: 503 if kind == 'chat' and number <= 2 else 200
        answer = '查证完成，下面是完整结果。\n' + '这里保留逐项说明与依据，不人为缩成占位确认。' * 16
        self.callback = lambda body: self.choose('send_message', {'text': answer}) if not self.all_results(body) else dialogue.text_response('{"action":"silence"}')
        self.run_deferred(self.send(self.message('eventually', '查清以后再回复')))
        self.assertEqual([e['text'] for e in self.progress('eventually')], [answer])
        self.assertEqual(len(self.kind('chat')), 4)
        requests = [r['body'] for r in self.kind('chat')]
        self.assertEqual(requests[0], requests[1])
        self.assertEqual(requests[1], requests[2])
        self.assertEqual(self.turn('查清以后再回复')['status']['state'], 'Completed')
        self.assertIn('EVE_DEFERRED_REPLY', self.panel_stderr.read_text())
        self.assertEqual([(event['facts']['kind'], state) for event, state, _ in self.speech_rows()],
                         [('expression', 'sent')])

    def test_command_result_survives_failure_without_reexecution_or_duplicate_send_after_restart(self):
        retried = []

        def status(kind, _number):
            if kind != 'chat':
                return 200
            body = self.requests[-1]['body']
            if len(self.all_results(body)) == 1:
                retried.append(copy.deepcopy(body))
                return 503 if len(retried) <= 2 else 200
            return 200

        self.http_status = status

        def respond(body):
            results = self.all_results(body)
            if not results:
                return self.choose('run_command', {'program': sys.executable, 'args': ['-c',
                    "from pathlib import Path; p=Path('deferred-once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('已确认的执行结果')"]})
            if len(results) == 1:
                self.assertIn('已确认的执行结果', results[0]['stdout'])
                return self.choose('send_message', {'text': '已经核对结果，命令只执行了一次。'})
            return dialogue.text_response('{"action":"silence"}')

        self.callback = respond
        self.run_deferred(self.send(self.message('command', '执行一次检查，故障恢复后告诉我结果')))
        self.assertEqual((self.source / 'deferred-once.txt').read_text(), 'x')
        self.assertEqual(len(retried), 3)
        self.assertTrue(all(body == retried[0] for body in retried))
        self.assertEqual([e['text'] for e in self.progress('command')], ['已经核对结果，命令只执行了一次。'])
        before = len(self.requests)
        self.run_deferred([{'send': self.message('command', '执行一次检查，故障恢复后告诉我结果')}, self.wait_command('command', 'finish')])
        self.assertEqual(len(self.requests), before)
        self.assertFalse(self.progress('command'))
        self.assertEqual((self.source / 'deferred-once.txt').read_text(), 'x')

    def test_new_input_during_backoff_is_applied_before_the_next_request(self):
        self.http_status = lambda kind, number: 503 if kind == 'chat' and number == 1 else 200

        def respond(body):
            self.assertIn('改为只查询，不执行命令', self.users(body))
            return self.choose('send_message', {'text': '按你补充的要求，只查询。'}) if not self.all_results(body) else dialogue.text_response('{"action":"silence"}')

        self.callback = respond
        # Handled 先持久化，通道随后才写 finish；桥接必须消费收尾帧后再退出。
        self.run_deferred([{'send': self.message('first', '看看这个问题')},
            {'wait_file': str(self.arrived('chat', 1))},
            {'send': self.message('steer', '改为只查询，不执行命令')},
            self.wait_receipt('steer', 'Merged'), self.wait_receipt('first'),
            self.wait_command('first', 'finish')],
            env_extra={'EVE_DEFERRED_REPLY_INITIAL_DELAY_MS': '500', 'EVE_DEFERRED_REPLY_MAX_DELAY_MS': '500'})
        self.assertEqual(len(self.kind('chat')), 3)
        self.assertEqual([e['text'] for e in self.progress('first')], ['按你补充的要求，只查询。'])
        self.assertFalse(self.progress('steer'))
        self.assertEqual([e['id'] for e in self.events
                          if e.get('direction') == 'out' and e.get('type') == 'finish'], ['steer', 'first'])
        turns = next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))['turns']
        self.assertEqual(len(turns), 1)
        self.assertEqual([m['text'] for m in turns[0]['status']['messages'] if m['role'] == 'User'],
                         ['看看这个问题', '改为只查询，不执行命令'])

    def test_outer_model_timeout_recovers_without_delivering_late_response(self):
        def respond(body):
            if self.chat_rounds == 1:
                time.sleep(1.3)
                return self.choose('send_message', {'text': '过期响应不得投递'})
            return self.choose('send_message', {'text': '超时后继续查询的完整结果'}) if not self.all_results(body) else dialogue.text_response('{"action":"silence"}')

        self.callback = respond
        self.run_deferred(self.send(self.message('timeout', '等到查清再回答')))
        self.assertEqual([e['text'] for e in self.progress('timeout')], ['超时后继续查询的完整结果'])
        self.assertEqual(len(self.kind('chat')), 3)
        self.assertEqual(self.turn('等到查清再回答')['status']['state'], 'Completed')

    def test_retry_exhaustion_and_permanent_failure_preserve_failure_without_ack_or_new_task(self):
        self.http_status = lambda kind, number: 503 if kind == 'chat' else 200
        self.run_deferred(self.send(self.message('exhausted', '暂时没好也不要乱回复')),
                          more=['--background-tasks'], env_extra={'EVE_DEFERRED_REPLY_MAX_RETRIES': '2'})
        self.assertEqual(len(self.kind('chat')), 3)
        self.assertEqual(self.turn('暂时没好也不要乱回复')['status']['state'], 'Failed')
        self.assertFalse(self.progress('exhausted'))
        self.assertFalse(self.documents().get('eve.task', {}).get('tasks.v1', {}).get('tasks', []))
        before = len(self.requests)
        self.http_status = lambda kind, number: 400 if kind == 'chat' else 200
        self.run_deferred(self.send(self.message('rejected', '核对参数拒绝')))
        attempted = [r for r in self.requests[before:]
                     if next((m['content'] for m in reversed(r['body']['messages']) if m['role'] == 'user'), None) == '核对参数拒绝']
        self.assertEqual(len(attempted), 1)
        self.assertEqual(self.turn('核对参数拒绝')['status']['state'], 'Failed')
        self.assertFalse(self.progress('rejected'))

    def test_completed_send_before_failure_is_not_sent_again(self):
        failed = []

        def status(kind, number):
            if kind == 'chat' and self.all_results(self.requests[-1]['body']):
                failed.append(number)
                return 503 if len(failed) == 1 else 200
            return 200

        self.http_status = status
        self.callback = lambda body: self.choose('send_message', {'text': '这条结果已核实'}) if not self.all_results(body) else dialogue.text_response('{"action":"silence"}')
        self.run_deferred(self.send(self.message('sent-once', '结果好了就告诉我')))
        self.assertEqual([e['text'] for e in self.progress('sent-once')], ['这条结果已核实'])
        self.assertEqual(self.turn('结果好了就告诉我')['status']['state'], 'Completed')


if __name__ == '__main__':
    unittest.main()
