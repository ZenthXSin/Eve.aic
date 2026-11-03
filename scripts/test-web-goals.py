"""独立 Web 自然待办、原话审核、研究计划和恢复的真实进程验收。

模型由确定性本地服务提供，资料来自校验过的临时资料库；不启动 QQ，
不发送平台消息，不把计划或模型的完成说明当作执行证据。
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import time
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "web_autonomy_fixture", Path(__file__).with_name("test-web-autonomy.py"))
autonomy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(autonomy)
from dialogue_tools_test import text_response, tool_response
from plan_test import step, RESEARCH, PRACTICE

QUOTE = "每条观察记录都应保存来源与验证结果。"
CORPUS_TEXT = "# 验收资料\n\n" + QUOTE + "\n"


class WebGoalAcceptance(autonomy.WebAutonomyAcceptance):
    for name in dir(autonomy.WebAutonomyAcceptance):
        if name.startswith("test_"):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.plan = lambda request: {"steps": [step("read", "读取资料并保存引用", RESEARCH)]}
        self.select = lambda request: {"selected": [next(
            candidate["index"] for candidate in request["candidates"]
            if candidate["url"].endswith("/observations.md"))]}
        self.extract = lambda request: {"claims": [{
            "document_id": request["documents"][0]["document_id"], "kind": "procedure",
            "statement": QUOTE, "quote": QUOTE, "version": None}], "hypotheses": []}

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "proposal_id" in decoded and "capabilities" in decoded:
            return "plan"
        return super().classify(decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "plan":
            return json.dumps(self.plan(decoded), ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def prepare_corpus(self):
        corpus = self.source / "vendor/eve-portable-knowledge"
        corpus.mkdir(parents=True)
        (corpus / "observations.md").write_text(CORPUS_TEXT, encoding="utf8")
        manifest = {"format_version": 1, "corpus": "web-fixture", "upstream": "https://example.test/web-fixture",
            "commit": "a" * 40, "license": "CC0", "files": [{"path": "observations.md",
            "sha256": hashlib.sha256(CORPUS_TEXT.encode()).hexdigest(), "bytes": len(CORPUS_TEXT.encode())}]}
        (corpus / "MANIFEST.json").write_text(json.dumps(manifest), encoding="utf8")

    def user_goals(self):
        return [goal for goal in self.goals() if goal["source"]["kind"] == "User"]

    def plan_ledger(self):
        return self.documents().get("eve.plan", {}).get("plans.v1", {"plans": [], "proposals": []})

    def wait_saved(self, predicate, reason, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.assertIsNone(self.child.poll(), self.log_path.read_text())
            self.assertFalse(self.errors, self.errors)
            if predicate():
                return
            time.sleep(.03)
        self.fail(reason)

    def test_default_web_goal_feedback_full_review_and_restart_preserve_real_goal(self):
        words = ["保存一个出行待办。", "先记录路线。", "资料：" + "途中需要检查天气。" * 500 + "完整末尾。",
                 "稍后再决定预算。", "记录不代表行程已经完成。", "请记下待办：整理出行计划。"]
        feedback = "把刚才的待办补充为上午九点出发。"
        inspect = "重新打开后查看待办及反思。"
        stale = "核对旧版本反馈不能覆盖新的出发时间。"
        reviews, observed = [], {}

        def review(request):
            self.assertEqual(request["program"], "eve.manage_goal")
            self.assertEqual(request["user_messages"], words if not reviews else words[1:] + [feedback])
            operation = json.loads(request["arguments"][0])
            self.assertEqual(operation["trusted_scope"]["user_id"], autonomy.WEB_OWNER)
            self.assertEqual(operation["trusted_scope"]["session_id"], "web:chat:" + autonomy.CONVERSATION)
            self.assertFalse(request["delegated"])
            reviews.append(request)
            return {"risk": "medium", "authorization": "high", "outcome": "allow", "rationale": "逐字核对真实 Web 原话"}

        def respond(body):
            current, results = self.current_text(body), self.results(body)
            self.assertTrue({"manage_goal", "list_goals", "list_plans", "list_knowledge", "list_practice"} <= self.definitions(body))
            if current in words[:-1]:
                return text_response("收到。")
            if not results:
                if current == words[-1]:
                    return tool_response([("manage_goal", {"action": "create", "text": "整理出行计划"})])
                goal = observed["created"]["goal"]
                if current in (feedback, stale):
                    return tool_response([("manage_goal", {"action": "feedback", "goal_id": goal["id"],
                        "expected_revision": goal["revision"], "text": "上午九点出发"})])
                self.assertEqual(current, inspect)
                return tool_response([("manage_goal", {"action": "inspect", "goal_id": goal["id"]})])
            result = results[-1]
            if current == words[-1]:
                self.assertTrue(result["stored"])
                self.assertFalse(result["external_goal_completed"])
                observed["created"] = result
            elif current == feedback:
                self.assertTrue(result["stored"])
                self.assertEqual(result["goal"]["revision"], observed["created"]["goal"]["revision"] + 1)
                observed["updated"] = result
            elif current == stale:
                self.assertIn("版本", result["error"]["message"])
            else:
                self.assertEqual(result["goal"], observed["updated"]["goal"])
                self.assertFalse(result["goal_completed"])
                self.assertEqual(result["draft"]["status"], "Completed")
                self.assertIsNotNone(result["draft"]["artifact"])
            return text_response("已核对真实待办，出行尚未完成。")

        self.review_reply, self.callback = review, respond
        self.start_web_without_developer_config()
        self.assertEqual(self.web("/api/chat")[1]["role"], "owner")
        for number, word in enumerate(words, 1):
            self.send_and_finish(word, number)
        self.send_and_finish(feedback, 7)
        self.send_and_finish(stale, 8)
        gid = observed["updated"]["goal"]["id"]
        self.wait_saved(lambda: any(goal["source"]["reference"] == gid and goal["status"] == "Completed"
            for goal in self.goals() if goal["source"]["kind"] == "Inference"), "当前待办没有完成无工具反思")
        self.assertEqual(self.user_goals(), [observed["updated"]["goal"]])
        self.assertEqual(len(reviews), 2)
        audit = (self.work / "state/business-permissions.json").read_bytes()
        for record, request in zip(json.loads(audit)["audit"], reviews):
            self.assertEqual(json.loads(record["target"]), request)
        before = self.chat_rounds
        self.stop_web(graceful=True)
        self.start_web_without_developer_config()
        duplicate = self.request(words[-1], request=format(6, "032x"))
        self.assertEqual(self.web("/api/chat/send", duplicate)[1]["status"], "completed")
        self.assertEqual(self.chat_rounds, before)
        self.send_and_finish(inspect, 9)
        self.assertEqual((self.work / "state/business-permissions.json").read_bytes(), audit)
        self.assertEqual(len(reviews), 2)
        self.assertEqual(self.user_goals(), [observed["updated"]["goal"]])
        self.assertNotIn("eve.channel.qqbot", self.documents())

    @unittest.skipUnless(sys.platform.startswith("linux") and os.environ.get("EVE_TEST_BROWSER"),
                         "实际静态页面验证需要 EVE_TEST_BROWSER 指向可运行的 Chromium")
    def test_research_then_real_browser_repairs_failed_style_and_keeps_verified_artifact(self):
        self.prepare_corpus()
        browser = Path(os.environ["EVE_TEST_BROWSER"]).resolve(strict=True)
        tools = self.work / "browser-bin"
        tools.mkdir()
        (tools / "chromium").symlink_to(browser)
        goal_text = "把观察资料整理成静态网页，标题显示观察记录且颜色为蓝色。"
        create = "记下待办：" + goal_text
        propose = "先读资料，再运行浏览器检查网页的标题和蓝色样式，展示计划。"
        inspect = "查看当前计划全部步骤。"
        confirm = "确认这份计划，读取资料并用浏览器验证网页，不通过就按实际结果修正。"
        observed, reviews = {}, []
        self.plan = lambda request: {"steps": [step("read", "读取资料保存引用", RESEARCH),
            step("page", "实际浏览器验证并修正静态页面", PRACTICE, ["read"], 1800000)]}

        def draft(request):
            previous = request.get("previous")
            color = "rgb(255, 0, 0)"
            if previous:
                self.assertIsNotNone(previous["evidence"])
                probe = next(p for p in previous["evidence"]["probes"] if p["probe"]["property"] == "color")
                self.assertFalse(probe["passed"])
                self.assertEqual(probe["actual"], "rgb(255, 0, 0)")
                color = "rgb(0, 0, 255)"
            html = '<!doctype html><html><head><meta charset="utf-8"><style>#summary { color: ' + color + '; }</style></head><body><h1 id="summary">观察记录</h1></body></html>'
            self.assertEqual(request["runner"]["runner_id"], "static-web-page:v1")
            self.assertTrue(request["task"]["notes"])
            return {"applicable": True, "rationale": "依据资料生成静态页并核对真实浏览器的文字和样式。",
                "files": [{"path": "index.html", "content": html}],
                "probes": [{"subject": "summary", "property": "text", "expected": "观察记录"},
                    {"subject": "summary", "property": "color", "expected": "rgb(0, 0, 255)"}],
                "notes_used": [note["id"] for note in request["task"]["notes"]]}

        def review(request):
            reviews.append(request)
            operation = json.loads(request["arguments"][0])["operation"]
            if operation["arguments"]["action"] == "confirm":
                self.assertEqual(request["user_messages"], [create, propose, inspect, confirm])
                self.assertEqual([s["spec"]["capability"] for s in operation["plan"]["steps"]], [RESEARCH, PRACTICE])
                self.assertEqual(operation["plan"]["steps"][1]["spec"]["depends_on"], ["read"])
            return {"risk": "medium", "authorization": "high", "outcome": "allow",
                "rationale": "用户明确同意资料读取与隔离浏览器验证，完整步骤和原话已核对。"}

        def respond(body):
            current, results = self.current_text(body), self.results(body)
            if not results:
                if current == create:
                    return tool_response([("manage_goal", {"action": "create", "text": goal_text})])
                gid = observed["goal"]["id"]
                if current in [propose, inspect]:
                    return tool_response([("manage_plan", {"action": "propose" if current == propose else "inspect", "goal_id": gid})])
                self.assertEqual(current, confirm)
                plan = observed["plan"]
                return tool_response([("manage_plan", {"action": "confirm", "goal_id": gid,
                    "plan_id": plan["id"], "expected_revision": plan["revision"]})])
            result = results[-1]
            self.assertNotIn("error", result)
            if current == create:
                observed["goal"] = result["goal"]
            elif current == inspect:
                [observed["plan"]] = result["plans"]
            elif current == confirm:
                self.assertEqual(result["plan"]["status"], "active")
            return text_response("操作已保存，验证结论以实际运行记录为准。")

        self.draft, self.review_reply, self.callback = draft, review, respond
        with mock.patch.dict(os.environ, {"PATH": str(tools) + os.pathsep + os.environ.get("PATH", "")}):
            self.start_web_without_developer_config()
            self.send_and_finish(create, 1)
            self.send_and_finish(propose, 2)
            self.wait_saved(lambda: len(self.plan_ledger()["plans"]) == 1, "浏览器计划未保存")
            self.send_and_finish(inspect, 3)
            self.assertEqual(self.practice_runs(), [])
            self.send_and_finish(confirm, 4)
            self.wait_saved(lambda: self.plan_ledger()["plans"][0]["status"] == "completed",
                "实际浏览器修正没有完成", 80)
            [research], [practice] = self.research_runs(), self.practice_runs()
            self.assertEqual(research["status"], "Completed")
            self.assertGreaterEqual(practice["started_at_ms"], research["finished_at_ms"])
            self.assertEqual(practice["status"], "Verified")
            self.assertEqual([a["outcome"] for a in practice["attempts"]], ["failed", "verified"])
            evidence = practice["attempts"][-1]["evidence"]
            self.assertTrue(evidence["loaded"])
            self.assertEqual(evidence["exit"], "Completed")
            self.assertTrue(all(p["passed"] for p in evidence["probes"]))
            self.assertTrue(evidence["runtime_version"])
            self.assertEqual(len(evidence["log_sha256"]), 64)
            self.assertIn("rgb(0, 0, 255)", practice["attempts"][-1]["draft"]["files"][0]["content"])
            self.assertEqual(len(self.kind("draft")), 2)
            self.assertEqual(self.user_goals()[0]["status"], "Waiting")
            before = self.plan_ledger(), self.practice(), len(self.kind("draft")), len(reviews)
            self.stop_web(graceful=True)
            self.start_web_without_developer_config()
            self.send_and_finish(inspect, 5)
            self.assertEqual((self.plan_ledger(), self.practice(), len(self.kind("draft")), len(reviews)), before)

    def test_plan_denied_confirmation_then_background_research_and_restart_keep_evidence(self):
        self.prepare_corpus()
        create = "记下待办：整理如何保存观察记录。"
        propose = "提出阅读资料并保存引用的计划，先给我查看。"
        inspect = "展示计划与步骤证据。"
        denied = "先看看刚才的计划，暂时不要执行。"
        confirm = "确认这份计划，现在读取资料并保存可核对的引用。"
        reviews, observed = [], {}

        def review(request):
            reviews.append(request)
            operation = json.loads(request["arguments"][0])["operation"]
            action = operation["arguments"]["action"]
            allowed = request["user_messages"][-1] != denied
            if action == "confirm":
                self.assertEqual([s["spec"]["capability"] for s in operation["plan"]["steps"]], [RESEARCH])
                self.assertEqual(operation["goal"]["description"], "整理如何保存观察记录")
                self.assertEqual(request["user_messages"], [create, propose, inspect, denied] + ([confirm] if allowed else []))
            return {"risk": "medium", "authorization": "high" if allowed else "unknown",
                "outcome": "allow" if allowed else "deny", "rationale": "仅对用户明确确认的具体计划放行"}

        def respond(body):
            current, results = self.current_text(body), self.results(body)
            self.assertIn("manage_plan", self.definitions(body))
            if not results:
                if current == create:
                    return tool_response([("manage_goal", {"action": "create", "text": "整理如何保存观察记录"})])
                gid = observed["goal"]["id"]
                if current == propose:
                    return tool_response([("manage_plan", {"action": "propose", "goal_id": gid})])
                if current == inspect:
                    return tool_response([("manage_plan", {"action": "inspect", "goal_id": gid})])
                if current == "查看保存下来的引用":
                    return tool_response([("list_knowledge", {})])
                self.assertIn(current, [denied, confirm])
                plan = observed["plan"]
                return tool_response([("manage_plan", {"action": "confirm", "goal_id": gid,
                    "plan_id": plan["id"], "expected_revision": plan["revision"]})])
            result = results[-1]
            if current == create:
                observed["goal"] = result["goal"]
            elif current == propose:
                self.assertFalse(result["steps_started"])
            elif current == inspect:
                [observed["plan"]] = result["plans"]
            elif current == denied:
                self.assertIn("error", result)
            elif current == confirm:
                self.assertTrue(result["changed"])
                self.assertEqual(result["plan"]["status"], "active")
                self.assertFalse(result["goal_completed"])
            else:
                self.assertIn(QUOTE, json.dumps(result, ensure_ascii=False))
                observed["knowledge"] = result
            return text_response("已根据实际记录核对。")

        self.review_reply, self.callback = review, respond
        self.start_web_without_developer_config()
        self.send_and_finish(create, 1)
        self.send_and_finish(propose, 2)
        self.wait_saved(lambda: len(self.plan_ledger()["plans"]) == 1, "计划未保存")
        self.send_and_finish(inspect, 3)
        self.assertEqual(self.research_runs(), [])
        self.send_and_finish(denied, 4)
        self.assertEqual(self.plan_ledger()["plans"][0]["status"], "proposed")
        self.assertEqual(self.research_runs(), [])
        self.send_and_finish(confirm, 5)
        # 此后不再提交聊天，请求结束与浏览器断开都不停止后台研究。
        self.wait_saved(lambda: self.plan_ledger()["plans"][0]["status"] == "completed", "后台研究没有形成已核验步骤", 30)
        [run] = self.research_runs()
        self.assertEqual(run["status"], "Completed")
        [entry] = self.knowledge()["entries"]
        self.assertEqual(entry["source"]["quote"], QUOTE)
        document = next(d for d in self.knowledge()["documents"] if d["id"] == entry["source"]["document_id"])
        self.assertEqual(document["sha256"], hashlib.sha256(CORPUS_TEXT.encode()).hexdigest())
        plan = self.plan_ledger()["plans"][0]
        self.assertEqual(plan["steps"][0]["attempts"][0]["evidence"]["source_id"], run["id"])
        self.assertEqual(self.user_goals()[0]["status"], "Waiting")
        self.assertEqual(len(reviews), 4)
        self.assertEqual([r["outcome"] for r in self.business_ledger()["audit"]],
            ["review_allowed", "review_allowed", "review_denied", "review_allowed"])
        before = self.plan_ledger(), self.knowledge(), len(self.kind("plan")), len(self.kind("extract"))
        self.stop_web(graceful=True)
        self.start_web_without_developer_config()
        duplicate = self.request(confirm, request=format(5, "032x"))
        self.assertEqual(self.web("/api/chat/send", duplicate)[1]["status"], "completed")
        self.send_and_finish(inspect, 6)
        self.send_and_finish("查看保存下来的引用", 7)
        self.assertEqual((self.plan_ledger(), self.knowledge(), len(self.kind("plan")), len(self.kind("extract"))), before)
        self.assertEqual(len(reviews), 4)
        self.assertNotIn("eve.channel.qqbot", self.documents())


if __name__ == "__main__":
    unittest.main()
