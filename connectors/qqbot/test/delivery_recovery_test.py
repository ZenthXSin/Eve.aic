"""真实宿主验收：发送诊断、失败正文取回与跨会话隔离，不重跑任务。"""
import unittest
import dialogue_tools_test as dialogue
from tool_progress_test import ToolProgressAcceptance


class DeliveryRecoveryAcceptance(ToolProgressAcceptance):
    for _name in dir(ToolProgressAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def failed(self, message):
        return [{'send': message | {'fail_delivery': True}}, self.wait_receipt(message['id'], 'Failed')]

    def test_failed_reply_can_be_retrieved_in_original_private_session_after_restart_without_model_or_tools(self):
        result = '已经核对的结果；未完成的部分仍未完成。'
        self.callback = lambda _: dialogue.text_response(result)
        self.run_eve([*self.failed(self.message('original', '执行原任务', expected=result)),
                      *self.send(self.message('other', '/last-reply', user='user-2', contains='没有可取回')),
                      *self.send(self.message('retrieve', '/last-reply', expected=result))])
        original = self.receipt('original')
        self.assertEqual(original['state'], 'Failed')
        self.assertEqual(original['reply'], result)
        self.assertEqual(self.chat_rounds, 1)
        self.run_eve(self.send(self.message('restart-retrieve', '/last-reply', expected=result)))
        self.assertEqual(self.receipt('original'), original)
        self.assertEqual(self.chat_rounds, 1, '取回失败正文不重新调用模型或执行工具')
        self.assertEqual(self.receipt('restart-retrieve')['state'], 'Sent')

    def test_group_retrieval_never_reads_private_messages_other_groups_or_other_senders(self):
        def respond(body):
            query = next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')
            return dialogue.text_response('私聊结果' if query == '私聊任务' else '本群结果')
        self.callback = respond
        self.run_eve([*self.failed(self.message('private', '私聊任务', expected='私聊结果')),
                      *self.failed(self.message('group', '群任务', scope='group', expected='本群结果')),
                      *self.send(self.message('group-retrieve', '/last-reply', scope='group', expected='本群结果')),
                      *self.send(self.message('other-group', '/last-reply', scope='group', target='group-2', contains='没有可取回')),
                      *self.send(self.message('other-member', '/last-reply', scope='group', user='user-2', contains='没有可取回')),
                      *self.send(self.message('private-retrieve', '/last-reply', expected='私聊结果'))])
        self.assertEqual(self.chat_rounds, 2)

    def test_sdk_diagnostics_describe_explicit_send_without_automatic_error_or_final(self):
        self.callback = lambda body: dialogue.text_response('内部结束') if self.results(body) else dialogue.tool_response(
            [('send_message', {'text': '当前核实的发现', 'purpose': 'conversation'})])
        message = self.message('diagnostic', '执行诊断任务') | {
            'expected_type': 'finish', 'fail_progress': True,
            'progress_diagnostic': {'http_status': 429, 'biz_code': 1234, 'failure_kind': 'http',
                                    'secret': 'never-print-private-body'},
        }
        self.run_eve([{'send': message}, self.wait_receipt('diagnostic', 'Handled')], panel=True,
                     env_extra={'EVE_QQ_LIVE_INTERACTION': 'true'})
        logs = self.panel_stderr.read_text()
        self.assertIn('progress_failed_no_retry', logs)
        for value in ['429', '1234']:
            self.assertIn(value, logs)
        self.assertNotIn('never-print-private-body', logs)
        self.assertEqual(self.receipt('diagnostic')['state'], 'Handled')
        self.assertEqual(self.receipt('diagnostic')['progress'][0]['state'], 'Failed')
        self.assertIsNone(self.receipt('diagnostic')['reply'])
        self.assertFalse([e for e in self.events if e.get('type') in ('reply', 'segment', 'push')])

    def test_successful_http_without_receipt_persists_unknown_and_restart_never_replays_it(self):
        def respond(body):
            results = self.results(body)
            if not results:
                return dialogue.tool_response([('send_message', {'text': '这条发现可能已经送达',
                                                                 'purpose': 'conversation'})])
            self.assertEqual(len(results), 1)
            self.assertFalse(results[0]['sent'])
            self.assertEqual(results[0]['delivery_status'], 'unknown')
            self.assertIsNone(results[0]['message_id'])
            return dialogue.text_response('内部结束，未知消息不重发')
        self.callback = respond
        message = self.message('accepted-unknown', '核对一次发送回执') | {
            'expected_type': 'finish', 'fail_progress': True,
            'progress_diagnostic': {'http_status': 200, 'failure_kind': 'transport_unknown',
                                    'message_id': None, 'secret': 'PRIVATE_DELIVERY_BODY'},
        }
        # 通用替身的 send 保留显式 type，直接送入真实 Rust warning 解码；
        # 诊断和投递仍是两个独立帧，任意额外字段不得进入宿主日志。
        warning = {'id': 'send-warning', 'type': 'warning', 'code': 'send_failure',
                   'text': 'PRIVATE_WARNING_BODY', 'outbound': {
                       'version': 1, 'stage': 'receipt_validation', 'failure_kind': 'transport_unknown',
                       'elapsed_ms': 10674, 'http_status': 200,
                       'message': 'PRIVATE_ERROR', 'stack': 'PRIVATE_STACK',
                       'path': 'https://PRIVATE_HOST/PRIVATE_TOKEN',
                   }}
        self.run_eve([{'send': warning}, {'send': message},
                      self.wait_receipt(message['id'], 'Handled')], panel=True,
                     env_extra={'EVE_QQ_LIVE_INTERACTION': 'true'})
        logs = self.panel_stderr.read_text()
        for expected in ['bridge_send_failure', 'outbound_stage', 'receipt_validation',
                         'outbound_failure_kind', 'transport_unknown', 'outbound_elapsed_ms',
                         '10674', 'outbound_http_status', '200', 'progress_failed_no_retry']:
            self.assertIn(expected, logs)
        self.assertNotIn('PRIVATE_', logs)
        before = self.receipt(message['id'])
        self.assertEqual(before['state'], 'Handled')
        self.assertIsNone(before['reply'])
        self.assertEqual(len(before['progress']), 1)
        self.assertEqual(before['progress'][0]['state'], 'Unknown')
        self.assertIsNone(before['progress'][0].get('platform_message_id'))
        self.assertEqual(len(self.progress(message['id'])), 1)
        self.assertEqual(self.chat_rounds, 2)
        self.assertFalse([e for e in self.events if e.get('type') in ('reply', 'segment', 'push')])
        # 独立进程重新载入账本，并收到同一个平台输入 ID；只能去重结束。
        self.run_eve([{'send': message}, {'wait_command': {'id': message['id'], 'type': 'finish'}}],
                     env_extra={'EVE_QQ_LIVE_INTERACTION': 'true'})
        self.assertEqual(self.receipt(message['id']), before)
        self.assertEqual(self.chat_rounds, 2, '恢复及重复入站不重跑原模型或工具')
        self.assertFalse([e for e in self.events if e.get('type') in ('reply', 'segment', 'progress', 'push')])


if __name__ == '__main__':
    unittest.main()
