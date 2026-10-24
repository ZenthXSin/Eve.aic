"""实际 QQ 工具协议：按发送者隔离重置、管理员全局重置、重启保留边界和原始记录。"""
import copy
import json
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response
from request_messages import conversation_messages


class ContextResetAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def run_reset(self, script):
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config)})

    def users(self, body):
        return [m['content'] for m in conversation_messages(body['messages']) if m['role'] == 'user']

    def test_personal_and_global_boundaries_survive_restart_without_deleting_turns(self):
        checks = {}

        def respond(body):
            users = self.users(body)
            current = users[-1]
            if current in ('清空我的群上下文', '管理员清空全部'):
                results = self.results(body)
                if not results:
                    return tool_response([('clear_context' if current == '清空我的群上下文' else 'clear_all_contexts', {})])
                self.assertTrue(results[-1]['cleared'])
                self.assertTrue(results[-1]['raw_history_preserved'])
                return text_response('已清空')
            if current in checks:
                required, excluded = checks[current]
                for text in required:
                    self.assertIn(text, users)
                for text in excluded:
                    self.assertNotIn(text, users)
            return text_response('收到')

        self.callback = respond
        group_a = lambda id, text: self.message(id, text, user='user-2', scope='group', expected='收到')
        group_b = lambda id, text: self.message(id, text, user='user-3', scope='group', expected='收到')
        private_a = lambda id, text: self.message(id, text, user='user-2', expected='收到')
        self.run_reset([*self.send(group_a('a-old', '甲在本群的旧话题')),
                        *self.send(group_b('b-old', '乙在本群的旧话题')),
                        *self.send(private_a('a-private', '甲的私聊旧话题'))])
        originals = {text: copy.deepcopy(self.turn(text)) for text in ['甲在本群的旧话题', '乙在本群的旧话题', '甲的私聊旧话题']}
        checks.update({'甲清空后': ([], ['甲在本群的旧话题', '清空我的群上下文']),
                       '乙仍有上下文': (['乙在本群的旧话题'], ['甲在本群的旧话题']),
                       '甲私聊仍保留': (['甲的私聊旧话题'], ['甲在本群的旧话题'])})
        self.run_reset([*self.send(self.message('clear-a', '清空我的群上下文', user='user-2', scope='group', expected='已清空')),
                        *self.send(group_a('a-after', '甲清空后')),
                        *self.send(group_b('b-after', '乙仍有上下文')),
                        *self.send(private_a('private-after', '甲私聊仍保留'))])
        for text, turn in originals.items():
            self.assertEqual(self.turn(text), turn)
        self.run_reset(self.send(self.message('clear-global', '管理员清空全部', expected='已清空')))
        old = list(originals) + ['清空我的群上下文', '甲清空后', '乙仍有上下文', '甲私聊仍保留', '管理员清空全部']
        checks.update({text: ([], old) for text in ['甲全局重启后', '乙全局重启后', '甲私聊全局后', '管理员全局后']})
        self.run_reset([*self.send(group_a('a-final', '甲全局重启后')),
                        *self.send(group_b('b-final', '乙全局重启后')),
                        *self.send(private_a('private-final', '甲私聊全局后')),
                        *self.send(self.message('admin-final', '管理员全局后', expected='收到'))])
        for text, turn in originals.items():
            self.assertEqual(self.turn(text), turn)
        self.assertEqual(sum(r['message']['id'].startswith('clear-') for r in self.documents()['eve.channel.qqbot']['receipts.v1']['entries']), 2)

    def test_non_admin_global_and_forged_personal_target_are_rejected_without_changing_boundaries(self):
        def respond(body):
            current = self.users(body)[-1]
            if not self.results(body):
                if current == '普通用户清全局':
                    return tool_response([('clear_all_contexts', {})])
                if current == '指定其他用户':
                    return tool_response([('clear_context', {'user_id': 'user-1'})])
                return text_response('收到')
            result = self.results(body)[-1]
            self.assertFalse(result.get('cleared', False))
            self.assertNotIn('context-resets.v1', self.documents().get('eve.session', {}))
            return text_response('没有清空')

        self.callback = respond
        self.run_reset([*self.send(self.message('seed', '不要清掉的原话', expected='收到')),
                        *self.send(self.message('denied', '普通用户清全局', user='user-2', scope='group', expected='没有清空')),
                        *self.send(self.message('forged', '指定其他用户', user='user-2', scope='group', expected='没有清空'))])
        self.assertEqual(self.turn('不要清掉的原话')['status']['state'], 'Completed')


if __name__ == '__main__':
    unittest.main()
