"""实际 QQ 宿主进程：Jev 关系、含混积攒、独立主动回复与事后经验闭环。

所有 HTTP 模型及 QQ 发送均为本地替身，不向真实群发测试消息。
"""
import json
import sqlite3
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import group_chat_test
import reminder_test

class GroupRelationAcceptance(group_chat_test.GroupChatAcceptance):
    for _name in dir(group_chat_test.GroupChatAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.jev_requests=[]
        self.recall_requests=[]
        self.recall_arrivals=[]
        self.recall_score=lambda candidate:0.1
        self.classification=lambda state:('uncertain','none',0.4,False)
        self.jev_status=200
        # 关系判断应答前的延迟（秒）与进行中的时间段，用于复现名额被判断占用的情形。
        self.relation_delay=0
        self.relation_spans=[]
        self.bad_evidence=False
        self.active_reply=False
        outer=self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*_): pass
            def do_POST(self):
                try:
                    assert self.path=='/v1/systemone'
                    assert self.headers['Authorization']=='Bearer group-jev-test-secret'
                    body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                    assert body['model']=='group-jev-test-model'
                    if 'candidates' in body['state']:
                        # 原话挑选：每条候选一个 noul 问题。
                        assert set(body['questions'])=={c['candidate'] for c in body['state']['candidates']}
                        outer.recall_requests.append(body)
                        outer.recall_arrivals.append(time.monotonic())
                        answer={'answers':{c['candidate']:{'type':'noul','noul':outer.recall_score(c)} for c in body['state']['candidates']}}
                        data=json.dumps(answer).encode()
                        self.send_response(outer.jev_status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
                        return
                    assert set(body['questions'])=={'relation','topic','needs_reply'}
                    outer.jev_requests.append(body)
                    started=time.monotonic()
                    if outer.relation_delay: time.sleep(outer.relation_delay)
                    outer.relation_spans.append((started,time.monotonic()))
                    kind,topic,confidence,needs=outer.classification(body['state'])
                    def choice(name,selected):
                        return {'type':'choice','choice':selected,'confidence':confidence,'probabilities':{key:1.0 if key==selected else 0.0 for key in body['questions'][name]['criteria']}}
                    answer={'answers':{'relation':choice('relation',kind),'topic':choice('topic',topic),'needs_reply':{'type':'noul','noul':0.99 if needs else 0.01}}}
                    data=json.dumps(answer).encode()
                    self.send_response(outer.jev_status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
                except Exception: outer.errors.append(traceback.format_exc())
        self.jev_server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        self.jev_server.daemon_threads=True
        self.jev_thread=threading.Thread(target=self.jev_server.serve_forever,daemon=True);self.jev_thread.start()

    def tearDown(self):
        self.jev_server.shutdown();self.jev_server.server_close();self.jev_thread.join()
        super().tearDown()

    def run_eve(self,script,**options):
        env=dict(EVE_JEV_API_KEY='group-jev-test-secret',EVE_JEV_BASE_URL=f'http://127.0.0.1:{self.jev_server.server_port}',
                 EVE_MODELS_JEV_ENABLED='true',EVE_MODELS_JEV_PROVIDER='jev',EVE_MODELS_JEV_MODEL='group-jev-test-model',
                 EVE_MODELS_JEV_TIMEOUT_MS='10000',EVE_CHIME_COOLDOWN_MS='0',
                 EVE_GROUP_RELATION_ACTIVE_INTERVAL_MS='3000',EVE_GROUP_RELATION_REVIEW_INTERVAL_MS='1000')
        env.update(options.pop('env_extra',{}))
        return reminder_test.base().run_eve(self,script,interest=True,research=False,practice=False,
            more=['--group-chat','--training','--message-judge','jev','--autonomy-budget','100'],env_extra=env,panel=True,timeout=45,**options)

    def test_disabled_group_keeps_raw_history_without_jev_or_proactive_model(self):
        def settled():
            self.wait_for(lambda:len(self.rows('observations')) >= 3,'关闭后未保存群原话',10)
            self.assertFalse(self.run_release.wait(3.2))
        self.run_eve([self.observation('off-ambient','这个思路，Eve你怎么看'),
                      self.observation('off-at','帮我看看',mentioned=True),
                      self.observation('off-prefix','EVE 在不在'),
                      *self.send(self.message('off-prefix','EVE 在不在',expected='这个思路可以试试',scope='group')),
                      *self.checkpoint('settled')],
                     env_extra={'EVE_CHIME_ENABLED':'false'},checkpoints={'settled':settled})
        self.assertEqual(self.jev_requests,[])
        self.assertEqual(self.kind('chime'),[])
        self.assertEqual(self.kind('group_review'),[])
        self.assertEqual(len(self.kind('chat')),1)
        self.assertEqual(self.relation_rows('judgments'),[])
        self.assertEqual(self.pushes(),[])

    def classify(self,decoded,latest):
        if isinstance(decoded,dict) and {'input','subsequent'} <= set(decoded): return 'group_review'
        return super().classify(decoded,latest)

    def reply(self,kind,decoded,latest):
        if kind=='chime': return json.dumps({'reply':self.active_reply,'confidence':95,'reason':'批量观察后决定是否接话'})
        if kind=='group_review':
            evidence=[m['id'] for m in decoded['subsequent'] if not m['from_me']]
            return json.dumps({'decision':{'kind':'mindustry_question','needs_reply':True,'confidence':95,'topic':None,'reason':'后续群友明确说明了是在问 Mindustry'},'evidence':['invented-id'] if self.bad_evidence else evidence[:1]})
        return super().reply(kind,decoded,latest)

    def relation_rows(self,table):
        path=self.work/'state/jev-group-experience.sqlite3'
        if not path.exists(): return []
        with sqlite3.connect(path) as db:
            db.row_factory=sqlite3.Row
            return [dict(row) for row in db.execute('SELECT * FROM '+table+' ORDER BY seq')]

    def wait_judgments(self,count):
        self.wait_for(lambda:len(self.relation_rows('judgments'))>=count and all(r['status']!='judging' for r in self.relation_rows('judgments')),'群 Jev 判断未完成',12)

    def test_full_answered_context_survives_restart_and_related_followup_uses_it(self):
        self.classification=lambda state:('eve_question','none',0.99,True) if not state['exchanges'] else ('related','t0',0.99,True)
        self.run_eve([self.observation('original','请问 Eve，Mindustry 传送带怎么接',mentioned=True),
            *self.send(self.message('original','请问 Eve，Mindustry 传送带怎么接',expected='这个思路可以试试',scope='group')),
            *self.checkpoint('saved')],checkpoints={'saved':lambda:self.wait_judgments(1)})
        self.run_eve([self.observation('followup','那逻辑处理器呢',user='another-member'),*self.checkpoint('sent')],checkpoints={'sent':self.wait_outbound})
        latest=self.jev_requests[-1]['state']
        self.assertEqual(latest['exchanges'][0]['question']['text'],'请问 Eve，Mindustry 传送带怎么接')
        self.assertEqual(latest['exchanges'][0]['eve_reply'],'这个思路可以试试')
        self.assertEqual(len(self.kind('chime')),0,'明确关系不再请求主模型插话判断')
        prompt='\n'.join(m.get('content','') for m in self.kind('chat')[-1]['body']['messages'])
        self.assertIn('本条追问所关联的完整历史问答',prompt)
        self.assertEqual(self.pushes()[-1]['mention_user_id'],'another-member')

    def test_uncertain_accumulates_then_independent_active_and_review_learns_and_recall_uses_it(self):
        self.active_reply=True
        def immediate():
            self.wait_judgments(2)
            self.assertEqual(len(self.kind('chime')),0,'Jev 含混不能立即回退主模型')
            self.assertEqual(len(self.kind('chat')),0)
            self.assertEqual(len(self.relation_rows('experiences')),0)
        def learned():
            self.wait_for(lambda:len(self.relation_rows('experiences'))>=1,'独立整理没有写入 Jev 经验',15)
        self.run_eve([self.observation('uncertain-1','那处理器怎么接'),*self.checkpoint('first'),
            self.observation('uncertain-2','我也在问 Mindustry 的处理器'),*self.checkpoint('immediate'),*self.checkpoint('sent'),
            self.observation('clarify','刚才两句都是在问 Eve 的 Mindustry 逻辑处理器'),*self.checkpoint('learned')],
            checkpoints={'first':lambda:self.wait_judgments(1),'immediate':immediate,'sent':self.wait_outbound,'learned':learned},
            env_extra={'EVE_CHIME_MAX_REPLIES_PER_HOUR':'1'})
        self.assertEqual(len(self.kind('chime')),1)
        self.assertEqual(len(self.kind('chat')),1)
        self.assertTrue(self.kind('group_review'))
        for request in self.kind('group_review'): self.assertEqual(request['body'].get('tools',[]),[])
        # 重启不会重发待判定消息；相似新消息从持久经验召回给 Jev。
        before=len(self.pushes())
        self.run_eve([self.observation('similar','那处理器怎么接'),*self.checkpoint('judged')],checkpoints={'judged':lambda:self.wait_judgments(4)})
        self.assertTrue(self.jev_requests[-1]['state']['examples'])
        self.assertEqual(len(self.pushes()),before)

    def test_silent_active_decision_still_allows_later_human_evidence_to_be_learned(self):
        self.active_reply=False
        def learned():
            self.wait_for(lambda:len(self.relation_rows('experiences'))>=1,'主动系统沉默时未整理后续证据',12)
        self.run_eve([self.observation('ambiguous','那处理器怎么接'),*self.checkpoint('first'),
            self.observation('human-proof','我刚才是在问 Mindustry 的逻辑处理器'),*self.checkpoint('learned')],
            checkpoints={'first':lambda:self.wait_judgments(1),'learned':learned})
        self.assertEqual(self.pushes(),[])
        self.assertEqual(self.kind('chat'),[])
        self.assertEqual(len(self.kind('chime')),1)
        self.assertTrue(self.kind('group_review'))

    def test_uncertain_at_message_never_replies_twice_or_replays_pending_on_restart(self):
        self.active_reply=True
        def settled():
            self.wait_judgments(1)
            deadline=time.monotonic()+3.5
            self.wait_for(lambda:time.monotonic()>=deadline,'等待主动周期',5)
        self.run_eve([self.observation('at','看看这个',mentioned=True),*self.send(self.message('at','看看这个',expected='这个思路可以试试',scope='group')),*self.checkpoint('settled')],checkpoints={'settled':settled})
        self.assertEqual(len(self.kind('chat')),1);self.assertEqual(self.pushes(),[]);self.assertEqual(self.kind('chime'),[])
        self.run_eve([*self.checkpoint('settled-restart')],checkpoints={'settled-restart':settled})
        self.assertEqual(len(self.kind('chat')),1);self.assertEqual(self.pushes(),[])
        self.assertEqual(self.relation_rows('judgments')[0]['status'],'pending')

    def test_failed_jev_persists_and_fabricated_review_evidence_is_not_learned(self):
        self.jev_status=503;self.active_reply=True;self.bad_evidence=True
        def reviewed():
            self.wait_for(lambda:len(self.kind('group_review'))>=1,'没有事后整理',12)
            self.wait_for(lambda:any(r['reviewed_revision'] for r in self.relation_rows('judgments')),'整理检查点未保存',8)
        self.run_eve([self.observation('offline','那处理器怎么接'),*self.checkpoint('first'),
            self.observation('later','我是在问 Eve 的 Mindustry 问题'),*self.checkpoint('sent'),*self.checkpoint('reviewed')],
            checkpoints={'first':lambda:self.wait_judgments(1),'sent':self.wait_outbound,'reviewed':reviewed})
        self.assertEqual(self.relation_rows('experiences'),[])
        self.assertTrue(all(r['status']=='pending' for r in self.relation_rows('judgments')))
        self.assertEqual(len(self.kind('chat')),1,'失败只能由独立主动系统触发一次实际回复')

if __name__=='__main__': unittest.main()
