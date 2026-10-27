"""真实进程：待补充任务保存为阻塞，通知一次，重启和无关消息不重跑。"""
import hashlib
import json
import unittest
import reminder_test

import delegation_context_test as context
from background_task_test import calls
from dialogue_tools_test import text_response


class AskOnceAcceptance(context.DelegationContextAcceptance):
    for _name in dir(context.DelegationContextAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    events = reminder_test.ReminderAcceptance.events
    pushes = reminder_test.ReminderAcceptance.pushes

    def setUp(self):
        super().setUp()
        config = json.loads(self.developer_config.read_text())
        config['operators'].append('qq:' + hashlib.sha256(json.dumps(
            ['1904159860','group','group-1','user-1'], separators=(',',':')).encode()).hexdigest())
        self.developer_config.write_text(json.dumps(config))

    def run_eve(self, script, **options):
        options['more'] = [*options.get('more', []), '--sleep']
        return super().run_eve(script, **options)

    def test_blocked_task_keeps_real_work_and_asks_original_user_once_across_restart(self):
        question = '已保存检查结果；发布到哪个环境？'
        def respond(body):
            results = self.results(body)
            if self.is_worker(body):
                if not results:
                    return calls(('readiness', 'write_file', {
                        'path':'checked.txt','content':'INSPECTED','expected_sha256':''}))
                if len(results) == 1:
                    return calls(('blocked', 'task_progress', {'status':'blocked','remaining':question}))
                return text_response(question)
            if self.text(body) == '开始任务':
                if not results:
                    return calls(('dispatch','dispatch_background_task', {
                        'title':'发布核查','instructions':'检查完成后先确认发布环境，不能假定。','max_steps':4}))
                return text_response('后台已派发')
            if self.text(body) == '查看进度':
                if not results:
                    return calls(('query','list_background_tasks', {'task_id':self.tasks()[0]['id']}))
                task = results[0]['task']
                self.assertTrue(task['blocked'])
                self.assertFalse(task['waiting_to_resume'])
                self.assertIn(question, task['result'])
                self.assertEqual(task['step_total'], 2)
                return text_response('检查已完成，发布环境仍待补充')
            return text_response('收到')
        self.callback = respond
        def delivered():
            self.wait_for(lambda:any(t['status']=='Blocked' and (t.get('notice') or {}).get('status')=='Delivered'
                for t in self.tasks()), '待补充状态或通知未保存', 20)
        route = {'scope':'group','target':'group-1'}
        self.enabled([*self.send(self.message('begin','开始任务',expected='后台已派发',**route)),
                      *self.checkpoint('done')],checkpoints={'done':delivered})
        self.assertEqual((self.source/'checked.txt').read_text(),'INSPECTED')
        [push] = self.pushes()
        self.assertIn(question,push['text'])
        self.assertNotIn('完成了',push['text'])
        self.assertEqual(push['mention_user_id'],'user-1')
        workers=sum(self.is_worker(r['body']) for r in self.requests)
        self.enabled([*self.send(self.message('other','别的话题',user='user-2',expected='收到',**route)),
                      *self.send(self.message('query','查看进度',expected='检查已完成，发布环境仍待补充',**route))])
        self.assertEqual(sum(self.is_worker(r['body']) for r in self.requests),workers)
        self.assertEqual(len(self.pushes()),1)
        self.assertEqual(self.tasks()[0]['status'],'Blocked')

    def test_subagent_reports_blocked_to_parent_without_claiming_completion(self):
        def respond(body):
            results = self.results(body)
            if self.is_worker(body):
                if not results:
                    return calls(('blocked','task_progress',{'status':'blocked','remaining':'需要目标环境名称'}))
                return text_response('部署到哪个环境？')
            if not results:
                return calls(('delegate','run_subagent',{'task':'核对目标环境，缺少信息就提问。','max_steps':3}))
            self.assertFalse(results[0]['completed'])
            self.assertTrue(results[0]['blocked'])
            self.assertEqual(results[0]['result'],'部署到哪个环境？')
            return text_response('部署到哪个环境？')
        self.callback=respond
        self.enabled(self.send(self.message('delegate','核对环境',expected='部署到哪个环境？')))
        self.assertEqual(self.tasks(),[])


if __name__ == '__main__':
    unittest.main()
