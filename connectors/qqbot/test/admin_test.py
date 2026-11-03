"""平台管理员绑定、权限与敏感命令脱敏的实际宿主验收。"""
import json
import unittest
import reminder_test
from background_task_test import calls
from dialogue_tools_test import text_response, tool_response
from execution_test import ExecutionAcceptance

class AdminAcceptance(ExecutionAcceptance):
    test_pending_task_continues_after_early_final_and_sleep_without_new_user_input = None

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and {"action", "policy_reason", "user_messages"} <= decoded.keys():
            return "review"
        return super().classify(decoded, latest)

    def provider_response(self, body, kind, fallback):
        if kind == "review":
            self.assertEqual(self.definitions(body), {"submit_review"})
            request = json.loads(body["messages"][-1]["content"])
            self.assertNotIn("isolated-test-bind-key", json.dumps(request))
            if "admin-ok" not in request["action"]:
                return tool_response([("submit_review", {"risk": "low", "authorization": "unknown",
                    "outcome": "deny", "rationale": "普通读取没有授权执行命令"})])
            self.assertEqual(len(request["user_messages"]), 6)
            return tool_response([("submit_review", {"risk": "low", "authorization": "high",
                "outcome": "allow", "rationale": "隔离测试已绑定身份授权运行 echo"})])
        return super().provider_response(body, kind, fallback)

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

    def test_workspace_and_commands_require_binding(self):
        (self.work / "hello.txt").write_text("public source")
        def respond(body):
            results = self.results(body)
            admin = any('"administrator":true' in (m.get("content") or "") for m in body["messages"])
            if not results:
                if self.text(body) == "普通读取":
                    return calls(("read", "read_file", {"path": "hello.txt"}))
                if admin:
                    return calls(("cmd", "run_command", {"program": "echo", "args": ["admin-ok"]}))
                return calls(("cmd", "run_command", {"program": "cat", "args": ["hello.txt"]}))
            # 普通成员的文件与命令工具只在自己的隔离项目里生效（见普通成员工具权限），
            # 看不到宿主工作区；绑定管理员后才在宿主工作区执行。
            result = json.dumps(results[-1], ensure_ascii=False)
            if self.text(body) == "普通读取":
                self.assertIn("error", results[-1])
                self.assertNotIn("public source", result)
                return text_response("读取需要管理员")
            if admin:
                self.assertTrue(results[-1].get("success"), results[-1])
                self.assertIn("admin-ok", result)
                return text_response("管理员操作成功")
            self.assertNotIn("public source", result)
            return text_response("需要管理员")
        self.callback = respond
        self.run_eve([
            *self.send(self.message("read", "普通读取", expected="读取需要管理员")),
            *self.send(self.message("denied", "执行命令", expected="需要管理员")),
            *self.send(self.message("bind", "/bind isolated-test-bind-key", contains="绑定成功")),
            # 密钥正文不保存为原话，最近六条仍含该缺口时业务失败关闭。
            *self.send(self.message("gap", "执行命令", contains="尚未启动工具调用")),
            *[step for i in range(6) for step in self.send(self.message(
                f"complete-original-{i}", "/admin", expected="当前平台身份：管理员。"))],
            *self.send(self.message("allowed", "执行命令", expected="管理员操作成功")),
        ], developer=True)


if __name__ == "__main__":
    unittest.main()
