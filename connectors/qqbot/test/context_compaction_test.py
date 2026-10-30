"""真实 QQ 宿主的 150k 主动压缩；仅本地 HTTP 与隔离桥接，不连接 QQ。"""
import json
import unittest

import history_digest_test as digest
from dialogue_tools_test import text_response, tool_response


class ContextCompactionAcceptance(digest.HistoryDigestAcceptance):
    def run_window(self, script, **options):
        # 本组验证150k压缩而非64KiB历史窗口拒绝；保留原大回执进入压缩器。
        env = {"EVE_HISTORY_MAX_BYTES": "1048576", **options.pop("env_extra", {})}
        return super().run_window(script, env_extra=env, **options)

    for _name in dir(digest.HistoryDigestAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def test_provider_403_before_tools_preserves_history_and_recovers_after_access_is_fixed(self):
        (self.source / "README.md").write_text("中文结果" * 2700, encoding="utf8")
        self.dialogue_calls = 0
        def respond(body):
            if body["messages"][0]["content"].startswith(digest.DIGEST_PROMPT):
                request = json.loads(body["messages"][1]["content"])
                covered = request["previous_turns"] + len(request["turns"])
                return text_response(json.dumps({"points":[{"text":"八次文件读取已完成，等待继续核验", "turns":[covered]}]}, ensure_ascii=False))
            self.dialogue_calls += 1
            if self.dialogue_calls == 1:
                return tool_response([("read_file", {"path":"README.md"})] * 8)
            return text_response("核验完成")
        self.callback = respond
        self.run_window(self.send(self.message("seed", "保存原始核验", expected="核验完成")),
            env_extra={"EVE_CONTEXT_COMPACT_AT_TOKENS":"0", "EVE_CONTEXT_CASUAL_COMPACT_AT_TOKENS":"0"})
        before = json.dumps(self.turn("保存原始核验"), sort_keys=True)
        self.status_for_body = lambda body: 403 if body['messages'][0]['content'].startswith(digest.DIGEST_PROMPT) else None
        env = {"EVE_CONTEXT_COMPACT_AT_TOKENS":"150000", "EVE_CONTEXT_COMPACT_TO_TOKENS":"100000"}
        self.run_window(self.send(self.message("denied", "继续核验", contains=["上下文压缩失败", "HTTP 403", "分组权限", "尚未启动工具调用"])), env_extra=env)
        notice = self.receipt("denied")['reply']
        self.assertNotIn("继续前需要核对现有结果", notice)
        failure = self.turn("继续核验")['status']['failure']
        self.assertEqual((failure['code'], failure['started_tools']), ('Context', 0))
        self.assertEqual(failure['evidence']['provider_http_status'], 403)
        self.assertEqual(failure['evidence']['provider_failure'], 'unauthorized')
        self.assertEqual(json.dumps(self.turn("保存原始核验"), sort_keys=True), before)
        self.assertEqual(self.dialogue_calls, 2, '403 不应调用主对话或重跑工具')
        self.status_for_body = lambda body: None
        self.run_window(self.send(self.message("recovered", "权限已恢复，继续", expected="核验完成")), env_extra=env)
        self.assertEqual(self.turn("权限已恢复，继续")['status']['state'], 'Completed')
        self.assertEqual(json.dumps(self.turn("保存原始核验"), sort_keys=True), before)

    def test_digest_timeout_is_not_reported_as_main_dialogue_timeout_and_tools_are_not_replayed(self):
        (self.source / "README.md").write_text("中文结果" * 2700, encoding="utf8")
        self.callback = lambda body: tool_response([("read_file", {"path": "README.md"})] * 8)
        self.response_gates[("chat", 2)] = "keep-summary-blocked"
        self.response_gates[("chat", 3)] = "keep-summary-retry-blocked"
        self.run_window([*self.send(self.message("timeout", "读取后接续", contains=["上下文压缩未能完成", "已启动 8 次工具调用"]))],
                        env_extra={"EVE_CONTEXT_COMPACT_AT_TOKENS":"150000", "EVE_CONTEXT_COMPACT_TO_TOKENS":"100000",
                                   "EVE_MODELS_PRIMARY_TIMEOUT_MS":"1000", "EVE_OPENAI_TIMEOUT_SECONDS":"1"})
        self.assertEqual(self.turn("读取后接续")["status"]["failure"]["started_tools"], 8)
        # 一次主对话、两次有界摘要尝试；没有再次发主对话或重放读工具。
        self.assertEqual(len(self.requests), 3)
        for request in self.requests[1:]:
            self.assertIn("对话历史压缩器", request["body"]["messages"][0]["content"])
            self.assertEqual(request["body"].get("tools", []), [])

    def test_150k_tool_context_is_summarized_then_continues_and_restart_recomputes_selected_history(self):
        content = "中文结果" * 2700
        (self.source / "README.md").write_text(content, encoding="utf8")
        self.dialogue_calls = 0

        def respond(body):
            if body["messages"][0]["content"].startswith(digest.DIGEST_PROMPT):
                request = json.loads(body["messages"][1]["content"])
                covered = request["previous_turns"] + len(request["turns"])
                return text_response(json.dumps({"points": [{"text": "八次文件读取已经完成，接着核验并交付", "turns": [covered]}]}, ensure_ascii=False))
            self.dialogue_calls += 1
            if self.dialogue_calls == 1:
                return tool_response([("read_file", {"path": "README.md"})] * 8)
            self.assertTrue(any("宿主上下文压缩摘要" in (m.get("content") or "") for m in body["messages"]))
            self.assertFalse(any(m.get("tool_calls") or m["role"] == "tool" for m in body["messages"]))
            return text_response("全部完成")

        self.callback = respond
        env = {"EVE_CONTEXT_COMPACT_AT_TOKENS": "150000", "EVE_CONTEXT_COMPACT_TO_TOKENS": "100000"}
        features = ["--background-tasks", "--subagents"]
        def reject_invalid_target():
            status, page = self.panel("/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": "runtime.history"})
            self.assertEqual(status, 200)
            status, result = self.panel("/api/plugin-pages/save", {"plugin_id": "eve.config", "page_id": "runtime.history",
                "instance": page["instance"], "expected_revision": page["revision"], "values": {"compact_to_tokens": 150000}})
            self.assertEqual(status, 400, result)
        self.run_window([*self.send(self.message("tools", "读取八次并核验完成", expected="全部完成")),
                         *self.send(self.message("later", "接着检查", expected="全部完成")),
                         *self.checkpoint("invalid")], panel=True, env_extra=env, extra=features, checkpoints={"invalid":reject_invalid_target})
        events = [json.loads(line.split(" ", 1)[1]) for line in self.panel_stderr.read_text(encoding="utf8").splitlines()
                  if line.startswith("EVE_CONTEXT_COMPACTION ")]
        self.assertTrue(events)
        self.assertTrue(all(e["estimated_before"] >= 150000 and e["estimated_after"] < 100000 for e in events))
        completed = self.turn("读取八次并核验完成")
        messages = completed["status"]["messages"]
        self.assertEqual(sum(len(m.get("tool_calls", [])) for m in messages), 8)
        self.assertEqual(sum(len(m.get("tool_results", [])) for m in messages), 8)
        self.assertIn(content, json.dumps(completed, ensure_ascii=False))
        # 分层选择后的索引不能复用全历史covered_turns，持久摘要不作本轮缓存。
        previous = json.dumps(completed, sort_keys=True)
        requests_before = len(self.digest_requests())
        self.run_window([*self.send(self.message("restart", "重启后接着", expected="全部完成"))], env_extra=env, extra=features)
        self.assertGreater(len(self.digest_requests()), requests_before, "重启后必须针对重新选择的完整原轮次安全重算摘要")
        self.assertEqual(json.dumps(self.turn("读取八次并核验完成"), sort_keys=True), previous)


if __name__ == "__main__":
    unittest.main(verbosity=2)
