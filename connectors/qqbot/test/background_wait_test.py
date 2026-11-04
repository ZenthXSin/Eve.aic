"""真实 QQ 后台技术等待：宿主绑定当前任务，保存后重启并由真实文件条件接续。"""
import json
import unittest

import background_completion_test as completion
from background_task_test import calls
from dialogue_tools_test import text_response


class BackgroundWaitAcceptance(completion.BackgroundCompletionAcceptance):
    for _name in dir(completion.BackgroundCompletionAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def triggers(self):
        return self.documents().get("eve.trigger", {}).get("triggers.v1", {}).get("triggers", [])

    def enabled(self, script, **options):
        return super().enabled(script, more=["--triggers"], **options)

    def configure_wait(self, user_decision=False):
        expected = "真实文件条件已满足"
        wait_path = getattr(self, "wait_path", "wait-result.txt")
        verification = self.file_contract(wait_path, expected)

        def respond(body):
            if not self.is_worker(body):
                self.assertNotIn("register_task_wait", self.definitions(body))
                return self.foreground(body, verification, steps=6)
            self.assertIn("register_task_wait", self.definitions(body))
            self.assertIn("task_progress", self.definitions(body))
            self.assertNotIn("set_wakeup_trigger", self.definitions(body))
            results = self.worker_results(body)
            if not results:
                return calls(("wait-register", "register_task_wait", {
                    "condition": {"type": "file_changed", "path": wait_path},
                    "reason": "等待构建文件生成后读取并核对结果",
                }))
            self.assertTrue(results[0].get("registered"), results)
            if len(results) == 1:
                return calls(("wait-blocked", "task_progress", {
                    "status": "blocked",
                    "remaining": "需要主人决定是否继续发布" if user_decision else results[0]["remaining"],
                }))
            self.assertEqual(results[1].get("status"), "blocked", results[1])
            host_resume = any("blocked_task_resumed" in message.get("content", "")
                              for message in body["messages"] if message["role"] == "user")
            if not host_resume:
                return text_response("已经登记文件等待，任务状态已保存。")
            self.assertFalse(user_decision, "用户待决定事项不能随文件变化自动解除")
            if len(results) == 2:
                return calls(("wait-read", "read_file", {"path": wait_path}))
            if len(results) == 3:
                self.assertEqual(results[-1]["sha256"], verification["contract"]["checks"][0]["expected"])
                return calls(("wait-complete", "task_progress", {
                    "status": "completed", "remaining": "已经读取文件并核对真实摘要，再按固定合同作最后复验",
                }))
            if len(results) == 4:
                return calls(("wait-verify", "read_file", {"path": wait_path}))
            self.assertEqual(results[-1]["sha256"], verification["contract"]["checks"][0]["expected"])
            return text_response("已读取真实文件并通过原验收。")

        self.callback = respond
        return expected, verification

    def wait_blocked(self):
        self.wait_finished("Blocked")
        self.wait_for(lambda: len(self.triggers()) == 1, "技术条件未持久化", 20)
        [trigger] = self.triggers()
        self.assertEqual(trigger["status"], "Armed")
        [task] = self.tasks()
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertEqual(task["usage"]["tool_calls"], 2)
        self.assertEqual(trigger["spec"]["resume_task_id"], task["id"])

    def seed_wait(self, user_decision=False):
        expected, verification = self.configure_wait(user_decision)
        self.enabled([
            *self.send(self.message("dispatch-wait", "派发完成验收", expected="后台已派发")),
            *self.checkpoint("blocked"),
        ], checkpoints={"blocked": self.wait_blocked}, timeout=60)
        return expected, verification, self.tasks()[0], self.triggers()[0]

    def test_file_wait_survives_process_restart_and_finishes_same_task_with_original_contract(self):
        expected, verification, before, trigger = self.seed_wait()
        before_requests = len(self.worker_requests())
        (self.source / "wait-result.txt").write_text(expected)

        def completed():
            self.wait_finished("Completed")
            self.wait_for(lambda: self.triggers()[0]["status"] == "Completed",
                          "原任务接续未记录成功", 20)

        # 第二个真实进程不发送任何用户消息；文件变化本身使原任务从持久检查点继续。
        self.enabled(self.checkpoint("completed"), checkpoints={"completed": completed}, timeout=60)
        [after] = self.tasks()
        self.assertEqual(after["id"], before["id"])
        self.assertEqual(after["spec"], before["spec"])
        self.assertEqual(after["usage"]["tool_calls"], 5)
        self.assertEqual(after["execution"]["recoveries"], 1)
        self.assertEqual(after["execution"]["completion_contract"], verification)
        for field in ("completion_contract", "review_messages", "deadline_at_ms", "authorization_deadline_at_ms"):
            self.assertEqual(after["execution"].get(field), before["execution"].get(field), field)
        self.assertEqual(self.triggers()[0]["id"], trigger["id"])
        self.assertEqual(self.triggers()[0]["runs"], 1)
        self.assertEqual(len(self.worker_requests()) - before_requests, 4)
        self.assertEqual((self.source / "wait-result.txt").read_text(), expected)

    def test_external_file_wait_restarts_and_finishes_with_host_filesystem_enabled(self):
        outside = self.work / "outside-project"
        outside.mkdir()
        self.wait_path = str(outside / "result.txt")
        config = json.loads(self.developer_config.read_text())
        config["host_filesystem"] = True
        self.developer_config.write_text(json.dumps(config))
        expected, verification, before, trigger = self.seed_wait()
        (outside / "result.txt").write_text(expected)
        self.enabled(self.checkpoint("external-completed"), checkpoints={
            "external-completed": lambda: self.wait_finished("Completed")}, timeout=60)
        [after] = self.tasks()
        self.assertEqual(after["id"], before["id"])
        self.assertEqual(after["execution"]["completion_contract"], verification)
        self.assertEqual(after["execution"]["recoveries"], 1)
        self.assertEqual(self.triggers()[0]["id"], trigger["id"])
        self.assertEqual(self.triggers()[0]["spec"]["conditions"][0]["path"], self.wait_path)
        self.assertFalse((self.source / "result.txt").exists())

    def test_later_user_decision_is_not_released_by_a_registered_technical_condition(self):
        expected, _, before, _ = self.seed_wait(user_decision=True)
        before_requests = len(self.worker_requests())
        (self.source / "wait-result.txt").write_text(expected)
        self.enabled(self.checkpoint("rejected"), checkpoints={"rejected": lambda: self.wait_for(
            lambda: self.triggers()[0]["status"] == "Failed", "非技术阻塞被错误恢复", 20)}, timeout=40)
        self.assertEqual(self.tasks(), [before])
        self.assertEqual(len(self.worker_requests()), before_requests)
        failure = json.loads(self.triggers()[0]["result"])
        self.assertEqual(failure["stage"], "technical_wait")
        self.assertEqual(failure["execution"], "not_started")
        self.assertFalse(failure["replay_safe"])


if __name__ == "__main__":
    unittest.main()
