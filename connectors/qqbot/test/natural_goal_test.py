"""自然聊天待办与计划的真实 QQ 进程验收；本地模型替身只证明宿主契约。"""
import json
import unittest

import default_capabilities_test as defaults
import plan_test
from dialogue_tools_test import text_response, tool_response


def review_input(body):
    latest = body["messages"][-1].get("content") or ""
    try:
        value = json.loads(latest)
    except (ValueError, TypeError):
        return None
    if isinstance(value, dict) and {"action", "program", "policy_reason", "user_messages"} <= value.keys():
        return value
    return None


class NaturalGoalAcceptance(defaults.DefaultCapabilitiesAcceptance):
    for name in dir(defaults.DefaultCapabilitiesAcceptance):
        if name.startswith("test_"):
            locals()[name] = None
    del name

    def goals(self):
        return self.documents().get("eve.cognition", {}).get("cognition.v1", {}).get("state", {}).get("goals", {})

    def test_default_chat_create_feedback_and_restart_keep_actual_goal_without_review(self):
        words = ["安排一次出行。", "先保留沿途路线。", "资料：" + "出发前检查天气。" * 500 + "尾部完整保留。",
                 "还需要明确到达时间。", "暂时保存待办，现实准备尚未完成。", "请记下一件待办：整理出行计划。"]
        feedback = "补充刚才的待办：出发时间改成上午九点。"
        stale = "再检查之前那条旧版本反馈有没有覆盖新时间。"
        inspect = "重启后查看刚才的出行待办和反思。"
        replies = {}

        def respond(body):
            self.assertIsNone(review_input(body), "普通待办无需模型审查")
            current = self.text(body)
            results = self.results(body)
            if current in words[:-1]:
                return self.say("收到。")
            self.assert_default_tools(body)
            if not results:
                if current == words[-1]:
                    return tool_response([("manage_goal", {"action": "create", "text": "整理出行计划"})])
                goal = replies["created"]["goal"]
                if current in (feedback, stale):
                    return tool_response([("manage_goal", {"action": "feedback", "goal_id": goal["id"],
                        "expected_revision": goal["revision"], "text": "出发时间改成上午九点"})])
                self.assertEqual(current, inspect)
                return tool_response([("manage_goal", {"action": "inspect", "goal_id": goal["id"]})])
            result = results[-1]
            if current == words[-1]:
                self.assertTrue(result["stored"])
                self.assertFalse(result["external_goal_completed"])
                self.assertEqual(result["goal"]["status"], "Waiting")
                replies["created"] = result
                return self.say("待办已记录，现实准备尚未完成。")
            if current == feedback:
                self.assertTrue(result["stored"])
                self.assertEqual(result["goal"]["revision"], replies["created"]["goal"]["revision"] + 1)
                self.assertEqual(result["goal"]["status"], "Waiting")
                replies["updated"] = result
                return self.say("九点出发的反馈已保存。")
            if current == stale:
                self.assertIn("error", result)
                self.assertIn("版本", result["error"]["message"])
                return self.say("旧版本反馈没有覆盖新时间。")
            self.assertEqual(current, inspect)
            self.assertEqual(result["goal"], replies["updated"]["goal"])
            self.assertFalse(result["goal_completed"])
            return self.say("原待办和九点出发的反馈都还在。")

        self.callback = respond
        script = []
        for index, word in enumerate(words):
            script.extend(self.send(f"natural-{index}", word,
                "待办已记录，现实准备尚未完成。" if index == 5 else "收到。"))
        script.extend(self.send("natural-feedback", feedback, "九点出发的反馈已保存。"))
        script.extend(self.send("natural-stale", stale, "旧版本反馈没有覆盖新时间。"))
        self.run_default(script)
        goal = replies["updated"]["goal"]
        self.assertEqual(self.goals()[goal["id"]], goal)
        self.assertFalse(any(review_input(request["body"]) for request in self.requests))
        audit_path = self.state / "business-permissions.json"
        before = audit_path.read_bytes()
        audit = json.loads(before)["audit"]
        self.assertEqual(len(audit), 2)
        for record, action in zip(audit, ["create", "feedback"]):
            self.assertEqual(record["outcome"], "host_authorized")
            self.assertEqual(record["user_messages"], [])
            self.assertIsNone(record["verdict"])
            operation = json.loads(record["target"])
            self.assertEqual(operation["tool"], "manage_goal")
            self.assertEqual(operation["operation"]["arguments"]["action"], action)
            self.assertEqual(operation["trusted_scope"], record["scope"])
            self.assertEqual(operation["turn_id"], record["turn_id"])
            self.assertTrue(operation["call_id"])
            self.assertFalse(operation["delegated"])
        self.assertEqual(json.loads(before)["grants"], {})
        # 已有平台回执阻止整段消息重放；新的查询由真实宿主读取同一待办。
        replay = []
        for step in script:
            if "send" in step:
                message = {key: value for key, value in step["send"].items() if not key.startswith("expected")}
                replay.append({"send": message})
                replay.append({"wait_command": {"id": message["id"], "type": "finish"}})
        self.run_default(replay + self.send("natural-inspect", inspect, "原待办和九点出发的反馈都还在。"))
        self.assertFalse(any(review_input(request["body"]) for request in self.requests))
        self.assertEqual(audit_path.read_bytes(), before)
        self.assertEqual(self.goals()[goal["id"]], goal)


