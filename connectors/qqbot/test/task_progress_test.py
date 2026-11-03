"""默认 QQ 宿主的半小时进度提醒：真实进程、持久检查点和桥接回执。

模型和 QQ 网络使用本地协议替身。只在进程停止后平移隔离任务的时间，
保持 30 分钟产品常量；默认能力、原任务、预算及回执均不通过测试参数改写。
"""
import copy
import hashlib
import json
import threading
import time
import unittest

import default_capabilities_test as default


class TaskProgressAcceptance(default.DefaultCapabilitiesAcceptance):
    for _name in dir(default.DefaultCapabilitiesAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.release_worker = threading.Event()
        self.worker_waiting = threading.Event()
        self.worker_returned = threading.Event()
        self.addCleanup(self.release_worker.set)
        self.callback = self.respond

    def respond(self, body):
        results = self.results(body)
        if default.WORKER in body["messages"][0].get("content", ""):
            if not results:
                return default.tool_response("saved-step", "echo", {"text": "已记录验收步骤"})
            self.assertEqual(results, [{"echo": "已记录验收步骤"}], "恢复不得重复已确认的工具步骤")
            self.worker_waiting.set()
            self.assertTrue(self.release_worker.wait(60), "测试没有释放模型响应门控")
            self.worker_returned.set()
            return default.text_response("验收步骤已保存，任务尚未核验完成")
        self.assert_default_tools(body)
        if self.text(body) == "再聊两句，原来的任务继续":
            self.assertEqual(len(self.tasks()), 1)
            return default.text_response("原任务保留，可以继续聊")
        if self.text(body) == "取消这个后台任务":
            if not results:
                return default.tool_response("cancel-original", "cancel_background_task", {
                    "task_id": self.tasks()[0]["id"]})
            return default.text_response("已请求停止")
        self.assertEqual(self.text(body), "后台核对步骤并保留进度")
        if not results:
            return default.tool_response("dispatch-original", "dispatch_background_task", {
                "title": "核对步骤", "instructions": "保存验收用回显，再继续核对剩余工作。",
                "max_steps": 2})
        self.assertTrue(results[-1]["dispatched"], results)
        self.assertEqual(len(self.tasks()), 1, "答复派发成功前必须已持久化")
        return default.text_response("后台任务已保存")

    def wait_checkpoint(self):
        self.wait_for(lambda: self.worker_waiting.is_set() and len(self.tasks()) == 1 and
            self.tasks()[0]["execution"]["checkpoint"]["used_tool_calls"] == 1 and
            not self.tasks()[0]["execution"]["checkpoint"]["pending_calls"],
            "已确认的真实工具检查点未保存")

    def release_response(self):
        self.release_worker.set()
        self.wait_for(self.worker_returned.is_set, "旧模型响应未退出")
        self.release_worker = threading.Event()
        self.worker_waiting.clear()
        self.worker_returned.clear()
        self.addCleanup(self.release_worker.set)

    def seed_running(self):
        self.run_default([
            *self.send("dispatch", "后台核对步骤并保留进度", "后台任务已保存"),
            *self.checkpoint("saved"),
        ], checkpoints={"saved": self.wait_checkpoint}, stop_at="saved")
        self.release_response()
        [task] = self.tasks()
        self.assertEqual(task["status"], "Running")
        self.assertEqual(task["spec"]["max_steps"], 2)
        self.assertEqual(task["execution"]["checkpoint"]["provider_requests"], 2)
        self.assertFalse(task["execution"].get("progress_notices"))
        self.assertFalse(self.pushes(), "不到半小时不能发自动进度提醒")
        self.assertEqual(self.reminders(), [], "不靠用户另建提醒器")
        return task

    def age_stopped_task(self):
        self.assertIsNone(self.child, "不能修改运行中的账本")
        snapshot = json.loads(self.state_path.read_text())
        encoded = snapshot["entries"]["eve.task"]["tasks.v1"]
        index = json.loads(bytes(encoded))
        self.assertEqual(index["format_version"], 4, "验收必须覆盖默认冷正文格式")
        [entry] = index["tasks"]
        [task] = self.tasks()  # 公共读者先核对正文摘要和目录。
        execution = task["execution"]
        self.assertIsNone(execution["deadline_at_ms"])
        self.assertIsNone(execution.get("authorization_deadline_at_ms"))
        delta = 31 * default.MINUTE
        for field in ("created_at_ms", "started_at_ms", "finished_at_ms"):
            if task[field] is not None:
                task[field] -= delta
        for notice in execution.get("progress_notices", []):
            if notice["observed_finished_at_ms"] is not None:
                notice["observed_finished_at_ms"] -= delta
            for field in ("started_at_ms", "finished_at_ms"):
                if notice["attempt"][field] is not None:
                    notice["attempt"][field] -= delta
        payload = json.dumps(task, ensure_ascii=False).encode()
        digest = hashlib.sha256(payload).hexdigest()
        archive = self.state / "task-archive" / (digest + ".json")
        self.assertFalse(archive.exists())
        archive.write_bytes(payload)
        metadata = copy.deepcopy(task)
        metadata["spec"].pop("context", None)
        metadata["result"] = metadata["failure"] = None
        metadata["execution"]["checkpoint"] = None
        metadata["execution"].pop("review_messages", None)
        entry.update(directory=metadata, record={"hash": digest, "bytes": len(payload)})
        snapshot["entries"]["eve.task"]["tasks.v1"] = list(json.dumps(index).encode())
        self.state_path.write_text(json.dumps(snapshot), encoding="utf8")
        self.assertEqual(self.tasks(), [task])
        return task

    def progress_pushes(self):
        return [push for push in self.pushes() if push["id"].startswith("taskprogress-")]

    def notices(self):
        return self.tasks()[0]["execution"].get("progress_notices", [])

    def assert_original(self, before):
        [after] = self.tasks()
        for field in ("id", "owner", "source", "spec", "created_at_ms", "started_at_ms"):
            self.assertEqual(after[field], before[field], field)
        for field in ("completion_contract", "review_messages", "deadline_at_ms", "authorization_deadline_at_ms"):
            self.assertEqual(after["execution"].get(field), before["execution"].get(field), field)
        self.assertEqual(after["execution"]["checkpoint"]["used_tool_calls"], 1)
        self.assertFalse(after["execution"]["checkpoint"]["pending_calls"])
        return after

    def observe_without_more_pushes(self, count, expected_result):
        deadline = time.monotonic() + 2.2  # 跨过主动投递的真实扫描周期。
        while time.monotonic() < deadline:
            self.assertIsNone(self.child.poll())
            self.assertEqual(len(self.progress_pushes()), count)
            [notice] = self.notices()
            self.assertEqual(notice["attempt"]["result"], expected_result)
            time.sleep(0.05)

    def test_due_progress_survives_restart_and_new_chat_without_resetting_task(self):
        self.seed_running()
        before = self.age_stopped_task()
        push_id = "taskprogress-" + before["id"] + "-1"
        sent = {"sent": {"platform_message_id": "push-" + push_id}}

        def delivered():
            self.wait_checkpoint()
            self.wait_for(lambda: self.notices() and self.notices()[0]["attempt"]["result"] == sent,
                          "默认半小时进度提醒未取得实际桥接回执")

        self.run_default([
            *self.checkpoint("delivered"),
            *self.send("new-chat", "再聊两句，原来的任务继续", "原任务保留，可以继续聊"),
            *self.checkpoint("new-chat-saved"),
        ], checkpoints={"delivered": delivered,
                       "new-chat-saved": lambda: self.assert_original(before)}, stop_at="new-chat-saved")
        self.release_response()
        [push] = self.progress_pushes()
        self.assertEqual((push["id"], push["scope"], push["target_id"]), (push_id, "c2c", "user-1"))
        self.assertNotIn("msg_id", push)
        self.assertIn("已记账工具步骤 1/2", push["text"])
        self.assertRegex(push["text"],
            r"等待自动恢复|等待调度接续；尚无新的执行确认|当前进程是否仍在执行尚无独立确认",
            "恢复退避和已接收执行均须如实表达，持久 Running 不能冒充持续执行证据")
        [notice] = self.notices()
        self.assertGreaterEqual(notice["attempt"]["started_at_ms"] - before["created_at_ms"], 30 * default.MINUTE)
        self.assertEqual(notice["status"], "Running")
        self.assertEqual(self.assert_original(before)["execution"]["recoveries"], 1)

        def unchanged():
            self.wait_checkpoint()
            self.observe_without_more_pushes(1, sent)
            self.assertEqual(self.assert_original(before)["execution"]["recoveries"], 2)

        self.run_default(self.checkpoint("unchanged"), checkpoints={"unchanged": unchanged},
                         stop_at="unchanged")
        self.release_response()
        before_next = self.age_stopped_task()
        previous_notice = copy.deepcopy(self.notices()[0])
        second_id = "taskprogress-" + before["id"] + "-2"
        second_sent = {"sent": {"platform_message_id": "push-" + second_id}}

        def next_interval():
            self.wait_checkpoint()
            self.wait_for(lambda: len(self.notices()) == 2 and
                self.notices()[1]["attempt"]["result"] == second_sent,
                "下一半小时间隔未继续同一任务的提醒序号")
            self.assertEqual(self.notices()[0], previous_notice)
            self.assertGreaterEqual(self.notices()[1]["attempt"]["started_at_ms"] -
                previous_notice["attempt"]["started_at_ms"], 30 * default.MINUTE)
            self.assertEqual(self.assert_original(before_next)["execution"]["recoveries"], 3)

        self.run_default([
            *self.checkpoint("next-interval"),
            *self.send("cancel", "取消这个后台任务", "已请求停止"),
            *self.checkpoint("cancelled"),
        ], checkpoints={"next-interval": next_interval, "cancelled": lambda: self.wait_for(
            lambda: self.tasks()[0]["status"] == "Cancelled", "取消未作用于原后台任务")})
        self.release_response()
        self.assertEqual([push["id"] for push in self.progress_pushes()], [push_id, second_id])
        self.assertEqual(self.tasks()[0]["usage"]["tool_calls"], 1)

    def test_unknown_progress_delivery_is_not_replayed_after_another_interval(self):
        self.seed_running()
        self.age_stopped_task()

        def written_without_ack():
            self.wait_checkpoint()
            self.wait_for(lambda: len(self.progress_pushes()) == 1, "主动发送未写到桥接")
            [notice] = self.notices()
            self.assertIsNone(notice["attempt"]["result"], "替身持有回执时不能提前确认送达")

        self.run_default([{"push_mode": {"hold": True}}, *self.checkpoint("written")],
            checkpoints={"written": written_without_ack}, stop_at="written")
        self.release_response()
        before = self.age_stopped_task()

        def unknown():
            self.wait_checkpoint()
            self.wait_for(lambda: self.notices()[0]["attempt"]["result"] == "unknown",
                          "重启没有保留未知投递结局")
            self.observe_without_more_pushes(1, "unknown")
            self.assert_original(before)

        self.run_default(self.checkpoint("unknown"), checkpoints={"unknown": unknown}, stop_at="unknown")
        self.release_response()
        self.assertEqual(len(self.progress_pushes()), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
