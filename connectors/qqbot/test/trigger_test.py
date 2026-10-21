"""真实 Rust QQ 宿主验收：条件满足后在原会话自动执行，不伪造用户输入或训练样本。"""
import json
import hashlib
import subprocess
import threading
import unittest
import reminder_test
from dialogue_tools_test import text_response
from background_task_test import calls

PREFIX = "【宿主条件触发事件；不是用户新消息】"


class TriggerAcceptance(reminder_test.ReminderAcceptance):
    for _name in dir(reminder_test.ReminderAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def run_eve(self, script, developer=True, tasks=False, **options):
        if developer:
            config = self.work / "developer.json"
            group="qq:"+hashlib.sha256(json.dumps(["1904159860","group","group-1","user-1"],separators=(",",":")).encode()).hexdigest()
            config.write_text(json.dumps({"workspace": str(self.work), "operators": [reminder_test.identity("user-1"),group], "commands": True}))
            options["env_extra"] = {"EVE_DEVELOPER_CONFIG": str(config)}
        flags = ["--triggers", "--max-tool-rounds", "5"] + (["--background-tasks"] if tasks else [])
        return reminder_test.base().run_eve(self, script, interest=False, research=False,
                                           practice=False, more=flags, **options)

    def triggers(self):
        return (self.documents().get("eve.trigger", {}).get("triggers.v1") or {}).get("triggers", [])

    def wait_sent(self):
        self.wait_for(lambda: any(t.get("delivery") == "Sent" for t in self.triggers()),
                      "wake result not delivered", 15)

    def configure(self, condition):
        def respond(body):
            text = self.text(body)
            if text.startswith(PREFIX):
                # It is the original persistent session, not a context-free worker.
                self.assertTrue(any(m.get("content") == "设置唤醒" for m in body["messages"]))
                self.assertTrue(any(m.get("content") == "已设置" for m in body["messages"]))
                return text_response("唤醒后已处理")
            if text == "设置唤醒":
                if not self.results(body):
                    return calls(("create", "set_wakeup_trigger", {
                        "title": "等条件完成", "instructions": "核对结果后继续", "conditions": [condition]}))
                self.assertTrue(self.results(body)[0]["created"])
                return text_response("已设置")
            return text_response("收到")
        self.callback = respond

    def test_timer_wakes_original_session_and_restart_does_not_repeat(self):
        self.configure({"type": "time", "after_seconds": 1})
        self.run_eve([*self.send(self.message("create", "设置唤醒", expected="已设置")),
                      *self.checkpoint("sent")], checkpoints={"sent": self.wait_sent})
        [t] = self.triggers()
        self.assertEqual((t["status"], t["delivery"]), ("Completed", "Sent"))
        [push] = self.pushes()
        self.assertEqual((push["scope"], push["target_id"], push["text"]),
                         ("c2c", "user-1", "唤醒后已处理"))
        before = len([r for r in self.kind("chat") if self.text(r["body"]).startswith(PREFIX)])
        self.run_eve([*self.send(self.message("back", "继续", expected="收到")),
                      *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        self.assertEqual(len([r for r in self.kind("chat") if self.text(r["body"]).startswith(PREFIX)]), before)
        self.assertEqual(len(self.pushes()), 1)

    def test_group_message_match_is_scoped_and_result_returns_to_original_group(self):
        self.configure({"type": "message_contains", "text": "构建完成"})
        route = {"scope": "group", "target": "group-1"}
        self.run_eve([
            *self.send(self.message("create", "设置唤醒", expected="已设置", **route)),
            *self.send(self.message("other", "构建完成", user="user-2", expected="收到", **route)),
            *self.checkpoint("isolated"),
            *self.send(self.message("match", "构建完成", expected="收到", **route)),
            *self.checkpoint("sent")], checkpoints={
                "isolated": lambda: self.assertEqual(self.triggers()[0]["status"], "Armed"),
                "sent": self.wait_sent})
        [push] = self.pushes()
        self.assertEqual((push["scope"], push["target_id"]), ("group", "group-1"))

    def test_file_change_wakes_after_real_file_write(self):
        self.configure({"type": "file_changed", "path": "output.txt"})
        def change():
            self.assertEqual(self.triggers()[0]["status"], "Armed")
            (self.work / "output.txt").write_text("finished")
            self.wait_sent()
        self.run_eve([*self.send(self.message("create", "设置唤醒", expected="已设置")),
                      *self.checkpoint("changed")], developer=True, checkpoints={"changed": change})
        self.assertEqual(self.triggers()[0]["delivery"], "Sent")

    @unittest.skipUnless(__import__("sys").platform.startswith("linux"), "Linux process observation")
    def test_process_exit_wakes_without_running_supplied_commands(self):
        child = subprocess.Popen(["sleep", "30"])
        try:
            self.configure({"type": "process_state", "pid": child.pid, "running": False})
            def stop_process():
                self.assertEqual(self.triggers()[0]["status"], "Armed")
                child.terminate()
                child.wait(timeout=5)
                self.wait_sent()
            self.run_eve([*self.send(self.message("create", "设置唤醒", expected="已设置", user="user-2")),
                          *self.checkpoint("exit")], developer=True, checkpoints={"exit": stop_process})
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()

    def test_background_task_completion_wakes_same_conversation(self):
        def respond(body):
            text = self.text(body)
            if "你在为 Eve 独立完成一项委托的任务" in body["messages"][0]["content"]:
                return text_response("任务已完成")
            if text.startswith(PREFIX):
                return text_response("根据后台结果继续完成")
            results = self.results(body)
            if not results:
                return calls(("dispatch", "dispatch_background_task", {"title": "核对", "instructions": "核对输入", "max_steps": 1}))
            if len(results) == 1:
                return calls(("register", "set_wakeup_trigger", {"title": "任务完成后继续", "instructions": "根据后台结果继续",
                    "conditions": [{"type": "task_finished", "task_id": results[0]["task"]["id"]}]}))
            return text_response("已设置")
        self.callback = respond
        self.run_eve([*self.send(self.message("create", "设置唤醒", expected="已设置")),
                      *self.checkpoint("sent")], tasks=True, checkpoints={"sent": self.wait_sent})
        self.assertEqual(self.triggers()[0]["delivery"], "Sent")

    def test_incoming_message_queues_while_wakeup_is_running(self):
        self.configure({"type": "time", "after_seconds": 1})
        original = self.callback
        started = threading.Event()
        release = threading.Event()
        def respond(body):
            if self.text(body).startswith(PREFIX):
                started.set()
                self.assertTrue(release.wait(15), "wake completion gate not released")
            return original(body)
        self.callback = respond
        def running():
            self.assertTrue(started.wait(10), "wake did not start")
        try:
            self.run_eve([*self.send(self.message("create", "设置唤醒", expected="已设置")),
                *self.checkpoint("running"),
                {"send": self.message("queued", "继续处理这条新消息", expected="收到")},
                *self.checkpoint("release"),
                self.wait_reply("queued"), self.wait_receipt("queued"),
                *self.checkpoint("sent")], checkpoints={"running": running, "release": release.set, "sent": self.wait_sent})
        finally:
            release.set()
        self.assertEqual(self.receipt("queued")["state"], "Sent")

    def test_cancelled_trigger_cannot_fire_after_restart(self):
        self.configure({"type": "time", "after_seconds": 600})
        original = self.callback
        def respond(body):
            if self.text(body) != "取消":
                return original(body)
            if not self.results(body):
                return calls(("cancel", "cancel_wakeup_trigger", {"trigger_id": self.triggers()[0]["id"]}))
            self.assertTrue(self.results(body)[0]["cancelled"])
            return text_response("已取消")
        self.callback = respond
        self.run_eve([*self.send(self.message("create", "设置唤醒", expected="已设置")),
                      *self.send(self.message("cancel", "取消", expected="已取消"))])
        self.assertEqual(self.triggers()[0]["status"], "Cancelled")
        self.assertEqual(self.pushes(), [])


if __name__ == "__main__":
    unittest.main()
