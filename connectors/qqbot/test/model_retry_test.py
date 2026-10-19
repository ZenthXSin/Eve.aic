"""真实运行器故障注入：重试模型，命令实际执行一次，重启不重放。"""
import copy
import sys
import unittest
import developer_tools_test as developer
from dialogue_tools_test import tool_response,text_response

class ModelRetryAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def test_transient_failure_after_command_retries_only_model_and_preserves_receipts(self):
        retried = []
        def status(kind, number):
            if kind != 'chat': return None
            body = self.requests[-1]['body']
            if self.results(body):
                retried.append(copy.deepcopy(body))
                return 503 if len(retried) <= 2 else 200
            return 200
        self.http_status = status
        def respond(body):
            if not self.results(body):
                return tool_response([('run_command',{'program':sys.executable,'args':['-c',
                    "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('COMMAND_RESULT')"]})])
            self.assertIn('COMMAND_RESULT',self.results(body)[0]['stdout'])
            return text_response('已恢复并完成，命令仅执行一次')
        self.callback = respond
        self.enabled(self.send(self.message('retry-command','执行检查并恢复模型',expected='已恢复并完成，命令仅执行一次')))
        self.assertEqual(len(retried),3)
        self.assertTrue(all(body == retried[0] for body in retried))
        self.assertEqual((self.source/'once.txt').read_text(),'x')
        self.assertEqual(self.turn('执行检查并恢复模型')['status']['state'],'Completed')
        self.assertEqual(self.receipt('retry-command')['state'],'Sent')
        self.assertEqual(len(self.interactions()),1)
        journal=self.work/'state/developer-tools/operations.jsonl'
        before=journal.read_bytes()
        self.http_status = lambda kind,number:200
        self.callback = lambda body:text_response('重启没有重放')
        self.enabled(self.send(self.message('retry-restored','查看上轮结果',expected='重启没有重放')))
        self.assertEqual((self.source/'once.txt').read_text(),'x')
        self.assertEqual(journal.read_bytes(),before)

    def test_transient_failure_before_tools_recovers_same_request(self):
        self.http_status = lambda kind,number:429 if kind=='chat' and number==1 else 200
        self.callback = lambda body:text_response('普通聊天自动恢复')
        self.enabled(self.send(self.message('retry-chat','你好',expected='普通聊天自动恢复')))
        chats=self.kind('chat')
        self.assertEqual(len(chats),2)
        self.assertEqual(chats[0]['body'],chats[1]['body'])
        self.assertEqual(len(self.interactions()),1)

if __name__=='__main__': unittest.main()
