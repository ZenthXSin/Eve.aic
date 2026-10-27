"""真实后台失败后保留可读原因、步骤及跨重启查询，失败命令不重放。"""
import json
import unittest

import delegation_context_test as context
from background_task_test import calls
from dialogue_tools_test import text_response


class BackgroundDiagnosticsAcceptance(context.DelegationContextAcceptance):
    for _name in dir(context.DelegationContextAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def test_completed_write_and_failed_command_remain_queryable_after_restart(self):
        def respond(body):
            results = self.results(body)
            if self.is_worker(body):
                if not results:
                    return calls(('save', 'write_file', {
                        'path':'kept.txt','content':'COMPLETED_BEFORE_FAILURE','expected_sha256':''}))
                self.assertEqual(len(results), 1)
                self.assertTrue(results[0]['success'])
                return calls(('missing', 'run_command', {'program':'eve-test-nonexistent-executable-4817','args':[]}))
            if self.text(body) == '查看失败步骤':
                if not results:
                    return calls(('details','list_background_tasks', {
                        'task_id':self.tasks()[0]['id'],'step_offset':0,'step_limit':1}))
                if len(results) == 1:
                    task = results[0]['task']
                    self.assertEqual(task['step_total'], 2)
                    self.assertEqual(task['execution_steps'][0]['tool'], 'write_file')
                    self.assertEqual(task['execution_steps'][0]['status'], '已返回结果')
                    self.assertEqual(task['next_step_offset'], 1)
                    return calls(('next','list_background_tasks', {
                        'task_id':self.tasks()[0]['id'],'step_offset':1,'step_limit':1}))
                task = results[-1]['task']
                self.assertEqual(task['execution_steps'][0]['tool'], 'run_command')
                self.assertEqual(task['execution_steps'][0]['status'], '失败')
                self.assertIn('程序无法启动', task['execution_steps'][0]['detail'])
                self.assertIsNone(task['next_step_offset'])
                return text_response('已写入文件，后续程序无法启动，原结果保留')
            if not results:
                return calls(('dispatch','dispatch_background_task', {
                    'title':'失败诊断','instructions':'先保存文件，再执行指定程序，保留失败记录。','max_steps':3}))
            return text_response('后台已派发')
        self.callback = respond
        def failed():
            self.wait_for(lambda:any(t['status']=='Failed' and (t.get('notice') or {}).get('status')=='Delivered'
                for t in self.tasks()), '后台失败及通知没有完成', 20)
        self.enabled([*self.send(self.message('dispatch','保存后执行',expected='后台已派发')),
                      *self.checkpoint('failed')], checkpoints={'failed':failed})
        [task] = self.tasks()
        self.assertIn('程序无法启动', task['failure'])
        self.assertIn('write_file', task['failure'])
        self.assertIn('run_command', task['failure'])
        self.assertEqual(task['usage']['tool_calls'], 2)
        self.assertEqual((self.source/'kept.txt').read_text(), 'COMPLETED_BEFORE_FAILURE')
        workers = sum(self.is_worker(r['body']) for r in self.requests)
        self.enabled(self.send(self.message('query','查看失败步骤',expected='已写入文件，后续程序无法启动，原结果保留')))
        self.assertEqual(sum(self.is_worker(r['body']) for r in self.requests), workers)
        self.assertEqual(self.tasks()[0]['id'], task['id'])
        self.assertEqual((self.source/'kept.txt').read_text(), 'COMPLETED_BEFORE_FAILURE')


if __name__ == '__main__':
    unittest.main()