class NaturalPlanAcceptance(plan_test.PlanAcceptance):
    for name in dir(plan_test.PlanAcceptance):
        if name.startswith("test_"):
            locals()[name] = None
    del name

    def provider_response(self, body, kind, fallback):
        if kind != "chat":
            return fallback
        return self.callback(body)

    def test_chat_plan_proposal_confirmation_and_verified_steps_survive_restart(self):
        create = "记录一件待办：帮忙制作 Mindustry 方块模组。"
        propose = "给刚才的模组待办拟一个先查资料再运行验证的计划，先展示步骤。"
        inspect = "查看这份计划的全部步骤和目前状态。"
        confirm = "确认刚才展示的步骤：先查官方资料，再实际运行最小模组验证。"
        observed = {}

        def respond(body):
            self.assertIsNone(review_input(body), "普通计划无需模型审查")
            messages = body["messages"]
            current_index = max(i for i, message in enumerate(messages) if message["role"] == "user")
            current = messages[current_index]["content"]
            results = [json.loads(message["content"]) for message in messages[current_index + 1:] if message["role"] == "tool"]
            if not results:
                if current == create:
                    return tool_response([("manage_goal", {"action": "create", "text": plan_test.GOAL})])
                goal_id = observed["goal"]["id"]
                if current == propose:
                    return tool_response([("manage_plan", {"action": "propose", "goal_id": goal_id})])
                if current == inspect:
                    return tool_response([("manage_plan", {"action": "inspect", "goal_id": goal_id})])
                self.assertEqual(current, confirm)
                plan = observed["plan"]
                return tool_response([("manage_plan", {"action": "confirm", "goal_id": goal_id,
                    "plan_id": plan["id"], "expected_revision": plan["revision"]})])
            result = results[-1]
            self.assertNotIn("error", result)
            if current == create:
                observed["goal"] = result["goal"]
                return text_response("已记录模组待办。")
            if current == propose:
                self.assertFalse(result["steps_started"])
                self.assertEqual(result["proposal"]["binding"]["goal_id"], observed["goal"]["id"])
                return text_response("正在拟定计划，尚未执行步骤。")
            if current == inspect:
                [observed["plan"]] = result["plans"]
                self.assertFalse(result["goal_completed"])
                if observed["plan"]["status"] == "proposed":
                    return text_response("计划是先查官方资料，再实际运行最小模组验证。")
                self.assertEqual(observed["plan"]["status"], "completed")
                return text_response("原计划的研究和运行验证均有结果。")
            self.assertEqual(current, confirm)
            self.assertEqual(result["plan"]["status"], "active")
            self.assertFalse(result["goal_completed"])
            return text_response("计划已确认，后台开始推进。")

        self.callback = respond
        self.inspect_run(lambda: self.wait_for(self.proposed, "计划未生成", 20), prefix=[
            *self.send(self.message("natural-create", create, expected="已记录模组待办。")),
            *self.send(self.message("natural-propose", propose, expected="正在拟定计划，尚未执行步骤。"))])
        self.assertEqual((self.research_runs(), self.practice_runs()), ([], []))
        self.inspect_run(lambda: self.wait_for(lambda: self.proposed("completed"), "计划步骤未完成", 40), prefix=[
            *self.send(self.message("natural-plan-inspect", inspect, expected="计划是先查官方资料，再实际运行最小模组验证。")),
            *self.send(self.message("natural-confirm", confirm, expected="计划已确认，后台开始推进。"))])
        [plan] = self.plans()
        [research] = self.research_runs()
        [practice] = self.practice_runs()
        self.assertEqual((research["status"], practice["status"]), ("Completed", "Verified"))
        self.assertEqual([step["status"] for step in plan["steps"]], ["satisfied", "satisfied"])
        self.assertEqual(plan["steps"][0]["attempts"][0]["evidence"]["source_id"], research["id"])
        self.assertEqual(plan["steps"][1]["attempts"][0]["evidence"]["source_id"], practice["id"])
        self.assertGreaterEqual(practice["started_at_ms"], research["finished_at_ms"])
        self.assertEqual(self.user_goal()["status"], "Waiting")
        self.assertEqual(self.kind("review"), [])
        audit_path = self.work / "state/business-permissions.json"
        business_before = audit_path.read_bytes()
        audit = json.loads(business_before)["audit"]
        self.assertEqual(len(audit), 3)
        self.assertTrue(all(record["outcome"] == "host_authorized" for record in audit))
        self.assertTrue(all(record["user_messages"] == [] and record["verdict"] is None for record in audit))
        record = audit[-1]
        saved = json.loads(record["target"])
        action = saved["operation"]
        self.assertEqual(saved["trusted_scope"], record["scope"])
        self.assertEqual(saved["turn_id"], record["turn_id"])
        self.assertTrue(saved["call_id"])
        self.assertEqual(action["arguments"]["action"], "confirm")
        self.assertEqual(action["plan"]["id"], plan["id"])
        self.assertEqual(action["plan"]["status"], "proposed")
        self.assertEqual([step["spec"]["capability"] for step in action["plan"]["steps"]],
                         [plan_test.RESEARCH, plan_test.PRACTICE])
        self.assertEqual(action["plan"]["steps"][1]["spec"]["depends_on"], ["learn"])
        self.assertEqual(action["goal"]["description"], plan_test.GOAL)
        self.assertEqual(json.loads(business_before)["grants"], {})
        before = self.ledger(), len(self.launches()), len(self.kind("plan")), len(self.kind("draft"))
        self.inspect_run(self.quiet, prefix=self.send(self.message("natural-plan-restart", inspect,
            expected="原计划的研究和运行验证均有结果。")))
        self.assertEqual((self.ledger(), len(self.launches()), len(self.kind("plan")), len(self.kind("draft"))), before)
        self.assertEqual(audit_path.read_bytes(), business_before)
        self.assertEqual(self.kind("review"), [])


if __name__ == "__main__":
    unittest.main()
