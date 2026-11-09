"""真实 QQ 宿主的 150k 主动压缩；仅本地 HTTP 与隔离桥接，不连接 QQ。"""
import json
import unittest

import history_digest_test as digest
from dialogue_tools_test import text_response, tool_response
from request_messages import conversation_messages


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

    def test_150k_tool_context_reuses_full_history_digest_after_restart_without_applying_it_to_selected_history(self):
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
            has_summary = any("宿主上下文压缩摘要" in (m.get("content") or "") for m in body["messages"])
            current = next(m["content"] for m in reversed(conversation_messages(body["messages"]))
                           if m["role"] == "user")
            self.assertEqual(has_summary, current != "缩小窗口后接着")
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
        # 100k 是压缩软目标；固定系统提示、当前原话和完整工具定义不可删除。
        # 工具增长后固定部分可能略高于目标，仍须低于请求阈值并实际缩小。
        # respond 同时核对摘要已注入且原工具批次已从请求副本移除。
        for event in events:
            self.assertEqual((event["trigger_tokens"], event["target_tokens"]), (150000, 100000))
            self.assertGreaterEqual(event["estimated_before"], 150000)
            self.assertLess(event["estimated_after"], 150000)
            self.assertLess(event["estimated_after"], event["estimated_before"])
        completed = self.turn("读取八次并核验完成")
        messages = completed["status"]["messages"]
        self.assertEqual(sum(len(m.get("tool_calls", [])) for m in messages), 8)
        self.assertEqual(sum(len(m.get("tool_results", [])) for m in messages), 8)
        self.assertIn(content, json.dumps(completed, ensure_ascii=False))
        # 此时仅有两轮，max_turns=2 的计划完整保留原历史；重启必须复用有原文
        # 指纹的同会话摘要，不能把“存在计划”误当成“筛选了历史”而重复压缩。
        previous = json.dumps(completed, sort_keys=True)
        later = json.dumps(self.turn("接着检查"), sort_keys=True)
        persisted = {path.name: path.read_bytes() for path in self.digest_files()}
        self.assertTrue(persisted, "完整历史压缩应留下可跨重启复用的摘要")
        requests_before = len(self.digest_requests())
        dialogue_before = self.dialogue_calls
        self.run_window([*self.send(self.message("restart", "重启后接着", expected="全部完成"))], env_extra=env, extra=features)
        self.assertEqual(len(self.digest_requests()), requests_before, "完整原历史前缀未变，重启不得再次请求摘要模型")
        self.assertEqual(self.dialogue_calls, dialogue_before + 1, "重启仅调用一次主对话，不重放旧工具")
        self.assertEqual({path.name: path.read_bytes() for path in self.digest_files()}, persisted)
        restarted = self.dialogue("重启后接着")
        self.assertEqual([m["content"] for m in conversation_messages(restarted["messages"]) if m["role"] == "user"],
                         ["接着检查", "重启后接着"], "缓存只覆盖旧前缀，最近原话和当前输入必须保留")
        self.assertEqual(self.turn("重启后接着")["status"]["state"], "Completed")
        for message in self.turn("重启后接着")["status"]["messages"]:
            self.assertFalse(message.get("tool_calls") or message.get("tool_results"))
        self.assertEqual(json.dumps(self.turn("读取八次并核验完成"), sort_keys=True), previous)
        self.assertEqual(json.dumps(self.turn("接着检查"), sort_keys=True), later)

        # 真正筛选子序列时，原 covered_turns 绝不能套到最新一轮并误删它。
        # 选中的小轮次无需压缩，原持久摘要仍保留但不注入本次请求。
        self.run_window([*self.send(self.message("selected", "缩小窗口后接着", expected="全部完成"))],
                        env_extra={**env, "EVE_HISTORY_MAX_TURNS": "1"}, extra=features)
        selected = self.dialogue("缩小窗口后接着")
        self.assertEqual([m["content"] for m in conversation_messages(selected["messages"]) if m["role"] == "user"],
                         ["重启后接着", "缩小窗口后接着"])
        self.assertEqual(self.turn("缩小窗口后接着")["status"]["state"], "Completed")
        self.assertEqual(json.dumps(self.turn("读取八次并核验完成"), sort_keys=True), previous)


if __name__ == "__main__":
    unittest.main(verbosity=2)
