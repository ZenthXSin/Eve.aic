"""平台管理员绑定、权限与敏感命令脱敏的实际宿主验收。"""
import json
import unittest
import reminder_test
from background_task_test import calls
from dialogue_tools_test import text_response
from execution_test import ExecutionAcceptance

class AdminAcceptance(ExecutionAcceptance):
    test_pending_task_continues_after_early_final_and_sleep_without_new_user_input = None
    def run_eve(self, script, developer=False, **options):
        env = {"EVE_ADMIN_BIND_KEY": "isolated-test-bind-key"}
        if developer:
            config = self.work / "developer.json"
            config.write_text(json.dumps({"workspace": str(self.work), "operators": ["unrelated-admin"], "commands": True}))
            env["EVE_DEVELOPER_CONFIG"] = str(config)
        return reminder_test.base().run_eve(self, script, interest=False, research=False,
                                           practice=False, more=["--sleep"], env_extra=env, **options)

    def test_binding_is_platform_scoped_persistent_and_never_reaches_model_or_receipts(self):
        self.callback = lambda _: self.fail("绑定与查询命令不得请求模型")
        g1 = {"scope": "group", "target": "group-1"}
        g2 = {"scope": "group", "target": "group-2"}
        self.run_eve([
            *self.send(self.message("bad", "/bind wrong-key", expected="绑定密钥无效。", **g1)),
            *self.send(self.message("bind", "/bind isolated-test-bind-key", contains="绑定成功", **g1)),
            *self.send(self.message("same", "/admin", expected="当前平台身份：管理员。", **g2)),
            *self.send(self.message("other", "/admin", contains="普通用户", user="user-2", **g2)),
            *self.send(self.message("direct", "/admin", contains="普通用户")),
        ])
        state = (self.work / "state/state.json").read_text()
        self.assertNotIn("isolated-test-bind-key", state)
        self.assertNotIn("wrong-key", state)
        self.run_eve([*self.send(self.message("restart", "/admin", expected="当前平台身份：管理员。", **g2))])

    def test_default_ordinary_access_and_sensitive_command_requires_binding(self):
        (self.work / "hello.txt").write_text("public source")
        def respond(body):
            results = self.results(body)
            admin = any('"administrator":true' in (m.get("content") or "") for m in body["messages"])
            if not results:
                if self.text(body) == "普通读取":
                    return calls(("read", "read_file", {"path": "hello.txt"}))
                return calls(("cmd", "run_command", {"program": "echo", "args": ["admin-ok"]}))
            if self.text(body) == "普通读取":
                self.assertEqual(results[-1]["content"], "public source")
                return text_response("读取成功")
            if admin:
                self.assertTrue(results[-1]["success"])
                return text_response("管理员操作成功")
            self.assertIn("操作者", json.dumps(results[-1], ensure_ascii=False))
            return text_response("需要管理员")
        self.callback = respond
        self.run_eve([
            *self.send(self.message("read", "普通读取", expected="读取成功")),
            *self.send(self.message("denied", "执行命令", expected="需要管理员")),
            *self.send(self.message("bind", "/bind isolated-test-bind-key", contains="绑定成功")),
            *self.send(self.message("allowed", "执行命令", expected="管理员操作成功")),
        ], developer=True)


if __name__ == "__main__":
    unittest.main()
