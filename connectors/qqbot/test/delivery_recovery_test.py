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

    def test_sdk_diagnostics_are_logged_for_progress_and_final_without_raw_error_content(self):
        self.callback = lambda body: dialogue.text_response('已保存正文') if self.results(body) else dialogue.tool_response(
            [('send_message', {'text': '正在核对当前结果', 'purpose': 'progress'})])
        message = self.message('diagnostic', '执行诊断任务', expected='已保存正文') | {
            'fail_delivery': True, 'fail_progress': True,
            'progress_diagnostic': {'http_status': 429, 'biz_code': 1234, 'failure_kind': 'http'},
            'delivery_diagnostic': {'http_status': 400, 'biz_code': 5678, 'delivery_mode': 'passive',
                                    'notice_status': 'sent', 'secret': 'never-print-private-body'},
        }
        self.run_eve([{'send': message}, self.wait_receipt('diagnostic', 'Failed')], panel=True,
                     env_extra={'EVE_QQ_LIVE_INTERACTION': 'true'})
        logs = self.panel_stderr.read_text()
        self.assertIn('progress_failed_no_retry', logs)
        self.assertIn('delivery_failed_no_retry', logs)
        for value in ['429', '1234', '400', '5678', 'notice_status']:
            self.assertIn(value, logs)
        self.assertNotIn('never-print-private-body', logs)
        self.assertEqual(self.receipt('diagnostic')['state'], 'Failed')
        self.assertEqual(self.receipt('diagnostic')['reply'], '已保存正文')


if __name__ == '__main__':
    unittest.main()
