"""同一个真实 Eve 进程自动加载 AGENT.md 新版本；模型与 QQ 都是本地替身。"""
import unittest
import interest_test
from dialogue_tools_test import text_response


class AgentPromptReloadAcceptance(interest_test.InterestAcceptance):
    for _name in dir(interest_test.InterestAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def test_changed_agent_file_is_loaded_without_restart_or_extra_model_requests(self):
        agent = self.work / 'AGENT.md'
        agent.write_text('你是 Eve。规则版本 LIVE_FILE_FIRST', encoding='utf8')
        observed = []

        def respond(body, kind, fallback):
            if kind == 'chat':
                observed.append(body['messages'][0]['content'])
                return text_response('已收到')
            return fallback

        def change():
            replacement = self.work / 'AGENT.new'
            replacement.write_text('你是 Eve。规则版本 LIVE_FILE_SECOND', encoding='utf8')
            replacement.replace(agent)
            # 覆盖两次250ms采样；实际请求内容和同进程计数仍由后面的断言验证。
            self.assertFalse(self.run_release.wait(1.0))

        self.provider_response = respond
        self.run_eve([
            *self.send(self.message('before-agent-change', '第一轮', expected='已收到')),
            *self.checkpoint('change-agent'),
            *self.send(self.message('after-agent-change', '第二轮', expected='已收到')),
        ], interest=False, extra=['--agent', str(agent)], checkpoints={'change-agent': change})
        self.assertEqual(self.runs, 1)
        self.assertEqual(len(observed), 2)
        self.assertIn('LIVE_FILE_FIRST', observed[0])
        self.assertIn('LIVE_FILE_SECOND', observed[1])
        self.assertNotIn('LIVE_FILE_FIRST', observed[1])
        self.assertEqual(len(self.kind('chat')), 2, '重新读取不调用模型或制造额外轮次')


if __name__ == '__main__':
    unittest.main()
