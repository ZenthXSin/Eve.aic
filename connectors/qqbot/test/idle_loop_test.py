"""无人新消息时的实际宿主回顾、行动、改观与跨进程恢复；QQ/主模型均为本地替身。

仅在子进程完全退出后推进临时夹具账本的到期时刻，避免测试等待半小时。
不在产品中加入改时钟接口，不修改正式数据或扩大预算。
"""
import hashlib
import json
import os
import sqlite3
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response

PREFIX='eve.idle.observation.v1\n'
PLAN_PREFIX='eve.subject.plan.v1\n'
EXPERIMENT='actual experiment evidence'
EXPERIMENT_SHA256=hashlib.sha256(EXPERIMENT.encode()).hexdigest()
ACTION_CONTRACT={'kind':'receipts','contract':{'checks':[{
    'tool':'read_file','arguments':{'path':'experiment.txt'},
    'output_pointer':'/sha256','expected':EXPERIMENT_SHA256}],
    'require_final_batch':True}}

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
        self.observations=[]
        self.plans=0
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
        if '本任务只规划一个有价值的下一目标' in system:
            self.assertFalse(body.get('tools'),'缺合同的旧行动必须先无工具规划')
            text=next(m['content'] for m in reversed(body['messages'])
                if PLAN_PREFIX in (m.get('content') or ''))
            frozen=json.JSONDecoder().raw_decode(text.split(PLAN_PREFIX,1)[1])[0]
            self.assertTrue(frozen['exact_goal'])
            self.assertTrue(frozen['require_receipts'])
            self.assertEqual(frozen['goal'],self.proposed_action['goal'])
            self.assertEqual(frozen['problem_key'],self.proposed_action['assessment']['problem_key'])
            self.assertTrue(set(self.proposed_action['assessment']['evidence']).issubset(frozen['evidence']))
            self.plans+=1
            return text_response(json.dumps({**self.proposed_action,'completion_contract':ACTION_CONTRACT},ensure_ascii=False))
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
            self.observations.append(observation)
            self.reflections+=1
            ids=[e['id'] for e in observation['evidence']]
            self.assertFalse(any(e['kind']=='focus' and e['id'].startswith('contract-')
                for e in observation['evidence']),'冻结合同和授权原话不能作为普通经历节选')
            choice='act' if self.mode in ('act','legacy-act','text-act','own-interest') and self.reflections==1 else 'wait'
            evidence=ids[0] if ids else None
            result={'choice':choice,'thought':'依据实际经历决定下一步','reason':'值得验证时行动，没有新证据时等待',
                'judgments':[],'action':None,'revisit_after_minutes':30}
            if self.mode=='malicious-tools':
                return tool_response([('write_file',{'path':'forbidden.txt','content':'不应执行','expected_sha256':''})])
            if evidence:
                statement='先做一个实际实验' if choice=='act' else '实验结果改变了原来判断'
                result['judgments']=[{'key':'experiment','statement':statement,'confidence':70,'evidence':[evidence],'revision_reason':'从新的实际结果修正认识'}]
            if self.mode=='own-interest':
                experiences=[e for e in observation['evidence']
                    if e['kind'] in ('completed_conversation','task_result')]
                self.assertTrue(experiences,'自己的兴趣需要实际经历')
                actual=next((e for e in experiences if e['kind']=='task_result'),experiences[0])
                result['interest_updates']=[{'key':'rule-patterns','topic':'规则形成的图案',
                    'phase':'curious' if self.reflections==1 else 'resting',
                    'strength':60 if self.reflections==1 else 30,
                    'reason':'想弄清简单规则的变化' if self.reflections==1 else '实际实验后先放下这条方向',
                    'want_to_try':'保存一个可复查的小实验' if self.reflections==1 else '',
                    'evidence':[actual['id']]}]
            if choice=='act':
                self.assertIsNotNone(evidence)
                result['action']={'title':'隔离验证空闲想法','goal':'在工作区写一次experiment.txt并核对回执',
                    'assessment':{'problem_key':'idle-experiment','expected_gain':'验证自主行动闭环','verification':'检查文件和工具回执',
                        'reuse_plan':'保存为下一次可比较经验','benefit':90,'confidence':90,'cost':10,'risk':0,'evidence':[evidence]},
                    'completion_contract':ACTION_CONTRACT}
                if self.mode=='own-interest':result['action']['interest_key']='rule-patterns'
                if self.mode=='legacy-act':result['action'].pop('completion_contract')
                if self.mode=='text-act':result['action']['completion_contract']={'kind':'text','required_fragments':['实验完成']}
                self.proposed_action=result['action']
            return text_response(json.dumps(result,ensure_ascii=False))
        if '你在为 Eve 独立完成一项委托的任务' in system:
            results=self.results(body)
            if not results:return tool_response([('write_file',{'path':'experiment.txt','content':EXPERIMENT,'expected_sha256':''})])
            if len(results)==1:
                self.assertTrue(results[-1].get('success'),results)
                return tool_response([('read_file',{'path':'experiment.txt'})])
            self.assertEqual(results[-1].get('sha256'),EXPERIMENT_SHA256,results)
            self.assertEqual(results[-1].get('content'),EXPERIMENT,results)
            return text_response('已经实际写入实验文件，并以末批读取回执核对完整SHA256')
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
        # 独立历史库是新宿主的当前认知来源；仅推进这个隔离夹具的再回顾时间。
        database=self.state_path.parent/'self-interests.sqlite3'
        if database.exists():
            with sqlite3.connect(database) as db:
                for scope,payload in db.execute('SELECT scope,payload FROM minds').fetchall():
                    mind=json.loads(payload);mind['next_at_ms']=now-1
                    db.execute('UPDATE minds SET payload=? WHERE scope=?',
                        (json.dumps(mind,ensure_ascii=False),scope))

    def wait_revision(self,n):self.wait_for(lambda:self.mind() and self.mind()['revision']>=n,'回顾结果未保存',15)
    def test_idle_action_and_changed_judgment_across_processes_without_new_user_message(self):
        self.initialize();self.due()
        def completed():
            self.wait_for(lambda:(self.source/'experiment.txt').exists(),'自主行动没有执行',15)
            self.wait_for(lambda:any(t['spec']['title']=='隔离验证空闲想法' and t['status']=='Completed' for t in self.tasks()),'行动未完成',15)
        self.run_idle(self.checkpoint('action'),checkpoints={'action':completed})
        self.assertEqual(self.reflections,1)
        self.assertEqual((self.source/'experiment.txt').read_text(),EXPERIMENT)
        first=self.mind();self.assertEqual(first['revision'],1)
        self.due()
        self.run_idle(self.checkpoint('reconsider'),checkpoints={'reconsider':lambda:self.wait_revision(2)})
        self.assertEqual(self.mind()['revision'],2)
        self.assertEqual(self.mind()['judgments'][0]['statement'],'实验结果改变了原来判断')
        actions=[t for t in self.tasks() if t['spec']['title']=='隔离验证空闲想法']
        self.assertEqual(len(actions),1)
        self.assertEqual(actions[0]['usage'],{'tool_calls':2,'model_requests':3})
        self.assertEqual(actions[0]['execution']['completion_contract'],ACTION_CONTRACT)
        self.assertEqual(actions[0]['execution']['recoveries'],0)
        checkpoint=actions[0]['execution']['checkpoint']
        calls=[c for m in checkpoint['messages'] for c in m.get('tool_calls',[])]
        self.assertEqual([c['name'] for c in calls],['write_file','read_file'])
        self.assertEqual(calls[-1]['arguments'],ACTION_CONTRACT['contract']['checks'][0]['arguments'])
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

    def test_own_interest_guides_real_action_changes_from_result_and_returns_to_chat(self):
        self.mode='own-interest';self.initialize();self.due()
        def completed():
            self.wait_for(lambda:any(t['spec']['title']=='隔离验证空闲想法' and t['status']=='Completed'
                for t in self.tasks()),'兴趣驱动的实际行动未完成',15)
        self.run_idle(self.checkpoint('own-action'),checkpoints={'own-action':completed})
        first=self.mind()['self_interests'][0]
        self.assertEqual(first['revision'],1)
        self.assertEqual(first['current']['phase'],'curious')
        self.assertEqual((self.source/'experiment.txt').read_text(),EXPERIMENT)
        self.due()
        self.run_idle(self.checkpoint('own-reconsider'),checkpoints={'own-reconsider':lambda:self.wait_revision(2)})
        self.assertEqual(self.observations[-1]['self_interests'],[first], '下一次回顾应收到原兴趣')
        current=self.mind()['self_interests'][0]
        self.assertEqual(current['revision'],2)
        self.assertEqual(current['first_recorded_at_ms'],first['first_recorded_at_ms'])
        self.assertEqual(current['current']['phase'],'resting')
        self.assertNotEqual(current['current']['evidence'],first['current']['evidence'])
        tasks_before=json.dumps(self.tasks(),sort_keys=True)
        mind_before=json.dumps(self.mind(),sort_keys=True)
        def chat(body):
            text=json.dumps(body,ensure_ascii=False)
            latest=next(m['content'] for m in reversed(body['messages']) if m.get('role')=='user')
            def query(arguments):
                response=tool_response([('manage_subject_focus',arguments)])
                response['choices'][0]['message']['tool_calls'][0]['id']=f'interest-read-{len(self.results(body))}'
                return response
            if latest=='别的窗口看不到私聊兴趣':
                self.assertNotIn('rule-patterns',text)
                return text_response('这里只知道本窗口的事')
            self.assertIn('rule-patterns',text)
            self.assertIn('实际实验后先放下这条方向',text)
            queried=self.results(body)
            if not queried:
                return query({'action':'interests','limit':1})
            if len(queried)==1:
                self.assertEqual(queried[-1]['total'],1)
                self.assertEqual(queried[-1]['items'][0]['current']['phase'],'resting')
                return query({'action':'interest_history','id':'rule-patterns','limit':1})
            if len(queried)==2:
                self.assertEqual(queried[-1]['items'][0]['current']['phase'],'curious')
                self.assertEqual(queried[-1]['next_after'],1)
                return query({'action':'interest_history','id':'rule-patterns','after_revision':1,'limit':1})
            self.assertEqual(queried[-1]['items'][0]['current']['phase'],'resting')
            self.assertIsNone(queried[-1]['next_after'])
            return text_response('做过这个小实验，现在先放一放')
        self.callback=chat
        self.run_idle([
            *self.send(self.message('own-chat','你刚才对什么感兴趣',expected='做过这个小实验，现在先放一放')),
            *self.send(self.message('other-chat','别的窗口看不到私聊兴趣',user='user-2',expected='这里只知道本窗口的事')),
        ])
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),tasks_before,'聊天和重启不重放兴趣行动')
        self.assertEqual(json.dumps(self.mind(),sort_keys=True),mind_before,'聊天不伪造新的兴趣修订')
        self.assertEqual(self.reflections,2)

    def test_legacy_action_freezes_receipts_in_zero_tool_planning_before_execution(self):
        self.mode='legacy-act';self.initialize();self.due()
        def completed_and_delivered():
            self.wait_for(lambda:any(t['source'].startswith('subject-focus:') and t['status']=='Completed'
                for t in self.tasks()),'旧行动没有完成规划、冻结和真实验收',15)
            self.wait_for(lambda:all(t['notice'] is None or t['notice']['status']=='Delivered'
                for t in self.tasks()),'重启快照前已完成任务通知尚未送达',15)
        self.run_idle(self.checkpoint('legacy-action'),checkpoints={'legacy-action':completed_and_delivered})
        self.assertEqual(self.reflections,1)
        self.assertEqual(self.plans,1)
        [planning]=[t for t in self.tasks() if t['source'].startswith('subject-plan:')]
        [action]=[t for t in self.tasks() if t['source'].startswith('subject-focus:')]
        self.assertEqual(planning['status'],'Completed')
        self.assertEqual(planning['usage'],{'tool_calls':0,'model_requests':1})
        self.assertEqual(planning['execution']['completion_contract']['kind'],'text')
        self.assertEqual(json.loads(planning['result'])['goal'],self.proposed_action['goal'])
        self.assertEqual(action['execution']['completion_contract'],ACTION_CONTRACT)
        self.assertEqual(action['usage'],{'tool_calls':2,'model_requests':3})
        self.assertEqual(action['execution']['recoveries'],0)
        self.assertEqual((self.source/'experiment.txt').read_text(),EXPERIMENT)
        policy=next(f for f in self.subject()['foci'] if f['id']=='autonomy-policy-v1')
        self.assertEqual(json.loads(policy['goal'])['issued'],3,'回顾、规划、行动各占原授权一个名额')
        before=json.dumps(self.tasks(),sort_keys=True)
        self.run_idle(self.checkpoint('legacy-quiet'),checkpoints={'legacy-quiet':lambda:time.sleep(1.3)})
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),before,'重启不改合同或重复规划、行动')

    def test_text_contract_cannot_authorize_or_complete_idle_action(self):
        self.mode='text-act';self.initialize();self.due()
        self.run_idle(self.checkpoint('text-rejected'),checkpoints={'text-rejected':lambda:self.wait_for(
            lambda:any(t['source'].startswith('subject-idle:') and t['status']=='Failed' for t in self.tasks()),
            '空闲行动错误接受纯文本合同',15)})
        self.assertFalse((self.source/'experiment.txt').exists())
        self.assertFalse(any(t['source'].startswith(('subject-focus:','subject-plan:')) for t in self.tasks()))
        self.assertIsNone(self.mind())
        [review]=[t for t in self.tasks() if t['source'].startswith('subject-idle:')]
        self.assertEqual(review['usage'],{'tool_calls':0,'model_requests':2})
        self.assertIn('空闲回顾结构、来源或判断修订检查失败',review['failure'])
        saved=json.loads(review['execution']['checkpoint']['result'])
        self.assertEqual(saved['action']['completion_contract']['kind'],'text')

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
