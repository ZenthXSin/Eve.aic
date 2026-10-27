"""实际 Eve 进程保存并执行可复用流程，跨重启复用且继续接受命令审核。"""
import json
import sys
import unittest

import developer_tools_test as developer
from background_task_test import calls
from dialogue_tools_test import text_response


class WorkflowToolsAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def enabled(self, script):
        self.run_eve(script,interest=False,memory=True,research=False,practice=False,
            more=['--workflow-tools'],env_extra={'EVE_DEVELOPER_CONFIG':str(self.developer_config)})

    def test_save_run_restart_and_reuse_with_actual_file_outputs(self):
        workflow = {}
        def respond(body):
            results=self.results(body)
            latest=next(m['content'] for m in reversed(body['messages']) if m['role']=='user')
            if latest=='保存并运行流程' and not results:
                return calls(('save','save_workflow_tool',{'name':'生成_report','description':'生成文件后检查内容',
                    'steps':[
                        {'id':'write','name':'write_file','arguments':{'path':{'$param':'path'},'content':{'$param':'content'},'expected_sha256':''}},
                        {'id':'verify','name':'run_command','arguments':{'program':sys.executable,'args':['-c',
                            'import pathlib,sys; assert pathlib.Path(sys.argv[1]).read_text()==sys.argv[2]',{'$param':'path'},{'$param':'content'}]}}]}))
            if latest=='保存并运行流程' and len(results)==1:
                self.assertTrue(results[0]['saved']); self.assertFalse(results[0]['executed'])
                workflow.update(results[0]['workflow'])
                self.assertFalse((self.source/'first.txt').exists())
            if (latest=='保存并运行流程' and len(results)==1) or not results:
                second=latest!='保存并运行流程'
                return calls(('execute','run_workflow_tool',{'workflow_id':workflow['id'],'revision':1,
                    'parameters':{'path':'second.txt' if second else 'first.txt','content':'SECOND' if second else 'FIRST'}}))
            self.assertTrue(results[-1]['completed'],results[-1])
            self.assertEqual([step['status'] for step in results[-1]['steps']],['returned','returned'])
            return text_response('流程完成并核实')
        self.callback=respond
        self.enabled(self.send(self.message('first','保存并运行流程',expected='流程完成并核实')))
        self.assertEqual((self.source/'first.txt').read_text(),'FIRST')
        self.enabled(self.send(self.message('second','重启后运行已保存流程',expected='流程完成并核实')))
        self.assertEqual((self.source/'second.txt').read_text(),'SECOND')
        runs=list((self.work/'state/workflow-tools/runs').glob('*.json'))
        self.assertEqual(len(runs),2)
        self.assertTrue(all(json.loads(p.read_text())['completed'] for p in runs))

    def test_workflow_cannot_bypass_action_review_or_continue_after_denied_step(self):
        folder=self.source/'retained'; folder.mkdir()
        reviewed=[]
        def review(request):
            reviewed.append(request)
            return {'risk':'medium','authorization':'unknown','outcome':'deny','rationale':'用户未授权删除'}
        self.review_reply=review
        def respond(body):
            results=self.results(body)
            if not results:
                return calls(('save','save_workflow_tool',{'name':'受审查流程','description':'命令仍需审查',
                    'steps':[{'id':'remove','name':'run_command','arguments':{'program':'rm','args':['-r','retained']}},
                             {'id':'after','name':'write_file','arguments':{'path':'must-not-run.txt','content':'bad','expected_sha256':''}}]}))
            if len(results)==1:
                return calls(('execute','run_workflow_tool',{'workflow_id':results[0]['workflow']['id'],'revision':1}))
            self.assertFalse(results[-1].get('completed',False))
            return text_response('审核拒绝，后续未执行')
        self.callback=respond
        self.enabled(self.send(self.message('denied','只检查现有目录',expected='审核拒绝，后续未执行')))
        self.assertTrue(folder.exists()); self.assertFalse((self.source/'must-not-run.txt').exists())
        self.assertEqual(len(reviewed),1)
        self.assertIn('只检查现有目录',reviewed[0]['user_messages'][-1])


if __name__=='__main__':unittest.main()
