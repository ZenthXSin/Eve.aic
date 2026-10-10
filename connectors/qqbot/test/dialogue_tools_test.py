"""实际 QQ 宿主→OpenAI 工具协议验收，不用模型的能力自述判断接线。

模型和 QQ 使用本地替身；源页面实际由本地 HTTP 读取。可用 EVE_MINDUSTRY_SERVER_JAR
让直接 run_practice 实际启动官方 Mindustry；否则用运行器替身验证账本/恢复。
"""
import json
import unittest

import practice_test

ALL_TOOLS = {"echo", "use_skill", "search_memory", "list_interests", "list_goals",
             "list_knowledge", "list_practice", "list_skills", "list_forged_tools",
             "list_plans", "list_outreach", "read_source", "check_practice_draft", "run_practice"}


def tool_response(calls):
    return {"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
        "role": "assistant", "content": None, "tool_calls": [
            {"id": f"call-{name}-{index}", "type": "function", "function": {
                "name": name, "arguments": json.dumps(arguments)}}
            for index, (name, arguments) in enumerate(calls)]}}]}


def text_response(text):
    return {"choices": [{"index": 0, "finish_reason": "stop", "message": {
        "role": "assistant", "content": text}}]}


class DialogueToolsAcceptance(practice_test.PracticeAcceptance):
    for _name in dir(practice_test.PracticeAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.chat_rounds = 0
        self.callback = lambda body: text_response("完成")

    def provider_response(self, body, kind, fallback):
        if kind != "chat":
            return fallback
        self.chat_rounds += 1
        return self.callback(body)

    def definitions(self, body):
        return {entry["function"]["name"] for entry in body.get("tools", [])}

    def capability_names(self, body):
        # 动态宿主能力和真正协议 tools 共用来源，不依赖历史助手回复。
        text = "\n".join(str(m.get("content", "")) for m in body["messages"] if m["role"] == "system")
        self.assertIn("eve.host.capabilities", text)
        for name in self.definitions(body):
            self.assertIn(name, text)

    def current_capabilities(self, body):
        # 紧邻本轮输入的宿主事实必须覆盖旧助手自述，并与真正协议定义一致。
        messages = body["messages"]
        current = max(index for index, message in enumerate(messages) if message["role"] == "user")
        self.assertEqual(messages[current - 1]["role"], "system")
        notice = messages[current - 1]["content"]
        self.assertNotIn("memory: ", notice)
        data = json.loads(notice.splitlines()[1])
        self.assertEqual(data["kind"], "eve.runtime.tool-capabilities")
        self.assertEqual(data["count"], len(self.definitions(body)))
        self.assertEqual({tool["name"] for tool in data["tools"]}, self.definitions(body))
        return data

    def results(self, body):
        messages = body["messages"]
        current = max(index for index, message in enumerate(messages) if message["role"] == "user")
        return [json.loads(m["content"]) for m in messages[current + 1:] if m["role"] == "tool"]

    def full(self, script, **options):
        options.setdefault("more", ["--skill-learning", "--tool-forging", "--plans", "--outreach"])
        self.run_eve(script, **options)

    def test_registered_tools_help_and_actual_source_roundtrip(self):
        def respond(body):
            self.assertEqual(self.definitions(body), ALL_TOOLS)
            self.capability_names(body)
            results = self.results(body)
            if not results:
                return tool_response([*[(name, {}) for name in (
                    "list_interests", "list_goals", "list_skills", "list_forged_tools")],
                    ("read_source", {"url": self.seed}),
                    ("read_source", {"url": "https://example.invalid/private"})])
            self.assertEqual(len(results), 6)
            self.assertTrue(all(r["items"] == [] for r in results[:4]))
            self.assertEqual(results[4]["source_kind"], "external_data")
            self.assertIn("Mindustry", results[4]["text"])
            self.assertFalse(results[4]["persisted_knowledge"])
            self.assertIn("URL 不在", json.dumps(results[-1], ensure_ascii=False))
            return text_response("工具已实际调用")
        self.callback = respond
        self.full([*self.send(self.message("help", "/help", contains=["14 个", "read_source", "run_practice"])),
                   *self.send(self.message("tools", "调用工具测试", expected="工具已实际调用"))])
        self.assertEqual(self.chat_rounds, 2)
        self.assertEqual(len(self.kind("chat")), 2)
        self.assertEqual(self.research_runs(), [], "只读抓取没有冒充已固化知识")

    def test_missing_capabilities_are_absent_and_memory_is_scope_bound(self):
        marker = "CURRENT_USER_MARKER"
        foreign = "OTHER_USER_MARKER"
        def respond(body):
            self.assertEqual(self.definitions(body), {"echo", "search_memory"})
            results = self.results(body)
            if not results:
                return tool_response([("search_memory", {"query": "MARKER"})])
            self.assertIn(marker, json.dumps(results))
            self.assertNotIn(foreign, json.dumps(results))
            return text_response("只读取当前会话")
        self.callback = respond
        self.run_eve([*self.send(self.message("own", "/remember " + marker, contains="偏好已保存")),
                      *self.send(self.message("foreign", "/remember " + foreign, user="user-2", contains="偏好已保存")),
                      *self.send(self.message("help", "/help", contains="2 个")),
                      *self.send(self.message("query", "查看记忆", expected="只读取当前会话"))],
                     interest=False, memory=True, research=False, practice=False)
        self.assertEqual(self.chat_rounds, 2)

    def test_existing_session_refreshes_tools_without_erasing_history_or_replaying(self):
        old_reply = "只有一个 echo 工具，不联网也不读写文件。"

        def old(body):
            self.assertEqual(self.definitions(body), {"echo", "search_memory"})
            self.current_capabilities(body)
            return text_response(old_reply)

        self.callback = old
        self.run_eve(self.send(self.message("old", "你现在有什么工具呢", expected=old_reply)),
                     interest=False, memory=True, research=False, practice=False,
                     more=["--memory-recall"])

        def upgraded(body):
            self.assertEqual(self.definitions(body), ALL_TOOLS)
            self.current_capabilities(body)
            self.assertIn({"role": "assistant", "content": old_reply}, body["messages"])
            self.assertTrue(any(m["role"] == "system" and "eve-memory-recall-v1" in
                                m.get("content", "") for m in body["messages"]),
                            "旧回答的召回也保留，但不能覆盖本轮能力")
            if not self.results(body):
                return tool_response([("list_interests", {}), ("list_goals", {})])
            self.assertEqual(len(self.results(body)), 2)
            return text_response("当前可调用 14 个工具，刚才已核对兴趣与目标。")

        self.callback = upgraded
        self.full([*self.send(self.message("help-upgraded", "/help", contains="14 个")),
                   *self.send(self.message("upgraded", "你现在有什么工具呢", expected=
                                          "当前可调用 14 个工具，刚才已核对兴趣与目标。"))],
                  more=["--self-learning", "--memory-recall", "--training", "--skill-learning",
                        "--tool-forging", "--plans", "--outreach"])
        self.assertEqual(len(self.kind("chat")), 3)
        self.assertEqual(self.practice_runs(), [])

        def reduced(body):
            self.assertEqual(self.definitions(body), {"echo", "search_memory"})
            self.current_capabilities(body)
            self.assertTrue(any(m["role"] == "assistant" and "14 个工具" in
                                (m.get("content") or "") for m in body["messages"]))
            return text_response("当前只装配了 2 个工具，原历史仍保留。")

        self.callback = reduced
        self.run_eve(self.send(self.message("reduced", "你现在有什么工具呢", expected=
                                           "当前只装配了 2 个工具，原历史仍保留。")),
                     interest=False, memory=True, research=False, practice=False)
        self.assertEqual(len(self.kind("chat")), 4)

    def test_direct_practice_persists_real_evidence_and_restarts_without_replay(self):
        def goal():
            return next(g for g in self.goals() if g["source"]["channel"] == "qq.goal")
        def respond(body):
            results = self.results(body)
            current = goal()
            if not results:
                return tool_response([("check_practice_draft", {"draft": practice_test.mod("Wall")}),
                    ("run_practice", {"goal_id": current["id"],
                    "goal_revision": current["revision"], "draft": practice_test.mod("Wall")})])
            self.assertEqual(len(results), 2)
            self.assertTrue(results[0]["preflight_passed"])
            self.assertFalse(results[0]["verified"])
            self.assertTrue(results[1]["started"])
            self.assertTrue(results[1]["verified"])
            self.assertEqual(results[1]["record"]["items"][0]["status"], "Verified")
            return text_response("实际验证成功")
        self.callback = respond
        real = practice_test.REAL_JAR is not None
        self.full([*self.send(self.message("goal", "/goal 创作一个 Mindustry 模组", contains="待办已保存")),
                   *self.send(self.message("run", "请调用工具实际验证", expected="实际验证成功"))],
                  real=real, timeout=180 if real else 30)
        [run] = self.practice_runs()
        self.assertEqual(run["drafter_version"], "eve.dialogue-tools-1")
        self.assertEqual(run["status"], "Verified")
        self.assertTrue(all(p["passed"] for p in run["attempts"][0]["evidence"]["probes"]))
        before = self.practice_runs()
        def read_only(body):
            results = self.results(body)
            if not results:
                current = goal()
                return tool_response([("run_practice", {"goal_id": current["id"],
                    "goal_revision": current["revision"], "draft": practice_test.mod("Wall")})])
            self.assertFalse(results[-1]["started"])
            return text_response("重启后没有重放")
        self.callback = read_only
        self.full(self.send(self.message("restart", "再次查看", expected="重启后没有重放")), real=real)
        self.assertEqual(self.practice_runs(), before)

    def test_direct_practice_rejects_other_user_goal(self):
        def respond(body):
            results = self.results(body)
            current = next(g for g in self.goals() if g["source"]["channel"] == "qq.goal")
            if not results:
                return tool_response([("run_practice", {"goal_id": current["id"],
                    "goal_revision": current["revision"], "draft": practice_test.mod("Wall")})])
            self.assertIn("目标不存在或不属于当前用户", json.dumps(results[-1], ensure_ascii=False))
            return text_response("没有访问其他用户的目标")
        self.callback = respond
        self.full([*self.send(self.message("goal", "/goal 创建 Mindustry 模组", contains="待办已保存")),
                   *self.send(self.message("other", "试着执行", user="user-2", expected="没有访问其他用户的目标"))])
        self.assertEqual(self.practice_runs(), [])
        self.assertEqual(self.launches(), [])

    def test_stale_goal_extra_identity_and_invalid_paths_never_execute(self):
        def respond(body):
            results = self.results(body)
            current = next(g for g in self.goals() if g["source"]["channel"] == "qq.goal")
            if not results:
                bad = practice_test.mod("Wall")
                bad["files"][0]["path"] = "../outside"
                return tool_response([
                    ("run_practice", {"goal_id": current["id"], "goal_revision": current["revision"] + 100,
                                      "draft": practice_test.mod("Wall")}),
                    ("run_practice", {"goal_id": current["id"], "goal_revision": current["revision"], "draft": bad}),
                    ("list_knowledge", {"owner": "other-user"}),
                ])
            self.assertEqual(len(results), 3)
            self.assertIn("目标修订已变化", json.dumps(results[0], ensure_ascii=False))
            self.assertIn("草稿结构", json.dumps(results[1], ensure_ascii=False))
            self.assertIn("参数", json.dumps(results[2], ensure_ascii=False))
            return text_response("未执行无效输入")
        self.callback = respond
        self.full([*self.send(self.message("goal", "/goal 创建 Mindustry 模组", contains="待办已保存")),
                   *self.send(self.message("bad", "检查无效输入", expected="未执行无效输入"))])
        self.assertEqual(self.practice_runs(), [])
        self.assertEqual(self.launches(), [])


if __name__ == "__main__":
    unittest.main()
