"""前台失败转后台、100 步申请、原会话批准及原操作不重放的真实进程验收。"""
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

    def run_recovery(self, script, **options):
        more = ["--background-tasks", "--sleep"] + options.pop("more", [])
        environment = {"EVE_DEVELOPER_CONFIG":str(self.developer_config),
                       "EVE_TASK_AUTO_RECOVER_FAILURES":"true"}
        environment.update(options.pop("env_extra", {}))
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
            more=more, env_extra=environment, **options)

    def wait_finished(self, status="Completed"):
        self.wait_for(lambda: any(t["status"] == status and (t.get("notice") or {}).get("status") == "Delivered" for t in self.tasks()), "恢复任务或通知未完成", 30)

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
        self.assertTrue(task["source"].startswith("failure:"))
        self.assertEqual((task["spec"]["max_steps"], task["usage"]["tool_calls"]), (100, 0))
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(self.turn("原任务检查")["status"]["state"], "Failed")
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
            *self.checkpoint("limit")],checkpoints={"limit":lambda:self.wait_finished("Failed")})
        [task]=self.tasks()
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
            checkpoints={"done":self.wait_finished})
        self.assertEqual(len(self.tasks()),2)
        self.assertEqual(self.worker_starts,2,"重复同意命令不创建或重跑接续任务")
        self.assertEqual(sorted(t["spec"]["max_steps"] for t in self.tasks()),[100,100])

    def test_uncertain_tool_effects_do_not_handoff(self):
        self.callback=lambda body: calls(("uncertain","run_command",{"program":sys.executable,"args":["-c","import time; time.sleep(2)"],"timeout_ms":50})) if not self.results(body) else {"choices":[]}
        self.run_recovery(self.send(self.message("unsafe","超时命令",contains=["存在未确认的工具副作用", "未自动派发"])))
        self.assertFalse(self.tasks())

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
