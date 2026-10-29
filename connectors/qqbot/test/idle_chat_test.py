"""真实宿主、假 QQ 与本地模型验收：普通朋友闲聊邀请及不重放边界，不发送真实消息。"""
import unittest
from outreach_test import OutreachAcceptance, CHAT

class IdleChatAcceptance(OutreachAcceptance):
    for name in dir(OutreachAcceptance):
        if name.startswith('test_'): locals()[name]=None
    del name

    def test_non_admin_friend_invitation_and_optout_survive_restart(self):
        def compose(request):
            self.assertEqual([f['kind'] for f in request['facts']], ['IdleContext'])
            self.assertIn('真实私聊', request['facts'][0]['text'])
            return {'text':'最近怎么样？有空可以聊聊。'}
        self.compose=compose
        env={'EVE_IDLE_CHAT_AFTER_MS':'1','EVE_ADMIN_BIND_KEY':'synthetic-local-bind-key-not-used'}
        self.run_eve([*self.send(self.message('role','/admin',contains='普通用户')),
                      *self.send(self.message('chat',CHAT)),*self.checkpoint('invited'),
                      *self.send(self.message('off','/outreach off',contains='不再'))],
                     checkpoints={'invited':lambda:self.wait_status('Delivered',80)},
                     env_extra=env, timeout=100)
        [invitation]=self.invitations()
        self.assertEqual(invitation['origin']['kind'],'IdleChat')
        self.assertIsNone(invitation['milestone'])
        self.assertTrue(invitation['goal_id'].startswith('chat-'))
        self.assertEqual(len(self.goals()),0,'闲聊不能伪造认知目标')
        [push]=self.pushes()
        self.assertEqual(push['target_id'],'user-1')
        self.assertEqual(len(invitation['attempts']),1)
        before=self.ledger()
        self.run_eve([*self.send(self.message('status','/outreach',contains='关闭')),
                      *self.checkpoint('quiet')],checkpoints={'quiet':self.quiet},env_extra=env)
        self.assertEqual(self.ledger(),before)
        self.assertEqual(len(self.pushes()),1,'重启不得重发')

if __name__=='__main__': unittest.main()
