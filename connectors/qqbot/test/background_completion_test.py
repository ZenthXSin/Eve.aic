"""真实 QQ 后台验收：冻结合同、真实回执、有限修复和重启后的完成语义。

模型与 QQ 为本地替身；文件读写由真实 developer 工具执行。替身故意自报完成、返回错值，
验收只相信持久账本和实际回执，不用模型的成功文字判断目标是否完成。
"""
import hashlib
import json
import sys
import threading
import unittest

import delegation_context_test as context
from background_task_test import calls
from dialogue_tools_test import text_response


class BackgroundCompletionAcceptance(context.DelegationContextAcceptance):
    for _name in dir(context.DelegationContextAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.hold = threading.Event()
        self.request_text = "派发完成验收"

    def tearDown(self):
        self.hold.set()
        super().tearDown()

    @staticmethod
    def text_contract(*fragments, expected=None):
        contract = {"kind": "text", "required_fragments": list(fragments),
                    "valid_json": False, "json_required_pointers": []}
        if expected is not None:
            contract["json_equals"] = expected
        return contract

    @staticmethod
    def file_contract(path="artifact.txt", expected="真实验收通过"):
        return {"kind": "receipts", "contract": {"checks": [{
            "tool": "read_file", "arguments": {"path": path},
            "output_pointer": "/sha256", "expected": hashlib.sha256(expected.encode()).hexdigest()}],
            "require_final_batch": True}}

    @staticmethod
    def worker_results(body):
        # 修复提示可以追加宿主消息，旧工具回执仍须在原检查点里保留。
        return [json.loads(message["content"]) for message in body["messages"]
                if message["role"] == "tool"]

    def worker_requests(self):
        return [request["body"] for request in self.kind("chat") if self.is_worker(request["body"])]

    def assert_repair_has_no_terminal_result(self):
        [task] = self.tasks()
        self.assertEqual(task["status"], "Running")
        for field in ("notice", "result", "usage", "finished_at_ms"):
            self.assertIsNone(task[field], f"修复过程中不得提前保存终局 {field}")
        self.assertEqual(self.pushes(), [], "中间验收失败不能发送伪终局通知")

    def enabled(self, script, **options):
        more = ["--background-tasks", "--subagents"] + options.pop("more", [])
        environment = {"EVE_DEVELOPER_CONFIG": str(self.developer_config)}
        environment.update(options.pop("env_extra", {}))
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                            more=more, env_extra=environment,
                            **options)

    def foreground(self, body, verification=None, steps=4):
        if self.text(body) != self.request_text:
            return text_response("后台状态已保留")
        if not self.results(body):
            arguments = {"title": "完成验收", "instructions": "交付本次指定产物，依据实际证据验收。",
                         "max_steps": steps}
            if verification is not None:
                arguments["verification"] = verification
            return calls(("dispatch", "dispatch_background_task", arguments))
        [result] = self.results(body)
        self.assertTrue(result["dispatched"], result)
        self.assertEqual(result["task"]["max_steps"], steps)
        return text_response("后台已派发")

    def wait_finished(self, status):
        self.wait_for(lambda: any(task["status"] == status
                                 and (task.get("notice") or {}).get("status") == "Delivered"
                                 for task in self.tasks()),
                      f"任务或通知未到达 {status}", 45)

    def dispatch_and_wait(self, status, **options):
        self.enabled([*self.send(self.message("dispatch", self.request_text, expected="后台已派发")),
                      *self.checkpoint("finished")],
                     checkpoints={"finished": lambda: self.wait_finished(status)}, timeout=60,
                     **options)
        [task] = self.tasks()
        return task

    def pushes(self):
        return [event for path in sorted(self.work.glob("events-*.jsonl"))
                for line in path.read_text().splitlines()
                if (event := json.loads(line)).get("type") == "push"]

    def test_missing_contract_never_completes_and_restart_preserves_terminal_budget(self):
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, steps=1)
            if not self.worker_results(body):
                return calls(("echo-claim", "echo", {"text": "模型声称所有工作已经完成"}))
            self.assertFalse(body.get("tools"), "原一步预算已用尽")
            return text_response("模型声称所有工作已经完成")
        self.callback = respond
        task = self.dispatch_and_wait("Blocked")
        self.assertEqual(task["usage"], {"tool_calls": 1, "model_requests": 2})
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertTrue(task["result"].startswith("任务尚未通过冻结验收："), task["result"])
        [notice] = self.pushes()
        self.assertIn("等待补充信息或解除阻塞", notice["text"])
        self.assertNotIn("完成了：", notice["text"])
        before_requests = len(self.worker_requests())
        self.enabled(self.send(self.message("restart", "重启后核对", expected="后台状态已保留")))
        self.assertEqual(self.tasks(), [task])
        self.assertEqual(len(self.worker_requests()), before_requests)
        self.assertEqual(len(self.pushes()), 1, "已送达的终局通知不能重启后重发")

    def test_text_contract_requires_exact_json_and_fragments_without_offering_tools(self):
        expected = {"title": "两项安排", "items": ["先核对", "再汇报"]}
        verification = self.text_contract("先核对", "再汇报", expected=expected)
        rendered = json.dumps(expected, ensure_ascii=False)
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification)
            self.assertFalse(body.get("tools"), "纯文本交付不允许通过工具产生外部副作用")
            self.assertIn("宿主已冻结以下验收条件", self.text(body))
            return text_response(rendered)
        self.callback = respond
        task = self.dispatch_and_wait("Completed")
        self.assertEqual(task["result"], rendered)
        self.assertEqual(task["usage"], {"tool_calls": 0, "model_requests": 1})
        self.assertEqual(task["execution"]["completion_contract"], verification)
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertIn("完成了：", self.pushes()[0]["text"])

    def test_text_contract_cannot_execute_a_model_supplied_write_call(self):
        verification = self.text_contract("文本已经交付")
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=2)
            self.assertFalse(body.get("tools"), "Text 合同执行者不能获得外部工具")
            if not self.worker_results(body):
                return calls(("smuggled-write", "write_file", {"path": "must-not-exist.txt",
                    "content": "模型试图绕过纯文本边界", "expected_sha256": ""}))
            self.assertEqual(self.worker_results(body)[0]["error"]["code"], "UnknownTool")
            return text_response("文本已经交付")
        self.callback = respond
        task = self.dispatch_and_wait("Blocked")
        self.assertFalse((self.source / "must-not-exist.txt").exists())
        self.assertEqual(task["usage"], {"tool_calls": 1, "model_requests": 2})
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertIn("text_contract_cannot_verify_tool_effects", task["result"])

    def test_self_reported_echo_receipt_is_rejected_before_dispatch(self):
        def respond(body):
            self.assertFalse(self.is_worker(body), "自报完成合同不能创建执行者")
            if not self.results(body):
                return calls(("invalid-verification", "dispatch_background_task", {
                    "title": "无效验收", "instructions": "不得执行", "verification": {
                        "kind": "receipts", "contract": {"checks": [{"tool": "echo",
                            "arguments": {"text": "已完成"}, "output_pointer": "/echo",
                            "expected": "已完成"}], "require_final_batch": True}}}))
            self.assertIn("verification", json.dumps(self.results(body)))
            return text_response("验收条件无效，未派发")
        self.callback = respond
        self.enabled(self.send(self.message("invalid", "派发自报完成验收", expected="验收条件无效，未派发")))
        self.assertEqual(self.tasks(), [])
        self.assertEqual(self.worker_requests(), [])
        self.assertEqual(self.pushes(), [])

    def test_wrong_text_is_repaired_from_the_same_checkpoint_before_completion(self):
        expected = {"answer": "完整文本产物"}
        verification = self.text_contract("完整文本产物", expected=expected)
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=1)
            self.assertFalse(body.get("tools"))
            if self.tasks()[0]["execution"]["recoveries"] == 0:
                return text_response('{"answer":"错值"}')
            self.assert_repair_has_no_terminal_result()
            self.assertIn("错值", json.dumps(body["messages"], ensure_ascii=False), "保留先前失败产物用于纠正")
            return text_response(json.dumps(expected, ensure_ascii=False))
        self.callback = respond
        task = self.dispatch_and_wait("Completed")
        self.assertEqual(task["execution"]["recoveries"], 1)
        self.assertEqual(task["usage"], {"tool_calls": 0, "model_requests": 2})
        self.assertEqual(task["spec"]["max_steps"], 1)
        self.assertEqual(task["execution"]["completion_contract"], verification)
        [notice] = self.pushes()
        self.assertIn("完成了：", notice["text"])

    def test_text_mismatch_stops_after_three_repairs_and_does_not_restart(self):
        verification = self.text_contract("必须出现的交付正文")
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=1)
            self.assertFalse(body.get("tools"))
            return text_response("缺少约定正文，模型仍声称完成")
        self.callback = respond
        task = self.dispatch_and_wait("Blocked")
        self.assertEqual(task["execution"]["recoveries"], 3)
        self.assertEqual(task["usage"], {"tool_calls": 0, "model_requests": 4})
        self.assertEqual(task["spec"]["max_steps"], 1)
        self.assertTrue(task["result"].startswith("任务尚未通过冻结验收："), task["result"])
        self.enabled(self.send(self.message("restart", "重启后核对", expected="后台状态已保留")))
        self.assertEqual(self.tasks(), [task])
        self.assertEqual(len(self.worker_requests()), 4, "三次修复上限在重启后继续有效")

    def test_missing_then_wrong_receipt_repair_uses_real_final_file_and_original_budget(self):
        (self.source / "artifact.txt").write_text("尚未修复", encoding="utf8")
        verification = self.file_contract()
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=4)
            recoveries = self.tasks()[0]["execution"]["recoveries"]
            if recoveries:
                self.assert_repair_has_no_terminal_result()
            results = self.worker_results(body)
            if recoveries == 0:
                self.assertFalse(results)
                return text_response("没有读取文件，但模型声称完成")
            if recoveries == 1:
                if not results:
                    return calls(("read-wrong", "read_file", {"path": "artifact.txt"}))
                self.assertEqual(results[-1]["content"], "尚未修复")
                return text_response("读到错值，但模型声称完成")
            self.assertEqual(recoveries, 2)
            if len(results) == 1:
                return calls(("repair-file", "write_file", {"path": "artifact.txt", "content": "真实验收通过",
                                                           "expected_sha256": results[0]["sha256"]}))
            if len(results) == 2:
                self.assertTrue(results[-1]["success"])
                return calls(("read-final", "read_file", {"path": "artifact.txt"}))
            self.assertEqual(len(results), 3)
            self.assertEqual(results[-1]["content"], "真实验收通过")
            return text_response("已根据最终文件真实回执核验完成")
        self.callback = respond
        task = self.dispatch_and_wait("Completed")
        self.assertEqual((self.source / "artifact.txt").read_text(), "真实验收通过")
        self.assertEqual(task["execution"]["recoveries"], 2)
        self.assertEqual(task["spec"]["max_steps"], 4)
        self.assertEqual(task["usage"], {"tool_calls": 3, "model_requests": 6})
        checkpoint = task["execution"]["checkpoint"]
        self.assertEqual(checkpoint["used_tool_calls"], 3)
        self.assertEqual([call["name"] for message in checkpoint["messages"] for call in message["tool_calls"]],
                         ["read_file", "write_file", "read_file"], "修复不重放原有工具批次")
        self.assertEqual(task["execution"]["completion_contract"], verification)
        [notice] = self.pushes()
        self.assertIn("完成了：", notice["text"])

    def test_matching_old_receipt_does_not_pass_after_later_file_mutation(self):
        (self.source / "artifact.txt").write_text("真实验收通过", encoding="utf8")
        verification = self.file_contract()
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=2)
            results = self.worker_results(body)
            if not results:
                return calls(("before-mutation", "read_file", {"path": "artifact.txt"}))
            if len(results) == 1:
                self.assertEqual(results[0]["content"], "真实验收通过")
                return calls(("mutate-after-check", "write_file", {"path": "artifact.txt", "content": "已被改坏",
                                                                   "expected_sha256": results[0]["sha256"]}))
            self.assertFalse(body.get("tools"))
            return text_response("引用旧回执，模型声称完成")
        self.callback = respond
        task = self.dispatch_and_wait("Blocked")
        self.assertEqual((self.source / "artifact.txt").read_text(), "已被改坏")
        self.assertEqual(task["usage"], {"tool_calls": 2, "model_requests": 3})
        self.assertEqual(task["execution"]["recoveries"], 0, "不能为验收修复扩大耗尽的原预算")
        self.assertTrue(task["result"].startswith("任务尚未通过冻结验收："), task["result"])

    def test_wrong_receipt_with_exhausted_budget_never_starts_a_repair(self):
        (self.source / "artifact.txt").write_text("错值", encoding="utf8")
        verification = self.file_contract()
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=1)
            if not self.worker_results(body):
                return calls(("read-only-step", "read_file", {"path": "artifact.txt"}))
            self.assertFalse(body.get("tools"))
            return text_response("预算已用尽，文件仍是错值")
        self.callback = respond
        task = self.dispatch_and_wait("Blocked")
        self.assertEqual(task["usage"], {"tool_calls": 1, "model_requests": 2})
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertEqual((self.source / "artifact.txt").read_text(), "错值")

    def test_explicitly_blocked_progress_does_not_start_automatic_completion_repair(self):
        (self.source / "artifact.txt").write_text("尚未核验", encoding="utf8")
        verification = self.file_contract()
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=4)
            if not self.worker_results(body):
                return calls(("blocked", "task_progress", {
                    "status": "blocked", "remaining": "需要原用户提供目标文件版本"}))
            return text_response("需要原用户提供目标文件版本，已有记录保留")
        self.callback = respond
        task = self.dispatch_and_wait("Blocked", more=["--sleep"])
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertEqual(task["usage"], {"tool_calls": 1, "model_requests": 2})
        self.assertIsNone(task["execution"]["checkpoint"]["remaining"])
        self.assertIn("需要原用户提供目标文件版本", task["result"])
        self.assertEqual((self.source / "artifact.txt").read_text(), "尚未核验")

    def test_unknown_command_effect_is_not_replayed_for_completion_repair(self):
        (self.source / "artifact.txt").write_text("真实验收通过", encoding="utf8")
        verification = self.file_contract()
        self.request_text = "在临时工作区执行验收命令：给 uncertain.txt 追加一次 x，然后等待五秒；一秒超时后保留回执，不重放命令。"
        reviews = []
        def review(request):
            reviews.append(request)
            return {"risk": "low", "authorization": "unknown", "outcome": "deny",
                    "rationale": "普通临时文件命令不应调用系统安全审查"}
        self.review_reply = review
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=4)
            self.assertFalse(self.worker_results(body), "未知效果之后不得继续请求模型")
            return calls(("timed-command", "run_command", {"program": sys.executable,
                "args": ["-c", "from pathlib import Path; import time; p=Path('uncertain.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); time.sleep(5)"],
                "timeout_ms": 1000}), ("matching-final-read", "read_file", {"path": "artifact.txt"}))
        self.callback = respond
        # 在未知工具效果之后，LLM 宿主不能保存可恢复的最终结果；严格任务保留真实失败为阻塞。
        task = self.dispatch_and_wait("Blocked")
        self.assertEqual(reviews, [], "普通临时命令直接执行，超时后的未知效果仍不得重放")
        self.assertEqual((self.source / "uncertain.txt").read_text(), "x")
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertEqual(task["usage"], {"tool_calls": 2, "model_requests": 1})
        self.assertIn("未知工具效果不重放", task["result"])
        self.enabled(self.send(self.message("restart", "重启后核对", expected="后台状态已保留")))
        self.assertEqual(self.tasks(), [task])
        self.assertEqual((self.source / "uncertain.txt").read_text(), "x")
        self.assertEqual(len(self.worker_requests()), 1)

    def test_restart_finishes_saved_real_receipt_without_replaying_or_resetting_budget(self):
        (self.source / "artifact.txt").write_text("真实验收通过", encoding="utf8")
        verification = self.file_contract()
        waiting_for_final = threading.Event()
        def respond(body):
            if not self.is_worker(body):
                return self.foreground(body, verification, steps=1)
            if not self.worker_results(body):
                return calls(("saved-read", "read_file", {"path": "artifact.txt"}))
            self.assertFalse(body.get("tools"), "重启不补充原一步预算")
            self.assertEqual(self.worker_results(body)[-1]["content"], "真实验收通过")
            waiting_for_final.set()
            self.hold.wait(20)
            return text_response("已依据持久真实回执核验完成")
        self.callback = respond
        stop = self.gate("stop-before-finish")
        def saved():
            self.wait_for(lambda: waiting_for_final.is_set() and any(task["status"] == "Running"
                and (checkpoint := task["execution"].get("checkpoint"))
                and checkpoint["used_tool_calls"] == 1 and not checkpoint["pending_calls"]
                and checkpoint["provider_requests"] >= 2 and checkpoint["result"] is None
                for task in self.tasks()),
                "最终读取回执未保存", 20)
        self.enabled([*self.send(self.message("dispatch", "派发完成验收", expected="后台已派发")),
                      *self.checkpoint("saved"), {"touch": str(stop)},
                      {"wait_file": str(self.gate("never-released"))}],
                     checkpoints={"saved": saved}, stop_at=stop,
                     env_extra={"EVE_TASK_TIMEOUT_MS": "60000"})
        self.hold.set()
        [before] = self.tasks()
        self.assertEqual(before["execution"]["checkpoint"]["provider_requests"], 2)
        self.assertIsNotNone(before["execution"]["deadline_at_ms"])
        self.enabled(self.checkpoint("resumed"),
                     checkpoints={"resumed": lambda: self.wait_finished("Completed")},
                     env_extra={"EVE_TASK_TIMEOUT_MS": "60000"})
        [task] = self.tasks()
        self.assertEqual(task["id"], before["id"])
        self.assertEqual(task["spec"]["max_steps"], 1)
        self.assertEqual(task["usage"], {"tool_calls": 1, "model_requests": 3})
        self.assertEqual(task["execution"]["recoveries"], 1)
        self.assertEqual(task["execution"]["deadline_at_ms"], before["execution"]["deadline_at_ms"])
        self.assertEqual(task["execution"]["completion_contract"], verification)
        self.assertEqual(task["execution"]["checkpoint"]["used_tool_calls"], 1)
        self.assertEqual(task["execution"]["checkpoint"]["provider_requests"], 3)
        self.assertEqual(sum(len(message["tool_calls"]) for message in task["execution"]["checkpoint"]["messages"]), 1)
        self.assertEqual(len(self.worker_requests()), 3)


if __name__ == "__main__":
    unittest.main(verbosity=2)
