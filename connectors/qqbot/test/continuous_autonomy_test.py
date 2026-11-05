"""持续授权的实际 QQ 宿主验收；只用本地模型与桥接，时间只推进退出后的临时状态。"""
import json
import sqlite3
import time
import unittest

import idle_loop_test as idle
from dialogue_tools_test import text_response, tool_response

DAY=24*60*60*1000
POLICY='autonomy-policy-v1'


class ContinuousAutonomyAcceptance(idle.IdleLoopAcceptance):
    for _name in dir(idle.IdleLoopAcceptance):
        if _name.startswith('test_'):locals()[_name]=None
    del _name

    def setUp(self):
        super().setUp()
        self.mode='wait'

    def policy(self):
        return next(f for f in self.subject()['foci'] if f['id']==POLICY)

    def grant(self):return json.loads(self.policy()['goal'])

    def enable_continuous(self):
        def respond(body):
            if '这是Eve无人打扰时的一次自主回顾' in str(body['messages'][0]['content']):
                return super(ContinuousAutonomyAcceptance,self).respond(body)
            if not self.results(body):
                return tool_response([('configure_subject_autonomy',{
                    'action':'configure_autonomy','enabled':True,'continuous':True,
                    'max_goals':1,'steps_per_goal':10})])
            result=self.results(body)[-1]
            self.assertTrue(result['continuous'])
            self.assertIsNone(result['expires_at_ms'])
            self.assertEqual(result['window']['used'],0)
            return text_response('持续自主回顾已启用')
        self.callback=respond
        self.run_idle(self.send(self.message('continuous-enable',
            '持续自主回顾和行动，每24小时最多1个目标、每个10步，直到我暂停。',expected='持续自主回顾已启用')))
        self.callback=super().respond

    def move_window(self):
        # 只移动此测试已经停止的临时授权窗口，不改总序号、用量和原任务。
        raw=json.loads(self.state_path.read_text())
        ledger=json.loads(bytes(raw['entries']['eve.subject']['subject.v1']))
        policy=next(f for f in ledger['foci'] if f['id']==POLICY)
        grant=json.loads(policy['goal'])
        grant['window']['started_at_ms']-=DAY
        policy['goal']=json.dumps(grant)
        raw['entries']['eve.subject']['subject.v1']=list(json.dumps(ledger).encode())
        self.state_path.write_text(json.dumps(raw))
        self.due()

    def test_window_exhaustion_restart_next_window_and_pause_do_not_regrant(self):
        self.enable_continuous();self.due()
        self.run_idle(self.checkpoint('first'),checkpoints={'first':lambda:self.wait_revision(1)})
        initial=self.grant()
        self.assertEqual((initial['issued'],initial['window']['used']),(1,1))
        self.assertEqual(self.policy()['status'],'active')
        self.assertEqual(len(self.tasks()),1)
        before=json.dumps(self.tasks(),sort_keys=True)
        self.due()
        self.run_idle(self.checkpoint('exhausted'),checkpoints={'exhausted':lambda:time.sleep(1.3)})
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),before)
        self.assertEqual(self.grant(),initial,'窗口内重启不改额度')
        self.move_window()
        self.run_idle(self.checkpoint('next'),checkpoints={'next':lambda:self.wait_revision(2)})
        self.assertEqual((self.grant()['issued'],self.grant()['window']['used']),(2,1))
        self.assertEqual(len({t['source'] for t in self.tasks()}),2)
        self.assertTrue(all(t['usage']['tool_calls']==0 and t['notice'] is None for t in self.tasks()))
        def pause(body):
            if not self.results(body):return tool_response([('configure_subject_autonomy',{'action':'configure_autonomy','enabled':False})])
            self.assertFalse(self.results(body)[-1]['enabled'])
            return text_response('已暂停持续自主')
        self.callback=pause
        self.run_idle(self.send(self.message('pause','暂停自主回顾',expected='已暂停持续自主')))
        self.assertEqual(self.policy()['status'],'cancelled')
        before=json.dumps(self.tasks(),sort_keys=True)
        self.move_window()
        self.run_idle(self.checkpoint('paused'),checkpoints={'paused':lambda:time.sleep(1.3)})
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),before)
        self.assertEqual(self.reflections,2)

    def test_operator_seed_uses_existing_identity_and_preserves_pause_after_focus_eviction(self):
        self.callback=lambda _:text_response('记得这次真实交流')
        self.run_idle(self.send(self.message('history','给足自由，持续自主思考和行动。',expected='记得这次真实交流')))
        history_before=json.dumps(self.turn('给足自由，持续自主思考和行动。'),sort_keys=True)
        events=[e for e in self.subject()['events'] if e['input']['source']=='user_ingress']
        self.assertTrue(events)
        scope=events[-1]['input']['scope']
        seed={'version':1,'authorization_id':'test-operator-1','scope':scope,'max_goals':1,
            'steps_per_goal':10,'interests':['从真实经历选择方向'],
            'review_messages':['给足自由，持续自主思考和行动。']}
        config=self.work/'operator-autonomy.json';config.write_text(json.dumps(seed,ensure_ascii=False))
        env={'EVE_HOST_AUTONOMY_CONFIG':str(config)}
        self.run_idle(self.checkpoint('installed'),env_extra=env,
            checkpoints={'installed':lambda:self.wait_for(lambda:any(f['id']==POLICY for f in self.subject()['foci']),'宿主授权未接入',10)})
        self.assertEqual((self.grant()['issued'],self.grant()['window']['used']),(0,0))
        self.assertEqual(json.dumps(self.turn('给足自由，持续自主思考和行动。'),sort_keys=True),history_before)
        self.assertEqual(len([e for e in self.subject()['events'] if e['input']['source']=='user_ingress']),len(events))
        self.callback=super().respond;self.due()
        self.run_idle(self.checkpoint('seeded-review'),env_extra=env,checkpoints={'seeded-review':lambda:self.wait_revision(1)})
        first=self.grant()
        completed_before=json.dumps(self.tasks(),sort_keys=True)
        # 同一新版程序的最小恢复组合仍保留启动配置，但不能重授预算或要求后台存在。
        self.run_eve(self.checkpoint('minimal-recovery'),interest=False,memory=True,research=False,
            practice=False,env_extra=env,checkpoints={'minimal-recovery':lambda:time.sleep(.2)})
        self.assertEqual(self.grant(),first)
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),completed_before)
        self.run_idle(self.checkpoint('no-regrant'),env_extra=env,checkpoints={'no-regrant':lambda:time.sleep(1.3)})
        self.assertEqual(self.grant(),first)
        # 暂停走真实工具入口；随后仅在停止的测试副本模拟已取消关注被热容量回收。
        def pause(body):
            if not self.results(body):return tool_response([('configure_subject_autonomy',{'action':'configure_autonomy','enabled':False})])
            return text_response('停止自主')
        self.callback=pause
        self.run_idle(self.send(self.message('stop','停止自主',expected='停止自主')),env_extra=env)
        self.assertEqual(self.policy()['status'],'cancelled')
        raw=json.loads(self.state_path.read_text());ledger=json.loads(bytes(raw['entries']['eve.subject']['subject.v1']))
        ledger['foci']=[f for f in ledger['foci'] if f['id']!=POLICY]
        raw['entries']['eve.subject']['subject.v1']=list(json.dumps(ledger).encode());self.state_path.write_text(json.dumps(raw))
        tasks_before=json.dumps(self.tasks(),sort_keys=True)
        self.run_idle(self.checkpoint('evicted'),env_extra=env,checkpoints={'evicted':lambda:time.sleep(1.3)})
        self.assertFalse(any(f['id']==POLICY for f in self.subject()['foci']))
        self.assertEqual(json.dumps(self.tasks(),sort_keys=True),tasks_before)
        with sqlite3.connect(self.state_path.parent/'self-interests.sqlite3') as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM host_authorizations').fetchone()[0],1)


if __name__=='__main__':unittest.main(verbosity=2)
