"""前台失败转后台、真实授权原话继承、100步申请和原操作不重放。

恢复可以继续安全执行；前台没有冻结完整验收合同时，模型声称完成仍须标为待核验。
"""
import hashlib
import json
import sys
import unittest

import delegation_context_test as delegation
from background_task_test import calls
from dialogue_tools_test import text_response


class AutoRecoveryAcceptance(delegation.DelegationContextAcceptance):
    for _name in dir(delegation.DelegationContextAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.review_requests = []

    def review_reply(self, request):
        self.review_requests.append(request)
        self.assertFalse(request["delegated"])
        self.assertEqual(request["program"], sys.executable)
        self.assertEqual(request["workspace"], str(self.source.resolve()))
        self.assertIn(request["arguments"], [
            ["-c", "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('SAVED_COMMAND_RECEIPT')"],
            ["-c", "from pathlib import Path; import time; p=Path('uncertain.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); time.sleep(5)"],
        ])
        self.assertTrue(request["user_messages"])
        self.assertIn(request["user_messages"][-1], [
            "原任务检查", "完成原任务并部署", "超时命令",
            "最后一条真实请求：检查文件并保留结果",
        ])
        return {"risk": "medium", "authorization": "high", "outcome": "allow",
                "rationale": "本地验收仅批准本例固定命令，按真实原话保留回执并检查不重放"}

    def run_recovery(self, script, **options):
        more = ["--background-tasks", "--sleep"] + options.pop("more", [])
        environment = {"EVE_DEVELOPER_CONFIG":str(self.developer_config),
                       "EVE_TASK_AUTO_RECOVER_FAILURES":"true"}
        environment.update(options.pop("env_extra", {}))
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
            more=more, env_extra=environment, **options)

    def wait_finished(self, status="Blocked", excluding=()):
        self.wait_for(lambda: any(t["id"] not in excluding and t["status"] == status
                                 and (t.get("notice") or {}).get("status") == "Delivered"
                                 for t in self.tasks()), "恢复任务或通知未完成", 30)

    def assert_unverified(self, task):
        self.assertEqual(task["status"], "Blocked")
        self.assertEqual(task["execution"]["completion_contract"], {"kind": "unspecified"})
        self.assertIn("missing_frozen_completion_contract", task["result"])

    def foreground(self, body):
        if not self.results(body):
            return calls(("foreground-once", "run_command", {"program":sys.executable,"args":["-c",
                "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('SAVED_COMMAND_RECEIPT')"]}))
        return {"choices":[]}  # 完成真实命令后，下一次模型响应违反协议。

    def test_error_handoff_has_100_steps_preserves_receipts_and_never_replays(self):
        def respond(body):
            if self.is_worker(body):
                self.assertIn("SAVED_COMMAND_RECEIPT", self.text(body))
                self.assertIn("原任务检查", self.text(body))
                self.assertFalse(self.definitions(body) & {"dispatch_background_task","run_subagent"})
                return text_response("原任务已核实完成，旧命令未重跑")
            return self.foreground(body)
        self.callback = respond
        self.run_recovery([*self.send(self.message("failed", "原任务检查", contains=["已保存并转交后台任务", "100 步"])),
            *self.checkpoint("finished")], checkpoints={"finished":self.wait_finished})
        [task] = self.tasks()
        self.assert_unverified(task)
        self.assertTrue(task["source"].startswith("failure:"))
        self.assertEqual(task["execution"]["review_messages"], ["原任务检查"])
        self.assertEqual((task["spec"]["max_steps"], task["usage"]["tool_calls"]), (100, 0))
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(self.turn("原任务检查")["status"]["state"], "Failed")
        self.assertEqual(self.turn("原任务检查")["status"]["failure"]["evidence"]["review_user_messages"],
                         ["原任务检查"])
        pushes = [json.loads(line) for line in (self.work / f"events-{self.runs}.jsonl").read_text().splitlines()
                  if json.loads(line).get("type") == "push"]
        self.assertTrue(pushes)
        self.assertNotIn("完成了", pushes[-1]["text"].splitlines()[0])
        self.run_recovery(self.send(self.message("again", "/tasks approve " + task["id"], contains=["没有等待追加步数"])))
        self.assertEqual(len(self.tasks()), 1)
        self.assertEqual((self.source / "once.txt").read_text(), "x")

    def test_budget_exhaustion_asks_original_group_and_requires_explicit_same_scope_approval(self):
        identity=json.dumps(["1904159860","group","group-1","user-1"],separators=(",",":")).encode()
        config=json.loads(self.developer_config.read_text())
        config["operators"].append("qq:"+hashlib.sha256(identity).hexdigest())
        self.developer_config.write_text(json.dumps(config))
        self.approved=False
        self.worker_starts=0
        def respond(body):
            if not self.is_worker(body):
                return {"choices":[]}
            if not self.results(body):
                self.worker_starts+=1
                if self.approved:
                    self.assertIn("明确同意追加",self.text(body))
                    return calls(("approved-complete","task_progress",{"status":"completed","remaining":"已完成并核验"}))
                return calls(("pending","task_progress",{"status":"in_progress","remaining":"还有核验工作未完成"}),
                             *[(f"echo-{i}","echo",{"text":str(i)}) for i in range(99)])
            return text_response("追加后已完成" if self.approved else "已有进度保存，仍需继续核验")
        self.callback=respond
        route={"scope":"group","target":"group-1"}
        self.run_recovery([*self.send(self.message("group-fail","群内原任务",contains=["已保存并转交后台任务"],**route)),
            *self.checkpoint("limit")],checkpoints={"limit":self.wait_finished})
        [task]=self.tasks()
        self.assertEqual(task["status"], "Blocked")
        self.assertIn("100 步预算不足", task["result"])
        self.assertEqual(task["execution"]["review_messages"], ["群内原任务"])
        self.assertEqual(task["usage"]["tool_calls"],100)
        self.assertEqual(task["execution"]["checkpoint"]["remaining"],"还有核验工作未完成")
        events=[json.loads(l) for l in (self.work/f"events-{self.runs}.jsonl").read_text().splitlines()]
        [push]=[e for e in events if e.get("type")=="push"]
        self.assertEqual((push["scope"],push["target_id"]),("group","group-1"))
        self.assertIn("/tasks approve "+task["id"],push["text"])
        self.assertNotIn("完成了",push["text"])
        self.assertEqual(self.worker_starts,1)
        self.approved=True
        self.run_recovery([*self.send(self.message("wrong-scope","/tasks approve "+task["id"],contains=["本会话没有这个任务"])),
            *self.send(self.message("approve","/tasks approve "+task["id"],contains=["已确认追加最多 100 步"],**route)),
            *self.checkpoint("done"),
            *self.send(self.message("duplicate","/tasks approve "+task["id"],contains=["已确认追加最多 100 步"],**route))],
            checkpoints={"done":lambda:self.wait_finished(excluding=(task["id"],))})
        self.assertEqual(len(self.tasks()),2)
        self.assertEqual(self.worker_starts,2,"重复同意命令不创建或重跑接续任务")
        self.assertEqual(sorted(t["spec"]["max_steps"] for t in self.tasks()),[100,100])
        [continued] = [saved for saved in self.tasks() if saved["id"] != task["id"]]
        self.assert_unverified(continued)
        self.assertEqual(continued["execution"]["review_messages"],
                         ["群内原任务", "/tasks approve " + task["id"]])
        self.assertEqual(continued["execution"]["completion_contract"],
                         task["execution"]["completion_contract"])

    def test_uncertain_tool_effects_do_not_handoff(self):
        self.callback=lambda body: calls(*[(f"checked-{i}", "echo", {"text": str(i)}) for i in range(17)],
            ("uncertain","run_command",{"program":sys.executable,"args":["-c",
                "from pathlib import Path; import time; p=Path('uncertain.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); time.sleep(5)"],"timeout_ms":1000})) if not self.results(body) else {"choices":[]}
        self.run_recovery(self.send(self.message("unsafe","超时命令",contains=["模型服务返回空回复", "已启动 18 次工具调用，收到 18 条工具回执", "存在未确认的工具副作用", "第 18 条调用 run_command 执行异常", "未自动派发"])))
        self.assertFalse(self.tasks())
        failure = self.turn("超时命令")["status"]["failure"]
        self.assertEqual(failure["evidence"]["protocol_failure"], "EmptyResponse")
        self.assertEqual(len(failure["evidence"]["tool_results"]), 18)
        self.assertEqual((self.source / "uncertain.txt").read_text(), "x")
        self.callback = lambda body: text_response("已保留失败回执，没有重跑")
        self.run_recovery(self.send(self.message("uncertain-restart", "重启后核对", expected="已保留失败回执，没有重跑")))
        self.assertEqual(self.turn("超时命令")["status"]["failure"], failure)
        self.assertEqual((self.source / "uncertain.txt").read_text(), "x")

    def test_preflight_rejection_is_not_an_unconfirmed_side_effect(self):
        def respond(body):
            if self.is_worker(body):
                self.assertIn("UnknownTool", self.text(body))
                return text_response("已经核对拒绝回执，未发生外部操作")
            if not self.results(body):
                return calls(("not-installed", "nonexistent_tool", {}))
            return {"choices": []}
        self.callback = respond
        self.run_recovery([*self.send(self.message("rejected", "核对拒绝的调用", contains=["已保存并转交后台任务"])),
            *self.checkpoint("rejected-done")], checkpoints={"rejected-done": self.wait_finished})
        [task] = self.tasks()
        self.assert_unverified(task)
        self.assertEqual(self.turn("核对拒绝的调用")["status"]["failure"]["started_tools"], 0)

    def test_recovery_preserves_six_complete_originals_and_never_uses_synthetic_authorization(self):
        originals = [f"原始授权 {index}\n" + ("只在原范围工作。" * 150) + f"\n完整尾部 {index}"
                     for index in range(7)]
        failed_input = "最后一条真实请求：检查文件并保留结果"
        expected = originals[-5:] + [failed_input]
        worker_requests = []

        def respond(body):
            if self.is_worker(body):
                worker_requests.append(body)
                self.assertIn("eve.failed_turn", self.text(body))
                return text_response("已检查现有回执，未规定完整验收条件")
            if self.text(body) != failed_input:
                return text_response("收到原始范围")
            return self.foreground(body)

        self.callback = respond
        script = []
        for index, text in enumerate(originals):
            script.extend(self.send(self.message(f"original-{index}", text, expected="收到原始范围")))
        script.extend(self.send(self.message("foreign", "其他人的内容不能成为授权",
                                             user="user-2", expected="收到原始范围")))
        script.extend(self.send(self.message("failed-last", failed_input,
                                             contains=["已保存并转交后台任务"])))
        script.extend(self.checkpoint("review-preserved"))
        self.run_recovery(script, checkpoints={"review-preserved": self.wait_finished})
        [task] = self.tasks()
        self.assert_unverified(task)
        self.assertEqual(task["execution"]["review_messages"], expected)
        failure = self.turn(failed_input)["status"]["failure"]
        self.assertEqual(failure["evidence"]["review_user_messages"], expected)
        self.assertEqual(len(worker_requests), 1)
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        frozen = task["execution"]["review_messages"]
        self.callback = lambda body: text_response("恢复记录仍然保留")
        self.run_recovery(self.send(self.message("restart-evidence", "重启后核对",
                                                 expected="恢复记录仍然保留")))
        [restored] = self.tasks()
        self.assertEqual(restored["execution"]["review_messages"], frozen)
        self.assertEqual(restored["status"], "Blocked")
        self.assertEqual((self.source / "once.txt").read_text(), "x")

    def test_hundred_calls_report_progress_with_large_context_and_no_replay_after_restart(self):
        self.closing_requests = 0
        def respond(body):
            self.assertFalse(self.is_worker(body), "用尽额度不自动再开 100 步后台任务")
            if not body.get("tools"):
                self.assertEqual(body.get("tool_choice"), "none")
                self.closing_requests += 1
                if self.closing_requests == 1:
                    return calls(("over-budget", "run_command", {"program":sys.executable,"args":["-c", "raise RuntimeError('不得执行')"]}))
                self.assertTrue(body["messages"][0]["content"].startswith("你是 Eve，现在只向原会话用户汇报"))
                self.assertTrue(all(m["role"] in ("system", "user") for m in body["messages"]))
                report_data = self.text(body)  # 正式对话 Provider 仍会注入表达约束。
                data = json.loads(report_data)
                self.assertEqual(data["kind"], "eve.read_only_progress_report")
                self.assertEqual(data["remaining"], "部署尚未执行")
                self.assertGreater(len(report_data.encode()), 2 * 1024 * 1024)
                results = [r for m in data["execution_record"] for r in m["tool_results"]]
                self.assertEqual(len(results), 100)
                self.assertIn("SAVED_COMMAND_RECEIPT", json.dumps(results))
                self.assertNotIn("over-budget", json.dumps(data))
                return text_response("已完成文件写入和 48 项核验，部署尚未执行，需要批准追加步数")
            return calls(
                ("pending", "task_progress", {"status":"in_progress","remaining":"部署尚未执行"}),
                ("write-once", "run_command", {"program":sys.executable,"args":["-c",
                    "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('SAVED_COMMAND_RECEIPT')"]}),
                *[(f"echo-{i}", "echo", {"text":"RECEIPT_START" + "x" * 32000 + f"RECEIPT_END_{i}"}) for i in range(98)])
        self.callback = respond
        self.run_recovery(self.send(self.message("limit-report", "完成原任务并部署", contains=["已完成文件写入", "部署尚未执行", "批准追加步数"])),
            env_extra={"EVE_CONTEXT_COMPACT_AT_TOKENS":"0"})
        self.assertEqual(self.closing_requests, 2)
        self.assertFalse(self.tasks())
        turn = self.turn("完成原任务并部署")
        self.assertEqual(turn["status"]["state"], "Completed")
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.callback = lambda body: text_response("原有进度仍保留")
        self.run_recovery(self.send(self.message("after-restart", "核对原有进度", expected="原有进度仍保留")),
            env_extra={"EVE_CONTEXT_COMPACT_AT_TOKENS":"0", "EVE_HISTORY_MAX_BYTES":"8388608"})
        self.assertEqual(self.turn("完成原任务并部署"), turn)
        self.assertFalse(self.tasks())
        self.assertEqual((self.source / "once.txt").read_text(), "x")

    def test_a_provider_that_refuses_reporting_cannot_create_another_budget(self):
        def respond(body):
            self.assertFalse(self.is_worker(body))
            if not self.results(body):
                return calls(*[(f"echo-{i}", "echo", {"text":str(i)}) for i in range(100)])
            return calls(("never-run", "echo", {"text":"不得执行"}))
        self.callback = respond
        self.run_recovery(self.send(self.message("no-report", "故障收尾", contains=["模型未能完成只读进度汇报", "未自动追加执行"])))
        self.assertFalse(self.tasks())
        failure = self.turn("故障收尾")["status"]["failure"]
        self.assertEqual((failure["code"], failure["started_tools"]), ("RoundLimit", 100))
        self.assertEqual(len(failure["evidence"]["tool_results"]), 100)
        self.assertEqual(self.chat_rounds, 3, "两次只读收尾失败后停止，不无限请求模型")

    def test_compatible_fallback_disables_new_handoffs_without_dropping_task_support(self):
        self.callback=lambda body: {"choices":[]}
        self.run_recovery(self.send(self.message("fallback","故障兜底检查",contains=["本轮已停止", "不再自动重试"])), more=["--no-error-recovery"])
        self.assertFalse(self.tasks())


if __name__ == "__main__":
    unittest.main()
