"""实际 eve-qqbot 的 QQ 计划入口验收；连接本地模型替身、本地文档站点、fake bridge 与 Mindustry 服务端替身。

用户用 /goal 保存待办，再用 /plan propose 请模型建议计划：宿主先保存请求记录，再发起一次无工具请求，
建议只能使用操作者已开启的受控研究与实践验证，校验后保存为待确认，不执行任何步骤。用户 /plan confirm
后，后台按依赖逐步推进：步骤先保存为执行中，待办才交给研究或实践后台；后台结束后按知识账本中
附逐字引用的知识、实践账本中实际验证通过的运行判定效果。重启不重放建议请求或任何步骤。
建议器是确定性替身，只证明宿主契约与恢复语义，不代表真实模型的计划质量。
"""
import json
import socket
import unittest
import urllib.error
import urllib.request

import practice_test

GOAL = "帮我做一个 Mindustry 方块模组"
RESEARCH = "eve.research.v1"
PRACTICE = "eve.practice.v1"
DISABLED = "计划入口未启用。"


def step(step_id, title, capability, depends_on=(), timeout_ms=600000):
    return {"id": step_id, "title": title, "capability": capability, "depends_on": list(depends_on),
            "max_attempts": 1, "timeout_ms": timeout_ms, "effect": {"kind": "verified"}}


TWO_STEPS = {"steps": [step("learn", "查官方资料", RESEARCH),
                       step("build", "做最小模组并实际运行", PRACTICE, ["learn"], 1800000)]}


def base():
    return practice_test.PracticeAcceptance


