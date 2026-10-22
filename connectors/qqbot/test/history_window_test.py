"""实际 QQ 宿主的历史窗口、热配置、失败接续和持久恢复；只连接本地替身。"""
import json
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response
import interest_test as interest


class HistoryWindowAcceptance(developer.DeveloperAcceptance):
    panel = interest.InterestAcceptance.panel
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def run_window(self, script, **options):
        env = {"EVE_HISTORY_MAX_TURNS": "2", "EVE_CONTEXT_COMPACT_AT_TOKENS": "0", "EVE_DEVELOPER_CONFIG": str(self.developer_config),
               **options.pop("env_extra", {})}
        interest.InterestAcceptance.run_eve(self, script, interest=False, memory=True, env_extra=env, **options)

    def save_window(self, **values):
        status, page = self.panel("/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": "runtime.history"})
        self.assertEqual(status, 200)
        status, saved = self.panel("/api/plugin-pages/save", {"plugin_id": "eve.config", "page_id": "runtime.history",
            "instance": page["instance"], "expected_revision": page["revision"], "values": values})
        self.assertEqual(status, 200, saved)
        self.assertEqual(saved["restart_required"], [])

    def chat_request(self, text):
        # 后台历史摘要请求可能晚于本轮对话到达，按本轮输入取对话请求。
        return next(r["body"]["messages"] for r in reversed(self.requests)
                    if r["body"]["messages"][-1].get("content") == text)

    def http_status(self, kind, number):
        return 503 if self.requests[-1]["body"]["messages"][-1]["content"] == "失败请求" else None

    def test_hot_window_preserves_tool_pairs_failed_input_and_restores_original_history_after_restart(self):
        def respond(body):
            latest = body["messages"][-1]
            if latest["content"] == "工具轮":
                return tool_response([("read_file", {"path": "README.md"})])
            return text_response("已完成" if latest["role"] == "tool" else latest["content"])
        self.callback = respond
        self.run_window([
            *self.send(self.message("old", "旧请求")),
            *self.send(self.message("tool", "工具轮", expected="已完成")),
            *self.send(self.message("failed", "失败请求", contains="HTTP 503")),
            *self.send(self.message("continue", "继续")),
            *self.checkpoint("shrink"),
            *self.send(self.message("smaller", "再继续"))], panel=True,
            checkpoints={"shrink": lambda: self.save_window(max_turns=1)})
        continuing = next(r["body"] for r in self.requests if r["body"]["messages"][-1]["content"] == "继续")
        users = [m["content"] for m in continuing["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["工具轮", "失败请求", "继续"])
        self.assertTrue(any(m["role"] == "tool" for m in continuing["messages"]))
        self.assertTrue(any("宿主失败记录" in (m.get("content") or "") for m in continuing["messages"]))
        self.assertTrue(any("eve.runtime.history-window" in (m.get("content") or "") for m in continuing["messages"]))
        last = self.chat_request("再继续")
        self.assertEqual([m["content"] for m in last if m["role"] == "user"], ["继续", "再继续"])
        before = self.turn("工具轮")
        self.run_window([*self.send(self.message("restart", "重启后")), *self.checkpoint("expand"),
                         *self.send(self.message("restored", "恢复查看"))], panel=True,
                        checkpoints={"expand": lambda: self.save_window(max_turns=16)})
        restored = self.chat_request("恢复查看")
        self.assertIn("旧请求", [m["content"] for m in restored if m["role"] == "user"])
        self.assertEqual(self.turn("工具轮"), before)
        self.assertEqual(sum(len(m.get("tool_calls", [])) for m in restored), 1)
        self.assertEqual(len([t for s in self.documents()["eve.session"]["sessions.v1"]["sessions"].values() for t in s["turns"]]), 7)

    def test_oversized_recent_turn_gets_fixed_notice_and_no_model_request_then_panel_can_recover(self):
        self.callback = lambda body: text_response("中" * 600 if body["messages"][-1]["content"] == "大回复" else "恢复完成")
        def recover():
            status, ready = self.panel("/api/readiness")
            self.assertEqual(status, 200)
            [dialogue] = [role for role in ready["roles"] if role["role"] == "dialogue"]
            self.assertEqual((dialogue["requests"], dialogue["failures"]), (1, 0))
            self.save_window(max_bytes=8192)
        self.run_window([*self.send(self.message("large", "大回复", expected="中" * 600)),
                         *self.send(self.message("stopped", "接着", contains=["最近一轮历史超过上下文窗口", "runtime.history", "尚未启动工具调用"])),
                         *self.checkpoint("recover"),
                         *self.send(self.message("recovered", "窗口已调大", expected="恢复完成"))],
                        panel=True, env_extra={"EVE_HISTORY_MAX_BYTES": "1024"},
                        checkpoints={"recover": recover})
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.turn("接着")["status"]["state"], "Failed")
        self.assertEqual(self.turn("接着")["status"]["failure"]["started_tools"], 0)
        self.assertEqual(self.turn("大回复")["status"]["messages"][-1]["text"], "中" * 600)
        self.assertEqual(self.turn("窗口已调大")["status"]["state"], "Completed")


if __name__ == "__main__":
    unittest.main(verbosity=2)
