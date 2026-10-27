"""真实进程：无新用户指令也检查磁盘并通知主人一次；跨重启去重、普通成员不收取。"""
import hashlib
import json
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

    def enabled_health(self,script,**options):
        # 阈值超过测试机磁盘，实际 statvfs 返回的可用量必低于它；不制造真实磁盘占满。
        env={'EVE_DEVELOPER_CONFIG':str(self.developer_config),'EVE_HEALTH_MIN_FREE_BYTES':str(2**63),'EVE_HEALTH_CHECK_MS':'1000'}
        return self.run_eve(script,interest=False,memory=True,research=False,practice=False,panel=True,
                            more=['--health-watch'],env_extra=env,**options)

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


if __name__=='__main__':unittest.main()
