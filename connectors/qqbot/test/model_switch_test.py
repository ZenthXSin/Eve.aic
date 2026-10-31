"""真实 QQ 宿主：模型快捷切换、权限、在途快照、角色配置与重启恢复。"""
import json
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response


class ModelSwitchAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def enabled(self, script, role=False, **options):
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config),
               'EVE_OPENAI_MODEL': 'direct-a', 'EVE_OPENAI_MODEL_ROLE': 'primary' if role else ''}
        if role:
            env.update(EVE_MODELS_PRIMARY_ENABLED='true', EVE_MODELS_PRIMARY_PROVIDER='openai',
                       EVE_MODELS_PRIMARY_MODEL='role-a', EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS='128',
                       EVE_MODELS_PRIMARY_CREDENTIAL_REF='env:EVE_OPENAI_API_KEY')
        return self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                            env_extra=env, **options)

    def stored(self):
        return json.loads((self.work / 'state/configuration/config.json').read_text())

    def text(self, body):
        return next(message['content'] for message in reversed(body['messages'])
                    if message['role'] == 'user')

    def test_switch_changes_real_requests_and_survives_restart(self):
        self.callback = lambda body: text_response('模型=' + body['model'])
        self.enabled([*self.send(self.message('query', '/model', contains=['当前主模型：direct-a', '全局'])),
                      *self.send(self.message('switch', '/model org/model-b:fast', contains=['主模型已设为：org/model-b:fast', '已保存'])),
                      *self.send(self.message('chat', '测试切换', expected='模型=org/model-b:fast'))])
        self.assertEqual(self.chat_rounds, 1, '指令不调用模型或探测供应商')
        current = self.stored()['current']
        self.assertEqual(current['namespaces']['provider.openai']['values'], {'model': 'org/model-b:fast'})
        self.enabled([*self.send(self.message('query-restored', '/model', contains='当前主模型：org/model-b:fast')),
                      *self.send(self.message('restored', '重启后测试', expected='模型=org/model-b:fast'))])
        self.assertEqual(self.chat_rounds, 2)
        self.assertEqual(self.stored()['current'], current)

    def test_primary_role_updates_only_the_selected_model_field(self):
        def respond(body):
            self.assertEqual(body['max_tokens'], 128)
            return text_response('角色=' + body['model'])
        self.callback = respond
        self.enabled([*self.send(self.message('query-role', '/model', contains='当前主模型：role-a')),
                      *self.send(self.message('switch-role', '/model role-b', contains='主模型已设为：role-b')),
                      *self.send(self.message('role-chat', '角色测试', expected='角色=role-b'))], role=True)
        current = self.stored()['current']
        self.assertEqual(current['namespaces']['runtime.models']['values'], {'primary_model': 'role-b'})
        self.assertNotIn('provider.openai', current['namespaces'])
        self.enabled(self.send(self.message('role-restored', '角色重启测试', expected='角色=role-b')), role=True)
        self.assertEqual(self.chat_rounds, 2)

    def test_ordinary_user_and_invalid_arguments_cannot_change_configuration(self):
        self.callback = lambda body: text_response('仍为=' + body['model'])
        self.enabled(self.send(self.message('seed', '/model model-b', contains='主模型已设为：model-b')))
        before = self.stored()
        self.enabled([*self.send(self.message('public-query', '/model', user='user-2', contains='当前主模型：model-b')),
                      *self.send(self.message('denied', '/model unauthorized-model', user='user-2', contains='需要主人或管理员授权')),
                      *self.send(self.message('too-many', '/model a b', contains='用法：/model')),
                      *self.send(self.message('multiline', '/model\ninvalid-model', contains='用法：/model')),
                      *self.send(self.message('too-long', '/model ' + 'x' * 201, contains='用法：/model')),
                      *self.send(self.message('same', '/model model-b', contains='配置未改动')),
                      *self.send(self.message('prefix', '/modelname', contains='当前会话没有可控制的任务')),
                      *self.send(self.message('unchanged', '核对配置未变', expected='仍为=model-b'))])
        self.assertEqual(self.stored(), before)
        self.assertEqual(self.chat_rounds, 1)

    def test_running_turn_keeps_its_model_and_next_turn_uses_the_new_one(self):
        self.response_gates[('chat', 2)] = 'release-old-model'
        def respond(body):
            if self.text(body) == '运行中的旧任务':
                self.assertEqual(body['model'], 'direct-a')
                if not self.results(body):
                    return tool_response([('echo', {'text': '工具只执行一次'})])
                return text_response('旧任务=direct-a')
            self.assertEqual(sum(m['role'] == 'tool' for m in body['messages']), 1)
            return text_response('新任务=' + body['model'])
        self.callback = respond
        old = self.message('old', '运行中的旧任务', expected='旧任务=direct-a')
        self.enabled([{'send': old}, {'wait_file': str(self.arrived('chat', 2))},
                      *self.send(self.message('switch-running', '/model model-b', contains='主模型已设为：model-b')),
                      {'touch': str(self.gate('release-old-model'))}, self.wait_reply('old'), self.wait_receipt('old'),
                      *self.send(self.message('next', '新任务测试', expected='新任务=model-b'))])
        self.assertEqual([r['body']['model'] for r in self.kind('chat')], ['direct-a', 'direct-a', 'model-b'])


if __name__ == '__main__':
    unittest.main()
