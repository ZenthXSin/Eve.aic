"""实际宿主：自定义/编码/只读子代理、后台派发与共享总步数；只使用本地替身。"""
import json
import threading
import unittest
import developer_tools_test as developer
from background_task_test import calls, WORKER
from dialogue_tools_test import text_response

FORBIDDEN = {"run_subagent", "dispatch_background_task", "create_subagent", "list_subagents", "delete_subagent", "set_wakeup_trigger"}

class SubagentProfilesAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def enabled(self, script, **options):
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                            more=["--background-tasks", "--subagents"],
                            env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config)}, **options)

    def text(self, body):
        return next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')

    def leaf(self, body):
        return "你是 Eve 的子代理" in body['messages'][0]['content']

    def worker(self, body):
        return WORKER in body['messages'][0]['content']

    def test_custom_coding_role_writes_verifies_and_survives_restart(self):
        saved = []
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & FORBIDDEN)
                self.assertEqual(self.definitions(body), {'read_file','write_file'})
                if not results:
                    return calls(('write', 'write_file', {'path':'from-agent.py','content':'value = 42\n','expected_sha256':''}))
                if len(results) == 1:
                    return calls(('verify','read_file',{'path':'from-agent.py'}))
                return text_response('文件已写入并读取核验')
            if self.text(body) == '列出角色':
                if not results: return calls(('list','list_subagents',{}))
                self.assertEqual(results[0]['profiles'][0]['id'], saved[0])
                return text_response('角色仍在')
            if not results:
                return calls(('create','create_subagent',{'name':'专用编码','instructions':'只处理本次授权代码并核验','tools':['read_file','write_file']}))
            if len(results) == 1:
                saved.append(results[0]['profile']['id'])
                return calls(('run','run_subagent',{'agent_id':saved[0],'task':'写入 from-agent.py 并读取验证'}))
            self.assertTrue(results[1]['completed'])
            self.assertEqual(results[1]['usage']['shared_steps_used'],4)
            return text_response('已创建并完成编码')
        self.callback = respond
        self.enabled(self.send(self.message('create','创建编码角色并完成实现',expected='已创建并完成编码')))
        self.assertEqual((self.source/'from-agent.py').read_text(), 'value = 42\n')
        self.enabled(self.send(self.message('restart','列出角色',expected='角色仍在')))

    def test_readonly_role_cannot_execute_a_write_or_recursive_tool(self):
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & (FORBIDDEN | {'write_file','edit_file','run_command'}))
                if not results:
                    return calls(('bad-write','write_file',{'path':'must-not-exist.txt','content':'bad','expected_sha256':''}),
                                 ('bad-recurse','run_subagent',{'task':'不得启动'}))
                self.assertTrue(all(r.get('failed') or r.get('code') or 'unknown' in json.dumps(r).lower() or 'UnknownTool' in json.dumps(r) for r in results), results)
                return text_response('未改动，也未递归派发')
            if not results: return calls(('review','run_subagent',{'agent_id':'review','task':'静态审阅'}))
            return text_response('只读核验结束')
        self.callback = respond
        self.enabled(self.send(self.message('read-only','用核验子代理审阅',expected='只读核验结束')))
        self.assertFalse((self.source/'must-not-exist.txt').exists())

    def test_ten_parallel_children_share_eves_fifty_steps(self):
        entered = []
        condition = threading.Condition()
        used = []
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                if not results and body.get('tools'):
                    with condition:
                        entered.append(self.text(body));condition.notify_all()
                        self.assertTrue(condition.wait_for(lambda:len(entered)==10,timeout=12))
                    return calls(*[(f'echo-{i}','echo',{'text':str(i)}) for i in range(5)])
                return text_response('依据已有回执收尾')
            if not results:
                return calls(*[(f'agent-{i}','run_subagent',{'task':f'并行核验{i}'}) for i in range(10)])
            used.extend(r['usage']['steps_used'] for r in results)
            self.assertTrue(all(r['usage']['shared_steps_used']<=50 for r in results))
            self.assertFalse(body.get('tools'),'共享预算耗尽后 Eve 也不能获得新工具')
            return text_response('共享步数已用尽')
        self.callback = respond
        self.enabled(self.send(self.message('shared','并行核验十项',expected='共享步数已用尽')))
        self.assertEqual(len(entered),10)
        self.assertEqual(sum(used),40,'主代理十次委派与子代理四十次工具调用共五十步')

    def test_background_worker_can_delegate_and_checkpoints_include_child_steps(self):
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & FORBIDDEN)
                if not results and body.get('tools'):
                    return calls(*[(f'child-{i}','echo',{'text':str(i)}) for i in range(3)])
                return text_response('子代理核验结束')
            if self.worker(body):
                if not results:
                    self.assertIn('run_subagent',self.definitions(body))
                    return calls(('child','run_subagent',{'task':'核验三个项目'}))
                self.assertEqual(results[0]['usage']['steps_used'],3)
                self.assertEqual(results[0]['usage']['shared_steps_remaining'],0)
                self.assertFalse(body.get('tools'))
                return text_response('后台核验结束')
            if not results:
                return calls(('dispatch','dispatch_background_task',{'title':'共享预算核验','instructions':'委派核验三个项目','max_steps':4}))
            return text_response('已派发')
        self.callback = respond
        def finished():
            self.wait_for(lambda:any(t['status']=='Completed' for t in self.documents().get('eve.task',{}).get('tasks.v1',{}).get('tasks',[])), 'background did not finish', 20)
        self.enabled([*self.send(self.message('background','后台委派核验',expected='已派发')),*self.checkpoint('finished')],checkpoints={'finished':finished})
        [task] = self.documents()['eve.task']['tasks.v1']['tasks']
        self.assertEqual(task['usage']['tool_calls'],4)
        frame = task['execution']['checkpoint']
        self.assertEqual((frame['used_tool_calls'],frame['delegated_tool_calls']),(1,3))
        self.assertEqual(frame['delegated_model_requests'],2)

if __name__ == '__main__': unittest.main(verbosity=2)
