"""真实 QQ 进程故障恢复：实际命令只执行一次，检查点接续沿用预算和授权。"""
import hashlib
import json
import sys
import threading
import unittest

import delegation_context_test as context
from background_task_test import calls
from dialogue_tools_test import text_response


class BackgroundRecoveryAcceptance(context.DelegationContextAcceptance):
    for _name in dir(context.DelegationContextAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.hold = threading.Event()
        self.hold_after_command = True
        self.once_command = {"program": sys.executable, "args": ["-c",
            "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); "
            "assert p.read_text() == 'x', 'command replayed or artifact invalid'; print('ACTUAL_SAVED_RECEIPT')"]}
        self.inflight_command = {"program": sys.executable, "args": ["-c",
            "from pathlib import Path; import time; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); time.sleep(2)"]}
        self.readback = {"path": "once.txt", "offset": 0}
        self.expected_hash = hashlib.sha256(b"x").hexdigest()
        self.verification = {"kind": "receipts", "contract": {"checks": [{
            "tool": "read_file", "arguments": self.readback,
            "output_pointer": "/sha256", "expected": self.expected_hash}], "require_final_batch": True}}

    def review_reply(self, request):
        self.assertTrue(request["delegated"])
        self.assertEqual(request["program"], sys.executable)
        self.assertIn(request["arguments"], [self.once_command["args"], self.inflight_command["args"]])
        self.assertEqual(request["workspace"], str(self.source.resolve()))
        self.assertTrue(any("派发恢复任务" in text for text in request["user_messages"]))
        return {"risk": "medium", "authorization": "high", "outcome": "allow",
                "rationale": "隔离验收仅执行固定命令并保留原任务授权和执行回执"}

    def tearDown(self):
        self.hold.set()
        super().tearDown()

    def callback_for_task(self, body):
        if not self.is_worker(body):
            if self.text(body) != "派发恢复任务":
                return text_response("背景已收到")
            if not self.results(body):
                return calls(("dispatch", "dispatch_background_task", {
                    "title":"恢复验收", "instructions":"执行一次命令，再读取 once.txt 核对内容哈希后完成。", "max_steps":2,
                    "include_context":True, "verification":self.verification}))
            self.assertTrue(self.results(body)[-1]["dispatched"])
            return text_response("后台已派发")
        self.assertIn("FROZEN_PARENT_561", self.text(body))
        self.assertNotIn("AFTER_SNAPSHOT_874", self.text(body))
        if not self.results(body):
            return calls(("actual-once", "run_command", self.once_command))
        self.assertIn("ACTUAL_SAVED_RECEIPT", json.dumps(self.results(body)))
        if len(self.results(body)) == 1:
            return calls(("verify-once", "read_file", self.readback))
        self.assertEqual(len(self.results(body)), 2)
        self.assertEqual(self.results(body)[-1]["sha256"], self.expected_hash)
        self.assertEqual(self.results(body)[-1]["content"], "x")
        self.assertFalse(body.get("tools"), "恢复不能补充已耗尽的两步预算")
        if self.hold_after_command:
            self.hold.wait(20)
        return text_response("恢复验收完成，命令仅执行一次")

    def seed_and_interrupt(self):
        self.callback = self.callback_for_task
        stop = self.gate("stop-checkpoint")
        def inspect():
            self.wait_for(lambda: (self.source / "once.txt").exists() and any(
                t.get("execution", {}).get("checkpoint", {}).get("used_tool_calls") == 2
                and not t["execution"]["checkpoint"]["pending_calls"]
                and t["execution"]["checkpoint"]["provider_requests"] >= 3 for t in self.tasks()),
                "safe checkpoint not saved", 20)
        self.enabled([*self.send(self.message("fact", "FROZEN_PARENT_561", expected="背景已收到")),
                      *self.send(self.message("dispatch", "派发恢复任务", expected="后台已派发")),
                      *self.checkpoint("checkpoint"), {"touch":str(stop)},
                      {"wait_file":str(self.gate("never"))}],
                     checkpoints={"checkpoint":inspect}, stop_at=stop)
        self.hold.set()
        self.hold_after_command = False
        [task] = self.tasks()
        self.assertEqual(task["status"], "Running")
        self.assertEqual(task["spec"]["max_steps"], 2)
        self.assertEqual(task["execution"]["checkpoint"]["used_tool_calls"], 2)
        self.assertEqual(task["execution"]["checkpoint"]["provider_requests"], 3)
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        return task

    def wait_finished(self, status="Completed"):
        # 等待通知的真实桥接回执，避免仅看到业务终态就关闭仍在投递的通道；失败与中断同样会通知用户。
        def settled(task):
            notice = task.get("notice")
            if status == "Completed":
                return (notice or {}).get("status") == "Delivered"
            if notice is None or notice["status"] in ("Delivered", "Expired", "Unknown"):
                return True
            # 失败或中断的通知可能主动推送未发出而保持 Pending 等下次机会；只需等到没有进行中的投递尝试。
            attempts = notice.get("attempts") or []
            return notice["status"] == "Pending" and bool(attempts) and attempts[-1].get("finished_at_ms") is not None
        self.wait_for(lambda: any(t["status"] == status and settled(t) for t in self.tasks()),
                      "task or its notice did not settle", 45)

    def test_restart_resumes_saved_receipts_without_replaying_or_resetting_budget(self):
        before = self.seed_and_interrupt()
        self.enabled([*self.send(self.message("later", "AFTER_SNAPSHOT_874", expected="背景已收到")),
                      *self.checkpoint("resumed")], checkpoints={"resumed":self.wait_finished})
        [task] = self.tasks()
        self.assertEqual(task["id"], before["id"])
        self.assertEqual(task["spec"]["max_steps"], before["spec"]["max_steps"])
        self.assertEqual(task["usage"]["tool_calls"], 2)
        self.assertEqual(task["usage"]["model_requests"], before["execution"]["checkpoint"]["provider_requests"] + 1)
        self.assertEqual(task["execution"]["checkpoint"]["used_tool_calls"], 2)
        self.assertEqual(task["execution"]["recoveries"], 1)
        self.assertEqual(task["execution"]["deadline_at_ms"], before["execution"]["deadline_at_ms"])
        self.assertEqual(task["execution"]["completion_contract"], self.verification)
        self.assertEqual(task["execution"]["completion_contract"], before["execution"]["completion_contract"])
        self.assertEqual(task["spec"]["context"], before["spec"]["context"])
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(self.receipt("dispatch")["state"], "Sent")

    def test_transient_http_failure_recovers_in_process_from_last_receipt(self):
        self.hold_after_command = False
        self.callback = self.callback_for_task
        def status(kind, number):
            body = self.requests[-1]["body"]
            if kind == "chat" and self.is_worker(body) and len(self.results(body)) == 2 and not self.tasks()[0]["execution"]["recoveries"]:
                return 503
            return 200
        self.http_status = status
        self.enabled([*self.send(self.message("fact", "FROZEN_PARENT_561", expected="背景已收到")),
                      *self.send(self.message("dispatch", "派发恢复任务", expected="后台已派发")),
                      *self.checkpoint("resumed")], checkpoints={"resumed":self.wait_finished}, timeout=60)
        [task] = self.tasks()
        self.assertEqual(task["execution"]["recoveries"], 1)
        self.assertEqual(task["usage"]["tool_calls"], 2)
        self.assertEqual(task["execution"]["checkpoint"]["used_tool_calls"], 2)
        self.assertEqual(task["spec"]["max_steps"], 2)
        self.assertEqual(task["execution"]["completion_contract"], self.verification)
        self.assertEqual((self.source / "once.txt").read_text(), "x")

    def test_revoked_admin_is_not_allowed_to_resume_tools_or_model(self):
        before = self.seed_and_interrupt()
        config = json.loads(self.developer_config.read_text())
        config["operators"] = ["another-admin"]
        self.developer_config.write_text(json.dumps(config))
        count = len(self.requests)
        self.enabled(self.checkpoint("revoked"), checkpoints={"revoked":lambda:self.wait_finished("Failed")})
        [task] = self.tasks()
        self.assertIn("授权已失效", task["failure"])
        self.assertEqual(task["usage"]["tool_calls"], 2)
        self.assertEqual(len(self.requests), count)
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(task["spec"]["max_steps"], before["spec"]["max_steps"])

    def test_in_flight_command_is_not_replayed_after_restart(self):
        base = self.callback_for_task
        def respond(body):
            if self.is_worker(body):
                self.assertFalse(self.results(body))
                return calls(("in-flight", "run_command", self.inflight_command))
            return base(body)
        self.callback = respond
        stop = self.gate("stop-tool")
        self.enabled([*self.send(self.message("fact", "FROZEN_PARENT_561", expected="背景已收到")),
                      *self.send(self.message("dispatch", "派发恢复任务", expected="后台已派发")),
                      *self.checkpoint("in-flight"), {"touch":str(stop)}, {"wait_file":str(self.gate("never"))}],
                     checkpoints={"in-flight":lambda:self.wait_for(lambda:(self.source / "once.txt").exists(), "command did not start", 20)},
                     stop_at=stop)
        count = len(self.requests)
        self.enabled(self.checkpoint("stopped"), checkpoints={"stopped":lambda:self.wait_finished("Blocked")})
        self.assertEqual(len(self.requests), count)
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        [task] = self.tasks()
        self.assertEqual(task["status"], "Blocked")
        self.assertIn("unknown_tool_effect_not_replayed", task["result"])
        self.assertTrue(task["execution"]["checkpoint"]["pending_calls"])

    def test_repeated_provider_failure_stops_after_three_recoveries(self):
        self.callback = self.callback_for_task
        self.http_status = lambda kind, number: 503 if kind == "chat" and self.is_worker(self.requests[-1]["body"]) else 200
        self.enabled([*self.send(self.message("fact", "FROZEN_PARENT_561", expected="背景已收到")),
                      *self.send(self.message("dispatch", "派发恢复任务", expected="后台已派发")),
                      *self.checkpoint("failed")], checkpoints={"failed":lambda:self.wait_finished("Failed")}, timeout=70)
        [task] = self.tasks()
        self.assertEqual(task["execution"]["recoveries"], 3)
        self.assertFalse((self.source / "once.txt").exists())
        count = len(self.requests)
        self.enabled(self.checkpoint("still-failed"), checkpoints={"still-failed":lambda:self.wait_finished("Failed")})
        self.assertEqual(len(self.requests), count)

    def test_cancel_during_retry_backoff_prevents_further_automatic_requests(self):
        base = self.callback_for_task
        def respond(body):
            if not self.is_worker(body) and self.text(body) == "取消恢复":
                if not self.results(body):
                    return calls(("cancel", "cancel_background_task", {"task_id":self.tasks()[0]["id"]}))
                return text_response("已请求取消")
            return base(body)
        self.callback = respond
        self.http_status = lambda kind, number: 503 if kind == "chat" and self.is_worker(self.requests[-1]["body"]) else 200
        self.enabled([*self.send(self.message("fact", "FROZEN_PARENT_561", expected="背景已收到")),
                      *self.send(self.message("dispatch", "派发恢复任务", expected="后台已派发")),
                      *self.checkpoint("backoff"),
                      *self.send(self.message("cancel", "取消恢复", expected="已请求取消")),
                      *self.checkpoint("cancelled")],
                     checkpoints={"backoff":lambda:self.wait_for(lambda:any(t["execution"]["resume_at_ms"] for t in self.tasks()), "no backoff", 20),
                                  "cancelled":lambda:self.wait_finished("Cancelled")}, timeout=50)
        [task] = self.tasks()
        self.assertEqual(task["execution"]["recoveries"], 1)
        self.assertIsNone(task["notice"])
        count = len(self.requests)
        self.enabled(self.checkpoint("still-cancelled"), checkpoints={"still-cancelled":lambda:self.wait_finished("Cancelled")})
        self.assertEqual(len(self.requests), count)


if __name__ == "__main__":
    unittest.main()
