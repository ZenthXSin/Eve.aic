"""真实 QQ 宿主：模型快捷切换、权限、在途快照、角色配置与重启恢复。"""
import json
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import developer_tools_test as developer
import memory_test
from dialogue_tools_test import text_response, tool_response


class ModelSwitchAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def enabled(self, script, role=False, auxiliary=False, **options):
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config),
               'EVE_OPENAI_MODEL': 'direct-a', 'EVE_OPENAI_MODEL_ROLE': 'primary' if role else ''}
        if role:
            env.update(EVE_MODELS_PRIMARY_ENABLED='true', EVE_MODELS_PRIMARY_PROVIDER='openai',
                       EVE_MODELS_PRIMARY_MODEL='role-a', EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS='128',
                       EVE_MODELS_PRIMARY_CREDENTIAL_REF='env:EVE_OPENAI_API_KEY')
        if auxiliary:
            env.update(EVE_MODEL_USE_DIALOGUE_ROLE='auxiliary_small',
                       EVE_MODELS_AUXILIARY_SMALL_ENABLED='true',
                       EVE_MODELS_AUXILIARY_SMALL_PROVIDER='openai',
                       EVE_MODELS_AUXILIARY_SMALL_MODEL='auxiliary-a',
                       EVE_MODELS_AUXILIARY_SMALL_MAX_OUTPUT_TOKENS='96')
        env.update(options.pop('env_extra', {}))
        return self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                            env_extra=env, **options)

    def stored(self):
        return json.loads((self.work / 'state/configuration/config.json').read_text())

    def text(self, body):
        return next(message['content'] for message in reversed(body['messages'])
                    if message['role'] == 'user')

    def test_member_can_inspect_configured_model_without_probe_writes_or_admin_access(self):
        def respond(body):
            self.assertIn('inspect_model_runtime', self.definitions(body))
            results = self.results(body)
            if not results:
                return tool_response([('inspect_model_runtime', {})])
            self.assertEqual(len(results), 1)
            result = results[0]
            self.assertEqual(result['purpose'], 'dialogue')
            self.assertEqual(result['configured']['role'], 'primary')
            self.assertEqual(result['configured']['model'], 'role-a')
            self.assertEqual(result['configured']['max_output_tokens'], 128)
            self.assertEqual(result['configured']['max_concurrent_requests'], 1)
            self.assertEqual(result['admission']['limit'], 1)
            self.assertEqual(result['admission']['active'], 0)
            self.assertEqual(result['admission']['waiting'], 0)
            self.assertFalse(result['configuration_written'])
            self.assertFalse(result['provider_availability_verified'])
            self.assertFalse(result['inflight_model_known'])
            encoded = json.dumps(result)
            for forbidden in ('api_key', 'credential_ref', 'base_url', 'user-2', 'test-model-secret'):
                self.assertNotIn(forbidden, encoded)
            return text_response('当前配置role-a，没有排队请求')
        self.callback = respond
        for index in range(2):
            self.enabled(self.send(self.message('inspect-' + str(index), '检查当前模型', user='user-2',
                                                expected='当前配置role-a，没有排队请求')), role=True)
            configuration = self.work / 'state/configuration/config.json'
            self.assertFalse(configuration.exists(), '只读查询不创建或保存配置')
        self.assertEqual(self.chat_rounds, 4, '两轮各一次对话和一次工具后续，无额外探测或重试')

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

    def test_auxiliary_dialogue_route_switches_actual_selected_role_and_survives_restart(self):
        def respond(body):
            self.assertEqual(body['max_tokens'], 96)
            return text_response('角色=' + body['model'])
        self.callback = respond
        self.enabled([*self.send(self.message('aux-query', '/model', contains='当前主模型：auxiliary-a')),
                      *self.send(self.message('aux-switch', '/model auxiliary-b', contains='主模型已设为：auxiliary-b')),
                      *self.send(self.message('aux-chat', '辅助路由测试', expected='角色=auxiliary-b'))], auxiliary=True)
        current = self.stored()['current']
        self.assertEqual(current['namespaces']['runtime.models']['values'], {'auxiliary_small_model': 'auxiliary-b'})
        self.assertNotIn('provider.openai', current['namespaces'])
        self.enabled([*self.send(self.message('aux-restored', '重启后辅助路由', expected='角色=auxiliary-b'))], auxiliary=True)
        self.assertEqual(self.stored()['current'], current)

    def test_independent_auxiliary_server_receives_only_its_key_and_restores_conversation(self):
        received = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                received.append({'path': self.path, 'key': self.headers.get('Authorization'), 'body': body})
                data = json.dumps(text_response('独立=' + body['model'])).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        env = {'EVE_OPENAI_AUXILIARY_BASE_URL': f'http://127.0.0.1:{server.server_address[1]}/v1',
               'EVE_OPENAI_AUXILIARY_PROTOCOL': 'chat',
               'EVE_AUXILIARY_API_KEY': 'auxiliary-fixture-key',
               'EVE_MODELS_AUXILIARY_SMALL_CREDENTIAL_REF': 'env:EVE_AUXILIARY_API_KEY'}
        try:
            self.enabled(self.send(self.message('independent-first', '第一次独立模型', expected='独立=auxiliary-a')),
                         auxiliary=True, env_extra=env)
            self.enabled(self.send(self.message('independent-restored', '重启后继续独立模型', expected='独立=auxiliary-a')),
                         auxiliary=True, env_extra=env)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        self.assertEqual(len(received), 2)
        self.assertTrue(all(r['path'] == '/v1/chat/completions' for r in received))
        self.assertTrue(all(r['key'] == 'Bearer auxiliary-fixture-key' for r in received))
        self.assertTrue(all(r['body']['max_tokens'] == 96 for r in received))
        self.assertTrue(any(m.get('content') == '第一次独立模型' for m in received[1]['body']['messages']))
        self.assertEqual(self.kind('chat'), [], '主端点不接收独立辅助对话')

    def test_unused_invalid_auxiliary_key_does_not_block_primary_dialogue(self):
        self.callback = lambda body: text_response('模型=' + body['model'])
        self.enabled(self.send(self.message('unused-bad-key', '普通对话继续', expected='模型=direct-a')),
                     env_extra={'EVE_AUXILIARY_API_KEY': 'invalid synthetic key'})
        self.assertEqual(len(self.kind('chat')), 1)

    def test_primary_role_active_request_is_visible_without_changing_configuration(self):
        self.response_gates[('chat', 1)] = 'release-concurrency'
        self.callback = lambda body: text_response('完成：' + self.text(body))
        first = self.message('shared-first', '第一条', expected='完成：第一条')

        def admission():
            status, body = memory_test.MemoryAcceptance.panel(self, '/api/readiness')
            self.assertEqual(status, 200, body)
            return next(r['admission'] for r in body['roles'] if r['role'] == 'dialogue')

        def inspect_active():
            configuration = self.work / 'state/configuration/config.json'
            before = configuration.read_bytes() if configuration.exists() else None
            seen = admission()
            self.assertEqual((seen['role'], seen['configured_limit'], seen['observed_limit']), ('primary', 1, 1))
            self.assertEqual((seen['active'], seen['waiting']), (1, 0))
            self.assertEqual(len(self.kind('chat')), 1)
            self.assertEqual(configuration.read_bytes() if configuration.exists() else None, before, '诊断读取不修改配置')

        def settled():
            self.assertEqual((admission()['active'], admission()['waiting']), (0, 0))

        self.enabled([{'send': first}, {'wait_file': str(self.arrived('chat', 1))},
                      *self.checkpoint('active'), {'touch': str(self.gate('release-concurrency'))},
                      self.wait_reply(first['id']), self.wait_receipt(first['id']),
                      *self.checkpoint('settled')], role=True, panel=True,
                     checkpoints={'active': inspect_active, 'settled': settled})
        self.assertEqual(len(self.kind('chat')), 1)

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