class PlanAcceptance(unittest.TestCase):
    # 复用实践验收的进程、模型替身、文档站点、服务端替身与持久状态读取。
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "inspect_run", "interest_state", "interests", "cognition", "goals", "learning_goal",
                  "kind", "quiet", "cognition_user", "default_observe", "observe_three_domains", "default_select",
                  "default_extract", "knowledge", "research_runs", "default_draft", "practice", "practice_runs",
                  "launches", "practiced", "wait_for"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)
        self.plan = lambda request: TWO_STEPS

    def tearDown(self):
        base().tearDown(self)

    def run_eve(self, script, plans=True, **options):
        more = (["--plans"] if plans else []) + list(options.pop("more", []))
        options.setdefault("max_executions", 4)
        return base().run_eve(self, script, more=more, **options)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "proposal_id" in decoded and "capabilities" in decoded:
            return "plan"
        return base().classify(self, decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "plan":
            return json.dumps(self.plan(decoded), ensure_ascii=False)
        return base().reply(self, kind, decoded, latest)

    def ledger(self):
        return self.documents().get("eve.plan", {}).get("plans.v1") or {"plans": [], "proposals": []}

    def plans(self):
        return self.ledger()["plans"]

    def user_goal(self):
        return next((goal for goal in self.goals() if goal["source"]["channel"] == "qq.goal"), None)

    def proposed(self, status="proposed"):
        return any(plan["status"] == status for plan in self.plans())

    def save_goal(self, text=GOAL):
        self.inspect_run(lambda: self.wait_for(lambda: self.user_goal() is not None, "goal was not saved", 15),
                         prefix=self.send(self.message("goal", f"/goal {text}", contains="待办已保存")))
        return self.user_goal()

    def test_panel_plan_reading_and_revision_guarded_withdrawal_persist_without_execution(self):
        goal = self.save_goal()
        gid = goal["id"]
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        token = "synthetic-plan-panel-token-1234567890"

        def api(endpoint, body=None, supplied_token=token):
            request = urllib.request.Request(f"http://127.0.0.1:{port}" + endpoint,
                data=None if body is None else json.dumps(body).encode(),
                headers={"Authorization": "Bearer " + supplied_token, "Content-Type": "application/json"})
            try:
                response = urllib.request.urlopen(request, timeout=5)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                return response.status, json.loads(response.read())

        def inspect_and_withdraw():
            self.wait_for(self.proposed, "plan was not proposed", 15)
            self.assertEqual(api("/api/goal/plans?id=" + gid, supplied_token="invalid")[0], 401)
            self.assertEqual(api("/api/goal/plans?id=%20")[0], 400)
            code, view = api("/api/goal/plans?id=" + gid)
            self.assertEqual(code, 200)
            self.assertEqual((view["goal_id"], view["plans"][0]["status"]), (gid, "proposed"))
            plan = view["plans"][0]
            request = {"plan_id": plan["id"], "revision": plan["revision"]}
            self.assertEqual(api("/api/goal/plans/withdraw", {**request, "revision": plan["revision"] + 1})[0], 409)
            self.assertEqual(self.plans()[0]["status"], "proposed")
            code, result = api("/api/goal/plans/withdraw", request)
            self.assertEqual((code, result["plan"]["status"]), (200, "withdrawn"))
            self.assertEqual(api("/api/goal/plans/withdraw", request)[0], 409)
            self.assertEqual(api("/api/goal/plans?id=" + gid)[1]["plans"][0]["status"], "withdrawn")

        options = {"more": ["--web-listen", f"127.0.0.1:{port}"], "env_extra": {"EVE_WEB_TOKEN": token}}
        self.inspect_run(inspect_and_withdraw,
            prefix=self.send(self.message("propose", f"/plan propose {gid}", contains="已请求计划建议")), **options)
        before = self.ledger()
        self.inspect_run(lambda: self.assertEqual(
            api("/api/goal/plans?id=" + gid)[1]["plans"][0]["status"], "withdrawn"), **options)
        self.assertEqual(self.ledger(), before)
        self.assertEqual(len(self.kind("plan")), 1)
        self.assertEqual((self.research_runs(), self.practice_runs()), ([], []))

    def test_confirmed_plan_researches_then_practices_and_settles_from_ledger_evidence(self):
        goal = self.save_goal()
        gid = goal["id"]
        self.inspect_run(lambda: self.wait_for(self.proposed, "plan was not proposed", 15),
                         prefix=self.send(self.message("propose", f"/plan propose {gid}", contains="已请求计划建议")))
        # 一次无工具请求；请求前已保存记录；建议只含已开启的能力；只建议，不执行。
        [request] = self.kind("plan")
        self.assertFalse(request["body"].get("tools"))
        body = json.loads(request["body"]["messages"][-1]["content"])
        self.assertEqual(body["goal"], GOAL)
        self.assertEqual([item["id"] for item in body["capabilities"]], [RESEARCH, PRACTICE])
        self.assertEqual(body["binding"], {"goal_id": gid, "goal_revision": goal["revision"], "input_sha256": None})
        saved = request["state"]["eve.plan"]["plans.v1"]["proposals"][0]
        self.assertEqual(saved["status"], {"kind": "requested"})
        self.assertEqual([run["topic"]["goal_id"] for run in self.research_runs()], [])
        self.assertEqual(self.practice_runs(), [])

        def completed():
            return any(plan["status"] == "completed" for plan in self.plans())

        self.inspect_run(lambda: self.wait_for(completed, "plan was not completed", 40), prefix=[
            *self.send(self.message("pending", f"/plans {gid}", contains=["待确认", "受控研究", "实践验证"])),
            *self.send(self.message("other", f"/plan confirm {gid}", user="user-2", contains="未找到该待办")),
            *self.send(self.message("confirm", f"/plan confirm {gid}", contains=["已确认", "查官方资料"]))])
        [plan] = self.plans()
        learn, build = plan["steps"]
        self.assertEqual((learn["status"], build["status"]), ("satisfied", "satisfied"))
        [research] = [run for run in self.research_runs() if run["topic"]["goal_id"] == gid]
        [practice] = [run for run in self.practice_runs() if run["task"]["goal_id"] == gid]
        self.assertEqual((research["status"], practice["status"]), ("Completed", "Verified"))
        self.assertEqual(learn["attempts"][0]["evidence"]["source_id"], research["id"])
        self.assertEqual(build["attempts"][0]["evidence"]["source_id"], practice["id"])
        # 依赖：研究结束后才开始实践；实践用到了这次研究得到的有来源知识。
        self.assertGreaterEqual(practice["started_at_ms"], research["finished_at_ms"])
        self.assertTrue(any(note["source_quoted"] for note in practice["task"]["notes"]))
        self.assertGreaterEqual(build["attempts"][0]["started_at_ms"], learn["attempts"][0]["finished_at_ms"])

        # 重启后查看：不重放建议请求、研究或实践，账本不变。
        before = (self.ledger(), len(self.kind("plan")), len(self.kind("draft")), len(self.launches()))
        self.run_eve([
            *self.send(self.message("show", f"/plans {gid}", contains=[
                "已完成", "第 1 步 查官方资料（受控研究）：已满足", "第 2 步 做最小模组并实际运行（实践验证）：已满足",
                f"证据：{practice['id']}"])),
            *self.send(self.message("list", "/plans", contains=["完成 2/2 步"])),
            *self.send(self.message("again", f"/plan propose {gid}", contains="已经请求过计划建议")),
            *self.send(self.message("help", "/plan", contains="用法：/plans")),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        self.assertEqual((self.ledger(), len(self.kind("plan")), len(self.kind("draft")), len(self.launches())), before)
        self.assertEqual(self.user_goal()["status"], "Waiting", "计划完成不改变待办状态")

    def test_invalid_proposal_is_not_retried_new_revision_can_propose_and_withdraw_and_disabled_is_explicit(self):
        goal = self.save_goal()
        gid = goal["id"]
        # 引用未登记的能力：记为不合规，不保存计划；同一版本不再请求。
        self.plan = lambda request: {"steps": [step("read", "读文件", "eve.file.observe.v1", timeout_ms=5000)]}
        self.inspect_run(lambda: self.wait_for(lambda: self.ledger()["proposals"] and all(
            record["status"] != {"kind": "requested"} for record in self.ledger()["proposals"]),
            "proposal did not finish", 15),
            prefix=self.send(self.message("propose", f"/plan propose {gid}", contains="已请求计划建议")))
        [record] = self.ledger()["proposals"]
        self.assertEqual(record["status"], {"kind": "failed", "failure": "invalid_output"})
        self.assertEqual(self.plans(), [])

        # 待办有了新版本后可以再请求；撤销待确认的计划不执行任何步骤。
        self.plan = lambda request: TWO_STEPS
        self.inspect_run(lambda: self.wait_for(self.proposed, "new revision was not proposed", 15), prefix=[
            *self.send(self.message("same", f"/plan propose {gid}", contains="已经请求过计划建议")),
            *self.send(self.message("feedback", f"/goal-feedback {gid} {goal['revision']} 先只做一面墙",
                                    contains="反馈已保存")),
            *self.send(self.message("again", f"/plan propose {gid}", contains="已请求计划建议"))])
        self.assertEqual(len(self.kind("plan")), 2)
        [plan] = self.plans()
        self.assertGreater(plan["binding"]["goal_revision"], goal["revision"])
        self.run_eve([
            *self.send(self.message("show", f"/plans {gid}", contains=["待确认", "建议不合规"])),
            *self.send(self.message("withdraw", f"/plan withdraw {gid}", contains="已撤销计划")),
            *self.send(self.message("none", f"/plan confirm {gid}", contains="没有待确认的计划")),
            *self.checkpoint("quiet-withdrawn")], checkpoints={"quiet-withdrawn": self.quiet})
        [plan] = self.plans()
        self.assertEqual(plan["status"], "withdrawn")
        self.assertTrue(all(item["status"] == "blocked" for item in plan["steps"]))
        self.assertEqual(self.practice_runs(), [])
        self.assertFalse([run for run in self.research_runs() if run["topic"]["goal_id"] == gid])

        # 未开启计划入口时如实说明，不请求建议。
        self.run_eve([*self.send(self.message("off", "/plans", contains=DISABLED)),
                      *self.checkpoint("quiet-off")], checkpoints={"quiet-off": self.quiet}, plans=False)
        self.assertEqual(len(self.kind("plan")), 2)

    def test_killed_process_interrupts_the_executing_step_and_restart_does_not_replay(self):
        goal = self.save_goal()
        gid = goal["id"]
        self.plan = lambda request: {"steps": [step("build", "做最小模组并实际运行", PRACTICE, timeout_ms=1800000)]}
        self.inspect_run(lambda: self.wait_for(self.proposed, "plan was not proposed", 15),
                         prefix=self.send(self.message("propose", f"/plan propose {gid}", contains="已请求计划建议")))
        # 实践在运行环境中停住时强制结束进程：步骤保存为执行中，实践保存为运行中。
        self.hold = self.gate("never-released")
        self.run_eve([*self.send(self.message("confirm", f"/plan confirm {gid}", contains="已确认")),
                      {"wait_file": str(self.gate("never-released"))}], stop_at=self.record)
        self.hold = None
        [plan] = self.plans()
        self.assertEqual(plan["steps"][0]["status"], "executing")
        launches = len(self.launches())
        self.run_eve([
            *self.send(self.message("show", f"/plans {gid}", contains=["已停止", "进程退出时中断，不重放"])),
            *self.checkpoint("quiet-restart")], checkpoints={"quiet-restart": self.quiet})
        [plan] = self.plans()
        self.assertEqual((plan["status"], plan["steps"][0]["status"]), ("blocked", "blocked"))
        self.assertEqual(plan["steps"][0]["attempts"][0]["failure"], "interrupted")
        [run] = self.practice_runs()
        self.assertEqual(run["status"], "Interrupted")
        self.assertEqual(len(self.launches()), launches, "重启不重放实践")


if __name__ == "__main__":
    unittest.main(verbosity=2)
