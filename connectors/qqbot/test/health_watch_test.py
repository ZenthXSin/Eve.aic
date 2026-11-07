"""真实进程：无新用户指令也检查磁盘并通知主人一次；跨重启去重、普通成员不收取。"""
import hashlib
import json
import time
import unittest
import reminder_test
import developer_tools_test as developer
from background_task_test import calls
from dialogue_tools_test import text_response


class HealthWatchAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name
    events=reminder_test.ReminderAcceptance.events
    pushes=reminder_test.ReminderAcceptance.pushes

    def setUp(self):
        super().setUp()
        principal='qq:'+hashlib.sha256(json.dumps(['1904159860','group','group-1','user-1'],separators=(',',':')).encode()).hexdigest()
        config=json.loads(self.developer_config.read_text());config['operators'].append(principal);self.developer_config.write_text(json.dumps(config))
        self.principal=principal
        self.speech_requests=[]
        self.speech_decision=lambda facts:{'action':'silence'}

    def enabled_health(self,script,min_free_bytes=2**63,**options):
        # 阈值超过测试机磁盘，实际 statvfs 返回的可用量必低于它；不制造真实磁盘占满。
        env={'EVE_DEVELOPER_CONFIG':str(self.developer_config),'EVE_HEALTH_MIN_FREE_BYTES':str(min_free_bytes),'EVE_HEALTH_CHECK_MS':'1000'}
        env.update(options.pop('env_extra',{}))
        return self.run_eve(script,interest=False,memory=True,research=False,practice=False,panel=True,
                            more=['--health-watch'],env_extra=env,**options)

    def provider_response(self,body,kind,fallback):
        try:
            facts=json.loads(body['messages'][-1].get('content') or 'null')
        except (TypeError,ValueError):
            facts=None
        if isinstance(facts,dict) and 'host_event' in facts:
            self.assertFalse(body.get('tools'))
            self.speech_requests.append(facts)
            return text_response(json.dumps(self.speech_decision(facts),ensure_ascii=False))
        return super().provider_response(body,kind,fallback)

    def health(self):
        path=self.work/'state/health-watch.json'
        return json.loads(path.read_text()) if path.exists() else None

    def seed_owner_route(self):
        self.callback=lambda _:text_response('收到')
        self.enabled_health([*self.checkpoint('bind-health-owner'),
            *self.send(self.message('health-route','正常聊天',expected='收到',scope='group',target='group-1'))],
            min_free_bytes=1,checkpoints={'bind-health-owner':self.bind_owner})

    def seed_pending(self,code,checked_at_ms=1):
        # 只在已退出的离线夹具写旧格式积压，不触碰实际运行状态。
        finding={'code':code,'summary':'旧巡检异常'}
        (self.work/'state/health-watch.json').write_text(json.dumps({
            'version':1,'next_id':2,'checked_at_ms':checked_at_ms,'active':[finding],
            'notices':[{'id':'health-1','at_ms':1,'findings':[finding],
                        'owner':None,'delivery':'Pending'}]}))

    def bind_owner(self):
        import memory_test
        status,current=memory_test.MemoryAcceptance.panel(self,'/api/plugin-pages/read',body={'plugin_id':'eve.config','page_id':'runtime.people'})
        self.assertEqual(status,200,current)
        status,result=memory_test.MemoryAcceptance.panel(self,'/api/plugin-pages/save',body={
            'plugin_id':'eve.config','page_id':'runtime.people','instance':current['instance'],'expected_revision':current['revision'],'values':{'owner_principals':self.principal}})
        self.assertEqual(status,200,result)

    @unittest.skipUnless(__import__('os').name=='posix','statvfs disk inspection')
    def test_idle_inspection_notifies_only_owner_once_and_restart_does_not_repeat(self):
        def respond(body):
            results=self.results(body)
            if next(m.get('content') for m in reversed(body['messages']) if m['role']=='user')=='看运行健康':
                if not results:return calls(('health','inspect_runtime_health',{}))
            if results:
                self.assertTrue(any(f['code']=='disk.low' for f in results[-1]['active']))
                return text_response('发现磁盘阈值告警')
            return text_response('收到')
        self.callback=respond
        def wait_notice():
            self.wait_for(lambda:len(self.pushes())==1,'未主动通知主人',12)
        route={'scope':'group','target':'group-1'}
        self.enabled_health([*self.send(self.message('other','其他成员消息',user='user-2',expected='收到',**route)),
            *self.checkpoint('before-owner'),*self.checkpoint('bind'),
            *self.send(self.message('owner','正常聊天',expected='收到',**route)),*self.checkpoint('notice'),
            *self.send(self.message('health','看运行健康',expected='发现磁盘阈值告警',**route))],checkpoints={
                'before-owner':lambda:self.assertEqual(self.pushes(),[]),'bind':self.bind_owner,'notice':wait_notice})
        [notice]=self.pushes()
        self.assertEqual((notice['scope'],notice['target_id'],notice['mention_user_id']),('group','group-1','user-1'))
        self.assertIn('磁盘',notice['text'])
        requests=len(self.requests)
        self.enabled_health([*self.checkpoint('quiet')],checkpoints={'quiet':self.quiet})
        self.assertEqual(len(self.pushes()),1)
        self.assertEqual(len(self.requests),requests,'巡检不请求模型')

    def test_member_cannot_read_host_health_records(self):
        def respond(body):
            results=self.results(body)
            if not results:return calls(('health','inspect_runtime_health',{}))
            self.assertNotIn('active',results[-1])
            self.assertIn('主人或管理员',json.dumps(results[-1],ensure_ascii=False))
            return text_response('没有宿主巡检权限')
        self.callback=respond
        self.enabled_health(self.send(self.message('member','看运行健康',user='user-2',expected='没有宿主巡检权限')))
        self.assertEqual(self.pushes(),[])

    @unittest.skipUnless(__import__('os').name=='posix','statvfs disk inspection')
    def test_recovered_pending_incident_never_calls_speech_and_new_fault_still_can_speak(self):
        self.seed_owner_route()
        self.seed_pending('disk.low')
        before=len(self.requests)
        def retired():
            self.wait_for(lambda:self.health()['notices'][0]['delivery']=='Retired',
                          '恢复后的待发事件没有退役',12)
            notice=self.health()['notices'][0]
            self.assertIsNone(notice['owner'])
            self.assertEqual(notice['resolutions'][0]['reason'],{'kind':'recovered'})
            time.sleep(1.1)
        live={'EVE_QQ_LIVE_INTERACTION':'true'}
        self.enabled_health(self.checkpoint('retired'),min_free_bytes=1,
                            env_extra=live,checkpoints={'retired':retired})
        self.assertEqual(len(self.requests),before,'恢复事件不应启动发言或主模型')
        self.assertEqual(self.speech_requests,[])
        self.assertEqual(self.pushes(),[])
        old=self.health()['notices'][0]
        self.enabled_health([{'delay_ms':1500}],min_free_bytes=1,env_extra=live)
        self.assertEqual(self.health()['notices'][0],old)
        self.assertEqual(len(self.requests),before,'重启不重新处理退役事件')
        def decide(facts):
            event=facts['host_event']
            self.assertEqual(event['kind'],'health_findings')
            self.assertEqual([f['code'] for f in event['findings']],['disk.low'])
            self.assertEqual(event['unknown'],[])
            return {'action':'speak','text':'当前磁盘阈值异常，需要检查。','after_ms':0}
        self.speech_decision=decide
        self.enabled_health(self.checkpoint('new-incident'),env_extra=live,
            checkpoints={'new-incident':lambda:self.wait_for(lambda:len(self.pushes())==1,
                '新故障没有保留通知机会',12)})
        self.assertEqual(len(self.speech_requests),1)
        self.assertEqual(self.health()['notices'][0],old)
        self.assertEqual(self.health()['notices'][1]['id'],'health-2')
        self.assertEqual(self.health()['notices'][1]['delivery'],'Sent')

    def test_restart_without_model_observation_preserves_unknown_until_real_success(self):
        self.seed_owner_route()
        self.seed_pending('model.dialogue',int(time.time()*1000))
        before=len(self.requests)
        def unknown():
            self.wait_for(lambda:any(f['code']=='model.dialogue' for f in self.health().get('unknown',[])),
                          '重启后的模型状态没有保留为未知',12)
            self.assertEqual(self.health()['active'],[])
            self.assertEqual(self.health()['notices'][0]['delivery'],'Pending')
            self.assertEqual(self.health()['notices'][0].get('resolutions',[]),[])
            time.sleep(1.1)
        live={'EVE_QQ_LIVE_INTERACTION':'true','EVE_HEALTH_CHECK_MS':'3600000'}
        self.enabled_health(self.checkpoint('unknown'),min_free_bytes=1,
                            env_extra=live,checkpoints={'unknown':unknown})
        self.assertEqual(len(self.requests),before,'未知旧故障不得自行触发模型')
        self.assertEqual(self.speech_requests,[])
        self.assertEqual(self.pushes(),[])
        self.callback=lambda _:text_response('实际模型调用成功')
        def recovered():
            self.wait_for(lambda:self.health()['notices'][0]['delivery']=='Retired',
                          '实际成功后旧模型事件没有退役',12)
        self.enabled_health([*self.send(self.message('real-success','继续聊天',expected='实际模型调用成功',
            scope='group',target='group-1')),*self.checkpoint('model-recovered')],min_free_bytes=1,
            checkpoints={'model-recovered':recovered})
        self.assertEqual(self.health()['notices'][0]['resolutions'][0]['reason'],{'kind':'recovered'})
        self.assertEqual(self.pushes(),[])


if __name__=='__main__':unittest.main()
