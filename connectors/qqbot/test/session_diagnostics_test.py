"""真实宿主的普通失败轮次查询、原始回执、重启及范围隔离；只用本地模型/QQ替身。"""
import json
import sys
import unittest

from dialogue_tools_test import text_response, tool_response
import model_timeout_test as timeout_fixture


def invoke(arguments, call_id):
    response = tool_response([('read_context_record', arguments)])
    response['choices'][0]['message']['tool_calls'][0]['id'] = call_id
    return response


class SessionDiagnosticsAcceptance(timeout_fixture.ModelTimeoutAcceptance):
    for _name in dir(timeout_fixture.ModelTimeoutAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def test_saved_failed_turn_can_be_inspected_after_restart_without_replaying_command(self):
        def fail_after_tool(body):
            if not self.results(body):
                return tool_response([('run_command', {'program': sys.executable, 'args': ['-c',
                    "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('一次执行')"]})])
            self.mode = 'stall'
            return text_response('PARTIAL_NEVER_DELIVER')
        self.callback = fail_after_tool
        self.enabled(self.send(self.message('failure', '运行后断流', contains=['响应超时', '已启动 1 次'])))
        self.assertEqual((self.source/'once.txt').read_text(), 'x')
        original = self.turn('运行后断流')['status']
        self.assertEqual(original['state'], 'Failed')
        self.assertEqual(original['failure']['code'], 'ProviderTimeout')
        references = []

        def inspect(body):
            results = self.results(body)
            query = next(message['content'] for message in reversed(body['messages']) if message['role'] == 'user')
            if query == 'OTHER_SCOPE':
                if not results:
                    return invoke({'action': 'read', 'reference': references[0]}, 'read-original-turn')
                self.assertNotIn('record_json_fragment', results[-1])
                self.assertNotIn('一次执行', json.dumps(results[-1], ensure_ascii=False))
                return text_response('没有当前会话的该记录')
            if not results:
                return invoke({'action': 'recent', 'limit': 3}, 'recent-finished-turns')
            if len(results) == 1:
                self.assertEqual(results[0]['scope'], 'current_only')
                failed = [record for record in results[0]['records'] if record['status'] == 'Failed']
                self.assertEqual(len(failed), 1)
                record = failed[0]
                self.assertEqual(record['turn_id'], 1)
                self.assertEqual(record['failure_code'], 'ProviderTimeout')
                self.assertEqual(record['started_tools'], 1)
                self.assertEqual(record['recorded_tool_receipts'], 1)
                self.assertEqual(record['tool_names_preview'], ['run_command'])
                self.assertFalse(record['automatic_replay'])
                if references:
                    self.assertEqual(record['reference'], references[0])
                else:
                    references.append(record['reference'])
                return invoke({'action': 'read', 'reference': references[0]}, 'read-original-turn')
            self.assertEqual(len(results), 2)
            self.assertIsNone(results[-1]['next_offset'])
            raw = json.loads(results[-1]['record_json_fragment'])
            self.assertEqual(raw['status'], original)
            self.assertEqual(results[-1]['sha256'], references[0].rsplit(':', 1)[1])
            return text_response('写入已完成，后续模型断流，未重跑')

        self.mode = 'healthy'
        self.callback = inspect
        self.enabled(self.send(self.message('inspect', '查看上轮失败', expected='写入已完成，后续模型断流，未重跑')))
        self.enabled(self.send(self.message('restart-inspect', '重启后再查', expected='写入已完成，后续模型断流，未重跑')))
        self.enabled(self.send(self.message('foreign', 'OTHER_SCOPE', user='user-2', expected='没有当前会话的该记录')))
        self.assertEqual((self.source/'once.txt').read_text(), 'x')
        self.assertEqual(self.turn('运行后断流')['status'], original)
        self.assertEqual(len(references), 1)


if __name__ == '__main__':
    unittest.main()
