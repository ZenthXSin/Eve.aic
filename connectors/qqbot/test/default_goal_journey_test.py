"""默认 QQ 入口的连续代码修复目标；实际文件、命令、审核、提醒及恢复。

模型与平台为本地协议替身，用于确定性验证宿主。用错误的完成声明触发原合同修复，
不把模型文字或单独文件写入当作交付；不涉及生产目录或外部平台。
"""
import hashlib
import json
import os
import sys
import threading
import time
import unittest

import default_capabilities_test as default
import task_progress_test as progress


@unittest.skipUnless(os.name == "posix", "此连续进程验收使用 POSIX Python 可执行链接")
class DefaultGoalJourneyAcceptance(progress.TaskProgressAcceptance):
    for _name in dir(progress.TaskProgressAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        (self.empty_path / "python3").symlink_to(sys.executable)
        self.project = self.work / "project"
        self.project.mkdir()
        self.before = "def departure_minutes(arrival, travel, preparation):\n    return arrival - travel + preparation\n"
        self.after = "def departure_minutes(arrival, travel, preparation):\n    return arrival - travel - preparation\n"
        (self.project / "schedule.py").write_text(self.before, encoding="utf8")
        self.verifier = (
            "import json, pathlib, runpy, unittest\n"
            "class ScheduleTests(unittest.TestCase):\n"
            "    def test_departure(self):\n"
            "        departure = runpy.run_path('schedule.py')['departure_minutes']\n"
            "        checks = [(515, 40, 15, 460), (600, 0, 0, 600), (15, 20, 10, -15)]\n"
            "        passed = all(departure(a, t, p) == expected for a, t, p, expected in checks)\n"
            "        with pathlib.Path('attempts.jsonl').open('a', encoding='utf8') as output:\n"
            "            output.write(json.dumps({'passed': passed, 'cases': len(checks)}) + '\\n')\n"
            "        for a, t, p, expected in checks:\n"
            "            with self.subTest(arrival=a, travel=t, preparation=p):\n"
            "                self.assertEqual(departure(a, t, p), expected)\n"
        )
        (self.project / "test_schedule.py").write_text(self.verifier, encoding="utf8")
        self.command = {"program": "python3", "args": ["-I", "-m", "unittest", "discover", "-v"], "cwd": "project"}
        self.expected_hash = hashlib.sha256(self.after.encode()).hexdigest()
        self.contract = {"kind": "receipts", "contract": {"checks": [
            {"tool": "run_command", "arguments": self.command,
             "output_pointer": "/exit_code", "expected": 0},
            {"tool": "read_file", "arguments": {"path": "project/schedule.py"},
             "output_pointer": "/sha256", "expected": self.expected_hash}], "require_final_batch": True}}
        self.originals = [
            "这是一个独立的时间计算小项目。",
            "到达时间、路上耗时和准备耗时都以分钟计。",
            "不要偷偷增加缓冲时间，也不要改变测试条件。",
            "允许在临时 project 目录中修改 schedule.py 并运行 Python unittest，记录测试结果。",
            "这一条要完整保留：" + "计算必须沿用原条件。" * 150 + "授权原话尾部_674",
            "在后台修复出发时间计算，先复现失败，再修复并通过现有三组测试；保留原目标继续做完。",
        ]
        self.repair_waiting = threading.Event()
        self.edit_waiting = threading.Event()
        self.release_edit = threading.Event()
        self.addCleanup(self.release_edit.set)
        self.reviews = []
        self.callback = self.respond_journey

    def all_results(self, body):
        return [json.loads(message["content"]) for message in body["messages"] if message["role"] == "tool"]

    def respond_journey(self, body):
        if self.definitions(body) == {"submit_review"}:
            request = json.loads(body["messages"][-1]["content"])
            self.assertTrue(request["delegated"])
            self.assertEqual(request["user_messages"], self.originals)
            self.assertEqual(request["workspace"], str(self.work.resolve()))
            self.assertEqual(request["program"], self.command["program"])
            self.assertEqual(request["arguments"], self.command["args"])
            self.assertEqual(request["cwd"], "project")
            self.reviews.append(request)
            return default.tool_response("review-" + str(len(self.reviews)), "submit_review", {
                "risk": "low", "authorization": "high", "outcome": "allow",
                "rationale": "原用户明确授权在隔离测试项目中运行固定行为检查，完整参数和原话已核对"})
        if default.WORKER not in body["messages"][0].get("content", ""):
            self.assert_default_tools(body)
            if self.text(body) != self.originals[-1]:
                return self.say("原任务和条件保留")
            if not self.results(body):
                return default.tool_response("journey-dispatch", "dispatch_background_task", {
                    "title": "修复出发时间", "instructions": self.originals[-1],
                    "max_steps": 5, "include_context": True, "verification": self.contract})
            self.assertTrue(self.results(body)[-1].get("dispatched"), self.results(body))
            return self.say("后台目标已保存")
        results = self.all_results(body)
        if not results:
            return default.tool_response("baseline", "run_command", self.command)
        self.assertEqual(results[0]["exit_code"], 1, "必须先真实复现缺陷")
        self.assertFalse(results[0]["success"])
        [task] = self.tasks()
        if task["execution"]["recoveries"] == 0:
            return default.text_response("模型声称已经修好并通过所有测试")
        self.assertEqual(task["status"], "Running")
        self.assertIsNone(task["notice"], "验收失配后不能提前发送完成通知")
        if len(results) == 1:
            if self.runs == 1:
                self.repair_waiting.set()
                self.assertTrue(self.release_worker.wait(60), "恢复模型门控未释放")
                self.worker_returned.set()
            return default.tool_response("inspect-source", "read_file", {"path": "project/schedule.py"})
        if len(results) == 2:
            self.assertEqual(results[-1]["content"], self.before)
            return default.tool_response("repair-source", "edit_file", {
                "path": "project/schedule.py", "old_string": "arrival - travel + preparation",
                "new_string": "arrival - travel - preparation", "expected_sha256": results[-1]["sha256"]})
        if len(results) == 3:
            self.assertTrue(results[-1]["success"], results[-1])
            self.edit_waiting.set()
            self.assertTrue(self.release_edit.wait(60), "编辑后的模型门控未释放")
            # 同一个最终批次覆盖行为和实际代码；检查之后不再写入产物。
            reply = default.tool_response("verify-behavior", "run_command", self.command)
            reply["choices"][0]["message"]["tool_calls"].extend(
                default.tool_response("verify-source", "read_file", {"path": "project/schedule.py"})
                ["choices"][0]["message"]["tool_calls"])
            return reply
        self.assertEqual(len(results), 5)
        self.assertEqual(results[-2]["exit_code"], 0)
        self.assertIn("OK", results[-2]["stderr"])
        self.assertEqual(results[-1]["sha256"], self.expected_hash)
        self.assertFalse(body.get("tools"), "五步上限耗尽后不能添加工具")
        return default.text_response("出发时间已修复，原有三组行为检查全部通过")

    def test_failure_repair_progress_new_chat_and_restart_finish_original_default_goal(self):
        def repair_started():
            self.wait_for(lambda: self.repair_waiting.is_set() and self.tasks()[0]["execution"]["recoveries"] == 1,
                          "错误完成声明未触发原合同的自动修复")
            self.assertEqual(self.tasks()[0]["status"], "Running")
            self.assertEqual(self.tasks()[0]["execution"]["checkpoint"]["used_tool_calls"], 1)
            self.assertFalse(self.pushes(), "行为验收失败不能发送完成通知")

        script = [step for index, text in enumerate(self.originals) for step in self.send(
            f"original-{index}", text, "后台目标已保存" if index == 5 else "原任务和条件保留")]
        self.run_default([*script, *self.checkpoint("repair-started")],
                         checkpoints={"repair-started": repair_started}, stop_at="repair-started")
        self.release_response()
        before = self.age_stopped_task()
        self.assertEqual(before["execution"]["review_messages"], self.originals)
        self.assertEqual(before["execution"]["completion_contract"], self.contract)
        self.assertEqual((self.project / "schedule.py").read_text(), self.before)

        def edited_and_notified():
            self.wait_for(lambda: self.edit_waiting.is_set() and self.notices() and
                isinstance(self.notices()[0]["attempt"]["result"], dict),
                "重启后实际修复或默认半小时提醒未发生")
            self.assertEqual(self.tasks()[0]["status"], "Running")
            self.assertEqual(self.tasks()[0]["execution"]["checkpoint"]["used_tool_calls"], 3)

        def original_kept():
            [task] = self.tasks()
            for field in ("id", "owner", "source", "spec"):
                self.assertEqual(task[field], before[field], field)
            self.assertEqual(task["execution"]["review_messages"], self.originals)
            self.release_edit.set()

        def completed():
            self.wait_for(lambda: self.tasks()[0]["status"] == "Completed" and
                self.tasks()[0]["notice"]["status"] == "Delivered", "原目标或最终投递未完成")

        self.run_default([
            *self.checkpoint("edited"),
            *self.send("new-chat", "我们可以继续聊，原来的修复别取消", "原任务和条件保留"),
            *self.checkpoint("original-kept"), *self.checkpoint("completed"),
        ], checkpoints={"edited": edited_and_notified, "original-kept": original_kept, "completed": completed})
        [task] = self.tasks()
        self.assertEqual(task["id"], before["id"])
        self.assertEqual(task["execution"]["completion_contract"], self.contract)
        self.assertEqual(task["execution"]["review_messages"], self.originals)
        self.assertEqual(task["execution"]["recoveries"], 2)
        self.assertEqual(task["usage"], {"tool_calls": 5, "model_requests": 7})
        self.assertEqual(task["spec"]["max_steps"], 5)
        self.assertEqual((self.project / "schedule.py").read_text(), self.after)
        self.assertEqual((self.project / "test_schedule.py").read_text(), self.verifier)
        attempts = [json.loads(line) for line in (self.project / "attempts.jsonl").read_text().splitlines()]
        self.assertEqual(attempts, [{"passed": False, "cases": 3}, {"passed": True, "cases": 3}])
        self.assertEqual(len(self.reviews), 2)
        self.assertEqual([push["id"] for push in self.pushes()], ["taskprogress-" + task["id"] + "-1",
            f'{task["id"]}:finish-{task["finished_at_ms"]}:attempt-1'])
        journal = self.state / "developer-tools" / "operations.jsonl"
        saved_journal = journal.read_bytes()
        saved_attempts = (self.project / "attempts.jsonl").read_bytes()

        def no_replay():
            deadline = time.monotonic() + 2.2
            while time.monotonic() < deadline:
                self.assertIsNone(self.child.poll())
                self.assertEqual(self.tasks(), [task])
                self.assertEqual(len(self.pushes()), 2)
                time.sleep(0.05)

        self.run_default(self.checkpoint("no-replay"), checkpoints={"no-replay": no_replay})
        self.assertEqual(journal.read_bytes(), saved_journal)
        self.assertEqual((self.project / "attempts.jsonl").read_bytes(), saved_attempts)
        self.assertEqual(len(self.reviews), 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
