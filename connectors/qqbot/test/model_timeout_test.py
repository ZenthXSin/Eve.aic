"""真实宿主的分阶段流式超时：慢流继续、断流保存工具结果、重启不重放。"""
import time
import sys
import unittest
import model_streaming_test as streaming
from dialogue_tools_test import text_response, tool_response


class ModelTimeoutAcceptance(streaming.ModelStreamingAcceptance):
    for _name in dir(streaming.ModelStreamingAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.mode = 'healthy'

    def enabled(self, script):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False, panel=True,
                     env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config),
                                'EVE_OPENAI_STREAM_COMPLETIONS': 'true',
                                'EVE_OPENAI_TIMEOUT_SECONDS': '1',
                                'EVE_MODELS_PRIMARY_TIMEOUT_MS': '1000',
                                'EVE_MODEL_FIRST_RESPONSE_TIMEOUT_MS': '300',
                                'EVE_MODEL_STREAM_IDLE_TIMEOUT_MS': '300',
                                'EVE_MODEL_STREAM_TOTAL_TIMEOUT_MS': '1600'})

    def write_provider_response(self, handler, body, encoded):
        try:
            if self.mode == 'healthy':
                # 总耗时超过旧总期限 1 秒，块间隔小于 300ms。
                width = max(1, (len(encoded) + 6) // 7)
                for index in range(0, len(encoded), width):
                    handler.wfile.write(encoded[index:index+width]); handler.wfile.flush()
                    if index + width < len(encoded):
                        time.sleep(.19)
            elif self.mode == 'stall':
                handler.wfile.write(encoded[:20]); handler.wfile.flush()
                # Partial bytes arrive, then stop long enough for the idle deadline.
                time.sleep(.5)
            elif self.mode == 'silent':
                time.sleep(.5)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def test_healthy_stream_outlives_old_total_timeout_and_delivers_full_text(self):
        self.callback = lambda _: text_response('慢流正常完成，只有完整结果交给 QQ')
        self.enabled(self.send(self.message('healthy', '慢流测试', expected='慢流正常完成，只有完整结果交给 QQ')))
        self.assertEqual(self.chat_rounds, 1)
        self.assertEqual(self.receipt('healthy')['state'], 'Sent')
        self.assertEqual(self.turn('慢流测试')['status']['state'], 'Completed')
        self.assertNotIn('EVE_MODEL_TIMEOUT', self.panel_stderr.read_text())

    def test_first_response_timeout_is_logged_without_partial_delivery(self):
        self.mode = 'silent'
        self.callback = lambda _: text_response('不应送达')
        self.enabled(self.send(self.message('silent', '无首包测试', contains=['响应超时', '尚未启动工具调用'])))
        self.assertIn('first_response', self.panel_stderr.read_text())
        self.assertEqual(self.turn('无首包测试')['status']['failure']['code'], 'ProviderTimeout')
        self.assertNotIn('不应送达', self.receipt('silent')['reply'])

    def test_idle_timeout_after_tool_keeps_evidence_and_restart_never_replays_command(self):
        def respond(body):
            if not self.results(body):
                return tool_response([('run_command', {'program': sys.executable, 'args': ['-c',
                    "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('一次执行')"]})])
            self.mode = 'stall'
            return text_response('PARTIAL_NEVER_DELIVER')
        self.callback = respond
        self.enabled(self.send(self.message('after-tool', '运行后断流', contains=['响应超时', '已启动 1 次'])))
        self.assertEqual((self.source/'once.txt').read_text(), 'x')
        failure = self.turn('运行后断流')['status']['failure']
        self.assertEqual(failure['code'], 'ProviderTimeout')
        self.assertEqual(failure['evidence']['tool_names'], ['run_command'])
        self.assertIn('stream_idle', self.panel_stderr.read_text())
        self.assertNotIn('PARTIAL_NEVER_DELIVER', self.receipt('after-tool')['reply'])
        self.mode = 'healthy'
        self.callback = lambda _: text_response('恢复后未重放')
        self.enabled(self.send(self.message('restart', '核对回执', expected='恢复后未重放')))
        self.assertEqual((self.source/'once.txt').read_text(), 'x')


if __name__ == '__main__':
    unittest.main()
