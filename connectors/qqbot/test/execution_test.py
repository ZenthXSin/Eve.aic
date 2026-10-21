"""真实宿主验收：短暂等待、提前收尾自动续行、平台管理员绑定与密钥脱敏。"""
import json
import unittest
import reminder_test
from background_task_test import calls
from dialogue_tools_test import text_response


class ExecutionAcceptance(reminder_test.ReminderAcceptance):
    for _name in dir(reminder_test.ReminderAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def run_eve(self, script, **options):
        return reminder_test.base().run_eve(self, script, interest=False, research=False,
                                           practice=False, more=["--sleep"], **options)

    def test_pending_task_continues_after_early_final_and_sleep_without_new_user_input(self):
        early = False
        def respond(body):
            nonlocal early
            results = self.results(body)
            if not results:
                return calls(("begin", "task_progress", {"status": "in_progress", "remaining": "等待后核对输出并完成"}))
            if len(results) == 1 and not early:
                early = True
                return text_response("接下来我会等待并核对。")
            if len(results) == 1:
                self.assertTrue(any("本轮任务仍为 in_progress" in (m.get("content") or "") for m in body["messages"]))
                return calls(("wait", "sleep", {"seconds": 1}))
            if len(results) == 2:
                self.assertGreaterEqual(results[-1]["elapsed_ms"], 1000)
                return calls(("verify", "echo", {"text": "核对成功"}))
            if len(results) == 3:
                return calls(("done", "task_progress", {"status": "completed", "remaining": "已等待并核对"}))
            return text_response("已完成并核对。")
        self.callback = respond
        self.run_eve([*self.send(self.message("work", "请执行并核对", expected="已完成并核对。"))])
        turns = [t for s in self.documents()["eve.session"]["sessions.v1"]["sessions"].values() for t in s["turns"]]
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["status"]["state"], "Completed")
        self.assertEqual(sum(m["role"] == "User" for m in turns[0]["status"]["messages"]), 1)


if __name__ == "__main__":
    unittest.main()
