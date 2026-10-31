"""实际宿主中的普通成员具体权限查询；查询不创建业务文件、审核请求或文件投递。"""
import json
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response


class PermissionInspectionAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def test_member_preflights_actual_arguments_and_delivery_without_executing_or_forging_identity(self):
        config = json.loads(self.developer_config.read_text())
        config['max_tool_rounds'] = 12
        self.developer_config.write_text(json.dumps(config))
        (self.source / 'host-only.txt').write_text('HOST_PRIVATE_MARKER')
        def tools(operations):
            result = tool_response(operations)
            for call in result['choices'][0]['message']['tool_calls']:
                call['id'] += f'-phase-{self.chat_rounds}'
            return result
        good = {'path': 'exports/checked.txt', 'content': 'member-output', 'expected_sha256': ''}
        def respond(body):
            self.assertIn('inspect_tool_permission', self.definitions(body))
            results = self.results(body)
            if not results:
                return tools([
                    ('inspect_tool_permission', {'name': 'write_file', 'arguments': good}),
                    ('inspect_tool_permission', {'name': 'write_file', 'arguments': {'path': 'checked.txt'}}),
                    ('inspect_tool_permission', {'name': 'clear_context', 'arguments': {}}),
                    ('inspect_tool_permission', {'name': 'read_file', 'arguments': {'path': 'README.md'}, 'user_id': 'user-1'}),
                ])
            if len(results) == 4:
                self.assertTrue(results[0]['allowed'])
                self.assertFalse(results[0]['executed'])
                self.assertFalse(results[0]['authorization_reserved'])
                self.assertFalse(results[0]['review_verified'])
                self.assertFalse(results[1]['allowed'])
                self.assertIn('error', results[2])
                self.assertIn('error', results[3])
                self.assertFalse((self.source / '.eve-members').exists(), '预检不能初始化成员项目')
                return tools([('write_file', good)])
            if len(results) == 5:
                self.assertTrue(results[-1]['success'])
                self.assertEqual([p.read_text() for p in self.source.glob('.eve-members/*/exports/checked.txt')], ['member-output'])
                return tools([
                    ('inspect_tool_permission', {'name': 'send_file', 'arguments': {'path': 'exports/checked.txt'}}),
                    ('inspect_tool_permission', {'name': 'send_file', 'arguments': {'path': str(self.source / 'host-only.txt')}}),
                ])
            self.assertEqual(len(results), 7)
            self.assertTrue(results[-2]['allowed'])
            self.assertFalse(results[-2]['executed'])
            self.assertIn('error', results[-1])
            self.assertNotIn('HOST_PRIVATE_MARKER', json.dumps(results))
            return text_response('仅实际写入生成文件，交付权限查询没有发送文件')
        self.callback = respond
        self.run_eve(self.send(self.message('inspect-member', '预检后保存自己的文件，只查询交付权限',
            user='user-2', expected='仅实际写入生成文件，交付权限查询没有发送文件')),
            interest=False, memory=False, research=False, practice=False,
            env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config)})
        self.assertEqual(len(self.kind('review')), 0)
        self.assertEqual(self.chat_rounds, 4)
        self.assertFalse((self.source / 'exports/checked.txt').exists(), '成员操作不能写入宿主项目')
        self.assertEqual((self.source / 'host-only.txt').read_text(), 'HOST_PRIVATE_MARKER')
        ledger = self.work / 'state/qq-outbox/ledger.json'
        if ledger.exists():
            self.assertEqual(json.loads(ledger.read_text()).get('records', []), [])
        self.assertEqual(list((self.work / 'state/qq-outbox/files').iterdir()), [])


if __name__ == '__main__':
    unittest.main()
