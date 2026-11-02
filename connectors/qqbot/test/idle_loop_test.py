"""无人新消息时的实际宿主回顾、行动、改观与跨进程恢复；QQ/主模型均为本地替身。

仅在子进程完全退出后推进临时夹具账本的到期时刻，避免测试等待半小时。
不在产品中加入改时钟接口，不修改正式数据或扩大预算。
"""
import hashlib
import json
import os
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response

PREFIX='eve.idle.observation.v1\n'

class IdleLoopAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'): locals()[_name]=None
    del _name

    def setUp(self):
        super().setUp()
        principal='qq:'+hashlib.sha256(json.dumps(['1904159860','c2c','user-1','user-1'],separators=(',',':')).encode()).hexdigest()
        path=self.work/'state/configuration/config.json'
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text(json.dumps({'format_version':1,'current':{'revision':1,'namespaces':{
            'runtime.people':{'schema_version':1,'values':{'owner_principals':principal}}}},'backups':[]}),encoding='utf8')
        self.small=[]
        self.review_reply=lambda _: {'risk':'medium','authorization':'high','outcome':'allow','rationale':'夹具主人原话明确授权本次有限回顾与本地实验'}
        self.reflections=0
        self.mode='act'
        outer=self
        class Small(BaseHTTPRequestHandler):
            def log_message(self,*_):pass
            def do_POST(self):
                body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                outer.small.append((dict(self.headers),body))
                observation=json.loads(body['messages'][-1]['content'].split(PREFIX,1)[1])
                ids=[e['id'] for e in observation['evidence']][:2]
                answer={'selected_ids':['invented-hint']} if outer.mode=='bad-small' else {'selected_ids':ids}
                data=json.dumps(text_response(json.dumps(answer))).encode()
                self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        self.small_server=ThreadingHTTPServer(('127.0.0.1',0),Small)
        self.small_thread=threading.Thread(target=self.small_server.serve_forever,daemon=True);self.small_thread.start()
        self.callback=self.respond

    def tearDown(self):
        self.small_server.shutdown();self.small_server.server_close();self.small_thread.join()
        super().tearDown()

    def subject(self):return self.documents().get('eve.subject',{}).get('subject.v1',{'foci':[]})
    def tasks(self):return self.documents().get('eve.task',{}).get('tasks.v1',{'tasks':[]})['tasks']
    def mind(self):
        return next((json.loads(f['goal']) for f in self.subject()['foci'] if f['id']=='idle-mind-v1'),None)
    def run_idle(self,script,**options):
        env={'EVE_DEVELOPER_CONFIG':str(self.developer_config),'EVE_IDLE_SMALL_BASE_URL':f'http://127.0.0.1:{self.small_server.server_port}'}
        env.update(options.pop('env_extra',{}))
        return self.run_eve(script,interest=False,memory=True,research=False,practice=False,
            more=['--background-tasks','--max-tool-rounds','4'],env_extra=env,**options)

    def respond(self,body):
        system=str(body['messages'][0]['content'])
        if '这是Eve无人打扰时的一次自主回顾' in system:
            self.assertFalse(body.get('tools'),'回顾轮次无外部操作工具')
            self.assertNotIn('invented-hint',json.dumps(body),'非法小模型ID不能注入主模型')
            messages=[]
            for m in body['messages']:
                text=m.get('content') or ''
                if '"kind":"eve.delegated_context"' in text:
                    packet=json.JSONDecoder().raw_decode(text[text.index('{"kind":"eve.delegated_context"'):])[0]
                    messages.extend(packet['messages'])
            observation=json.loads(next(m['text'][len(PREFIX):] for m in messages if (m.get('text') or '').startswith(PREFIX)))
            self.reflections+=1
            ids=[e['id'] for e in observation['evidence']]
            choice='act' if self.mode=='act' and self.reflections==1 else 'wait'
            evidence=ids[0] if ids else None
            result={'choice':choice,'thought':'依据实际经历决定下一步','reason':'值得验证时行动，没有新证据时等待',
                'judgments':[],'action':None,'revisit_after_minutes':30}
            if self.mode=='malicious-tools':
                return tool_response([('write_file',{'path':'forbidden.txt','content':'不应执行','expected_sha256':''})])
            if evidence:
                statement='先做一个实际实验' if choice=='act' else '实验结果改变了原来判断'
                result['judgments']=[{'key':'experiment','statement':statement,'confidence':70,'evidence':[evidence],'revision_reason':'从新的实际结果修正认识'}]
            if choice=='act':
                self.assertIsNotNone(evidence)
                result['action']={'title':'隔离验证空闲想法','goal':'在工作区写一次experiment.txt并核对回执',
                    'assessment':{'problem_key':'idle-experiment','expected_gain':'验证自主行动闭环','verification':'检查文件和工具回执',
                        'reuse_plan':'保存为下一次可比较经验','benefit':90,'confidence':90,'cost':10,'risk':0,'evidence':[evidence]}}
            return text_response(json.dumps(result,ensure_ascii=False))
        if '你在为 Eve 独立完成一项委托的任务' in system:
            if not self.results(body):return tool_response([('write_file',{'path':'experiment.txt','content':'actual experiment evidence','expected_sha256':''})])
            self.assertTrue(self.results(body)[-1].get('success'),self.results(body))
            return text_response('已经实际写入实验文件，回执成功')
        if not self.results(body):
            return tool_response([('configure_subject_autonomy',{'action':'configure_autonomy','enabled':True,'idle_reflection':True,'max_goals':5,'steps_per_goal':10})])
        self.assertTrue(self.results(body)[-1].get('enabled'),self.results(body))
        return text_response('空闲回顾已授权')

    def initialize(self):
        self.run_idle(self.send(self.message('enable','我是主人，授权空闲回顾、自主决定并在临时工作区验证；最多5个目标，各10步，24小时；未知副作用不重跑',expected='空闲回顾已授权')))

    def due(self):
        # 只编辑已经退出的隔离宿主的测试状态；其余原始内容和授权预算不改。
        raw=json.loads(self.state_path.read_text())
        ledger=json.loads(bytes(raw['entries']['eve.subject']['subject.v1']))
        now=int(time.time()*1000)
        for f in ledger['foci']:
            if f['status']=='active':f['next_check_at_ms']=now-1
            if f['id']=='idle-mind-v1':
                mind=json.loads(f['goal']);mind['next_at_ms']=now-1;f['goal']=json.dumps(mind,ensure_ascii=False)
        for event in ledger['events']:
            if event['input']['source']=='user_ingress':
                event['created_at_ms']-=31*60*1000
                for d in ledger['dedupe']:
                    if d['event_id']==event['id']:d['at_ms']=event['created_at_ms']
        raw['entries']['eve.subject']['subject.v1']=list(json.dumps(ledger,ensure_ascii=False).encode())
        self.state_path.write_text(json.dumps(raw),encoding='utf8')

    def wait_revision(self,n):self.wait_for(lambda:self.mind() and self.mind()['revision']>=n,'回顾结果未保存',15)
    def test_idle_action_and_changed_judgment_across_processes_without_new_user_message(self):
        self.initialize();self.due()
        def completed():
            self.wait_for(lambda:(self.source/'experiment.txt').exists(),'自主行动没有执行',15)
            self.wait_for(lambda:any(t['spec']['title']=='隔离验证空闲想法' and t['status']=='Completed' for t in self.tasks()),'行动未完成',15)
        self.run_idle(self.checkpoint('action'),checkpoints={'action':completed})
        self.assertEqual(self.reflections,1)
        self.assertEqual((self.source/'experiment.txt').read_text(),'actual experiment evidence')
        first=self.mind();self.assertEqual(first['revision'],1)
        self.due()
        self.run_idle(self.checkpoint('reconsider'),checkpoints={'reconsider':lambda:self.wait_revision(2)})
        self.assertEqual(self.mind()['revision'],2)
        self.assertEqual(self.mind()['judgments'][0]['statement'],'实验结果改变了原来判断')
        actions=[t for t in self.tasks() if t['spec']['title']=='隔离验证空闲想法']
        self.assertEqual(len(actions),1)
        self.assertEqual(actions[0]['usage']['tool_calls'],1)
        reviews=[t for t in self.tasks() if t['source'].startswith('subject-idle:')]
        self.assertEqual(len(reviews),2)
        self.assertTrue(all(t['usage']=={'tool_calls':0,'model_requests':2} and t['notice'] is None for t in reviews))
        self.assertTrue(all('Authorization' not in h for h,_ in self.small))
        before=json.dumps(self.tasks(),sort_keys=True)
        self.run_idle(self.checkpoint('quiet'),checkpoints={'quiet':lambda:time.sleep(1.3)})
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),before,'重启不重放旧行动或重复回顾')
        self.assertEqual(self.reflections,2)

    def test_invalid_small_selection_falls_back_to_original_observations(self):
        self.mode='bad-small';self.initialize();self.due()
        self.run_idle(self.checkpoint('wait'),checkpoints={'wait':lambda:self.wait_revision(1)})
        self.assertEqual(self.mind()['revision'],1)
        self.assertFalse((self.source/'experiment.txt').exists())
        self.assertEqual(len(self.small),1)

    @unittest.skipUnless(os.environ.get('EVE_TEST_IDLE_SMALL_URL'),'仅显式配置时连接真实无密钥小模型')
    def test_explicit_real_small_endpoint_with_synthetic_experiences(self):
        self.mode='wait';self.initialize();self.due()
        environment={key:os.environ[key] for key in ['HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','NO_PROXY','http_proxy','https_proxy','all_proxy','no_proxy','SSL_CERT_FILE','SSL_CERT_DIR'] if key in os.environ}
        environment['EVE_IDLE_SMALL_BASE_URL']=os.environ['EVE_TEST_IDLE_SMALL_URL']
        self.run_idle(self.checkpoint('real-small'),env_extra=environment,
            checkpoints={'real-small':lambda:self.wait_revision(1)})
        [review]=[t for t in self.tasks() if t['source'].startswith('subject-idle:')]
        self.assertEqual(review['usage'],{'tool_calls':0,'model_requests':2})
        reviews=[r['body'] for r in self.kind('chat') if '这是Eve无人打扰时的一次自主回顾' in str(r['body']['messages'][0].get('content'))]
        self.assertTrue(any('小模型挑选的回顾线索' in str(m.get('content')) for m in reviews[-1]['messages']),
                        '真实小模型的合法选择未进入主模型请求，不能把兜底当作成功')
        self.assertFalse((self.source/'experiment.txt').exists())

    def test_model_cannot_execute_tools_during_idle_review(self):
        self.mode='malicious-tools';self.initialize();self.due()
        self.run_idle(self.checkpoint('failed'),checkpoints={'failed':lambda:self.wait_for(
            lambda:any(t['source'].startswith('subject-idle:') and t['status']=='Failed' for t in self.tasks()),'非法工具调用未拒绝',15)})
        self.assertFalse((self.source/'forbidden.txt').exists())
        self.assertIsNone(self.mind())
        self.assertFalse(any(t['source'].startswith('subject-focus:') for t in self.tasks()))

if __name__=='__main__':unittest.main()
