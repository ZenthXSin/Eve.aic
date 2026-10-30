"""真实 Eve 进程、本地模型/QQ 协议替身；不连接正式平台或收费模型。"""
import json
import unittest
import developer_tools_test as developer
import dialogue_tools_test as dialogue
from memory_continuity_test import cross_data


def invoke(name, arguments, call_id):
    reply = dialogue.tool_response([(name, arguments)])
    reply['choices'][0]['message']['tool_calls'][0]['id'] = call_id
    return reply


class ReasoningSharingAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def test_reasoning_defaults_low_freezes_current_turn_persists_and_rejects_member(self):
        self.fixture_admin_users = ('user-1',)
        def respond(body):
            user = [m['content'] for m in body['messages'] if m['role'] == 'user'][-1]
            results = self.results(body)
            if 'REASONING_SET' in user:
                self.assertEqual(body['reasoning_effort'], 'low', '当前轮的工具往返不能换档')
                if not results:
                    return invoke('set_reasoning_effort', {'effort':'high'}, 'set-high')
                self.assertTrue(results[-1]['saved'])
                return dialogue.text_response('思考强度已保存')
            self.assertEqual(body['reasoning_effort'], 'high', '下一轮及重启须使用持久设置')
            if 'MEMBER_SET' in user:
                if not results:
                    return invoke('set_reasoning_effort', {'effort':'low'}, 'member-low')
                self.assertNotEqual(results[-1].get('saved'), True)
                return dialogue.text_response('普通成员没有修改权限')
            return dialogue.text_response('仍为high')
        self.callback = respond
        self.enabled([*self.send(self.message('set', 'REASONING_SET 把思考强度设为high', expected='思考强度已保存')),
                      *self.send(self.message('next', 'CHECK_HIGH', expected='仍为high'))])
        self.enabled([*self.send(self.message('member', 'MEMBER_SET 改成low', user='user-2', expected='普通成员没有修改权限')),
                      *self.send(self.message('restart', 'CHECK_HIGH_AFTER_RESTART', expected='仍为high'))])

    def test_natural_group_sharing_review_recall_and_revoke_survive_restart(self):
        source = dict(scope='group', target='group-a', user='user-1')
        target = dict(scope='group', target='group-b', user='user-1')
        marker = 'VERIFIED_GROUP_HISTORY_MARKER'
        saved = {}
        reviews = []
        def review(request):
            reviews.append(request)
            self.assertEqual(request['program'], 'eve.manage_memory_sharing')
            self.assertIn('SHARING_GRANT', request['user_messages'][-1])
            arguments = json.loads(request['arguments'][0])
            self.assertEqual(arguments['source_scene'], 'group')
            self.assertEqual(arguments['target_scene'], 'group')
            return {'risk':'medium','authorization':'high','outcome':'allow','rationale':'用户明确同意当前群读取本人另一群经历'}
        self.review_reply = review
        def respond(body):
            user = [m['content'] for m in body['messages'] if m['role'] == 'user'][-1]
            results = self.results(body)
            if 'SEED_HISTORY' in user:
                return dialogue.text_response('这段经历已收到')
            if 'SHARING_GRANT' in user:
                if not results:
                    return invoke('manage_memory_sharing', {'action':'list'}, 'sharing-list')
                if len(results) == 1:
                    self.assertEqual(len(results[0]['sources']), 1)
                    return invoke('manage_memory_sharing', {'action':'grant', 'source':results[0]['sources'][0]['source'], 'interactions':True}, 'sharing-grant')
                self.assertTrue(results[-1]['saved'])
                saved['id'] = results[-1]['binding_id']
                return dialogue.text_response('共享已保存')
            if 'SHARING_REVOKE' in user:
                if not results:
                    return invoke('manage_memory_sharing', {'action':'revoke', 'binding_id':saved['id']}, 'sharing-revoke')
                self.assertTrue(results[-1]['revoked'])
                return dialogue.text_response('共享已撤销')
            data = cross_data(body)
            if 'BEFORE_REVOKE' in user:
                self.assertIn(marker, json.dumps(data))
                return dialogue.text_response('已从授权来源找到原话')
            self.assertNotIn(marker, json.dumps(data))
            return dialogue.text_response('来源已不再共享')
        self.callback = respond
        self.enabled([*self.send(self.message('seed', 'SEED_HISTORY '+marker, expected='这段经历已收到', **source))])
        self.enabled([*self.send(self.message('grant', 'SHARING_GRANT 允许在当前群使用我在另一个群的原始交流，只向当前群共享', expected='共享已保存', **target)),
                      *self.send(self.message('read', 'BEFORE_REVOKE '+marker, expected='已从授权来源找到原话', **target))])
        self.enabled([*self.send(self.message('revoke', 'SHARING_REVOKE 撤销刚才的共享', expected='共享已撤销', **target)),
                      *self.send(self.message('after', 'AFTER_REVOKE '+marker, expected='来源已不再共享', **target))])
        self.assertEqual(len(reviews), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
