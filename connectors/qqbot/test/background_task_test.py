"""实际 eve-qqbot 的后台任务与子代理验收；只连接本地模型替身与 fake bridge。

用户在对话里请 Eve 去做一件需要多步的事：聊天模型调用 dispatch_background_task 派发，账本保存后才回复；
后台执行器看不到对话，只按派发说明调用工具，步数（工具调用次数）用尽后不再提供工具、由模型收尾；结束后
私聊通知派发它的用户。用户可以让 Eve 查看或取消；重启时没有检查点的旧任务记为中断，不自动重跑；新任务的自动接续由 background_recovery_test 覆盖。子代理在同一轮里
做完一件子任务并把结果交回。

模型替身按请求里的工具定义与上下文确定性地作答，只证明宿主契约，不代表真实模型的理解能力。
"""
import json
import os
import subprocess
import threading
import unittest

import reminder_test
from dialogue_tools_test import PLATFORM_TOOLS, text_response
from memory_test import BINARY, ROOT

WORKER = "你在为 Eve 独立完成一项委托的任务"
TASKS = '{"kind":"eve.background_tasks"'
ORCHESTRATION = {"dispatch_background_task", "list_background_tasks", "cancel_background_task", "run_subagent"}
CHAT_TOOLS = PLATFORM_TOOLS | {"echo"} | ORCHESTRATION | {"create_subagent","list_subagents","delete_subagent"}
ASK = "帮我在后台把日志整理一下"
DISPATCHED = "好，已经在后台处理，做完私聊你。"
INSTRUCTIONS = "读取三份日志，统计错误条数，给出结论。"
RESULT = "日志整理完毕：三份日志共 3 条错误。"
STATUS = "刚才那个任务怎么样了"
HOLD_SECONDS = 20


def calls(*entries):
    """带唯一调用 ID 的工具调用；同一轮的不同批次不能复用调用 ID。"""
    return {"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
        "role": "assistant", "content": None, "tool_calls": [
            {"id": call_id, "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}
            for call_id, name, arguments in entries]}}]}


class BackgroundTaskAcceptance(reminder_test.ReminderAcceptance):
    for _name in dir(reminder_test.ReminderAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.release = threading.Event()

    def tearDown(self):
        self.release.set()
        super().tearDown()

    def run_eve(self, script, tasks=True, subagents=True, **options):
        self.worker_subagents = subagents
        more = ["--max-tool-rounds", "4"] + (["--background-tasks"] if tasks else []) + (["--subagents"] if subagents else [])
        return reminder_test.base().run_eve(self, script, interest=False, research=False, practice=False,
                                            more=more, **options)

    def is_worker(self, body):
        return WORKER in body["messages"][0]["content"]

    def worker_requests(self):
        return [r["body"] for r in self.kind("chat") if self.is_worker(r["body"])]

    def context(self, body):
        found = [m["content"] for m in body["messages"] if m["role"] == "system" and TASKS in m["content"]]
        self.assertLessEqual(len(found), 1)
        return json.loads(found[0][found[0].index(TASKS):]) if found else None

    def ledger(self):
        return self.documents().get("eve.task", {}).get("tasks.v1") or {"format_version": 1, "tasks": []}

    def tasks(self):
        return self.ledger()["tasks"]

    def wait_task(self, predicate, message, seconds=20):
        self.wait_for(lambda: any(predicate(task) for task in self.tasks()), message, seconds)

    def dispatch(self, max_steps=3, text=ASK, reply=DISPATCHED, **route):
        """聊天模型派发一个任务；回复前账本已经保存。"""
        def respond(body):
            if self.is_worker(body):
                return self.worker(body)
            if self.text(body) != text:
                return self.chat(body)
            self.assertEqual(self.definitions(body), CHAT_TOOLS)
            results = self.results(body)
            if not results:
                return calls(("dispatch", "dispatch_background_task",
                              {"title": "整理日志", "instructions": INSTRUCTIONS, "max_steps": max_steps}))
            [result] = results
            self.assertTrue(result["dispatched"], result)
            self.assertEqual(result["task"]["max_steps"], max_steps)
            return text_response(reply)
        return respond

    def worker(self, body):
        """执行器替身：每步调用一次 echo，直到宿主不再提供工具时收尾。"""
        self.assertEqual(self.text(body), "任务：整理日志\n\n" + INSTRUCTIONS)
        roles = [m["role"] for m in body["messages"]]
        self.assertEqual(roles.count("user"), 1, "执行器看不到对话")
        if not body.get("tools"):
            return text_response(RESULT)
        self.assertEqual(self.definitions(body), PLATFORM_TOOLS | {"echo"} | ({"run_subagent"} if self.worker_subagents else set()), "后台执行者可以委派叶子子代理，但不能再派发后台任务")
        done = len(self.results(body))
        return calls((f"step-{done}", "echo", {"text": f"第 {done + 1} 步"}))

    def test_a_dispatched_task_runs_within_its_step_budget_and_notifies_the_owner(self):
        self.callback = self.dispatch(max_steps=3)
        self.run_eve([*self.send(self.message("ask", ASK, expected=DISPATCHED)), *self.checkpoint("delivered"),
                      *self.checkpoint("quiet")],
                     checkpoints={"delivered": lambda: self.wait_task(
                         lambda t: (t.get("notice") or {}).get("status") == "Delivered", "notice was not delivered"),
                         "quiet": self.quiet})
        [task] = self.tasks()
        self.assertEqual((task["status"], task["spec"]["title"], task["spec"]["max_steps"]),
                         ("Completed", "整理日志", 3))
        self.assertEqual(task["owner"]["user_id"], reminder_test.identity("user-1"))
        self.assertEqual(task["result"], RESULT)
        # 三步之后宿主不再提供工具：共 3 次工具调用、4 次模型请求，最后一次没有工具。
        self.assertEqual(task["usage"], {"tool_calls": 3, "model_requests": 4})
        workers = self.worker_requests()
        self.assertEqual(len(workers), 4)
        self.assertFalse(workers[-1].get("tools"))
        # 回复前已经保存：带派发结果的那次聊天请求发出时，账本里已有这个任务。
        answered = next(r for r in self.kind("chat")
                        if not self.is_worker(r["body"]) and self.results(r["body"]))
        self.assertEqual(answered["state"]["eve.task"]["tasks.v1"]["tasks"][0]["id"], task["id"])
        [push] = self.pushes()
        self.assertEqual((push["id"], push["target_id"]), (task["id"], "user-1"))
        self.assertEqual(push["text"], f"后台任务「整理日志」完成了：\n{RESULT}")

        # 之后再问：对话里能看到这个任务，模型按 ID 查看完整结果。
        def respond(body):
            if self.text(body) != STATUS:
                return self.chat(body)
            [shown] = self.context(body)["tasks"]
            self.assertEqual((shown["id"], shown["status"], shown["steps_used"]), (task["id"], "已完成", 3))
            results = self.results(body)
            if not results:
                return calls(("look", "list_background_tasks", {"task_id": task["id"]}))
            [result] = results
            self.assertEqual(result["task"]["result"], RESULT)
            return text_response("已经做完了：" + RESULT)
        self.callback = respond
        self.run_eve(self.send(self.message("status", STATUS, expected="已经做完了：" + RESULT)))
        # 其他用户看不到这个任务。
        self.callback = lambda body: (self.assertIsNone(self.context(body)), self.chat(body))[1]
        self.run_eve(self.send(self.message("other", "你好", user="user-2", expected="嗯。")))
        self.assertEqual(len(self.worker_requests()), 4, "完成的任务不重跑")

    def test_steps_above_the_configured_cap_are_refused_without_saving(self):
        def respond(body):
            results = self.results(body)
            if not results:
                return calls(("dispatch", "dispatch_background_task",
                              {"title": "整理日志", "instructions": INSTRUCTIONS, "max_steps": 300}))
            self.assertIn("超过宿主上限 200", json.dumps(results, ensure_ascii=False))
            return text_response("步数太多，没有派发。")
        self.callback = respond
        self.run_eve(self.send(self.message("ask", ASK, expected="步数太多，没有派发。")))
        self.assertEqual(self.tasks(), [])

    def held_worker(self, body):
        """执行器替身：第一个请求一直等到测试放行（或进程被停下）。"""
        self.release.wait(HOLD_SECONDS)
        return text_response(RESULT)

    def test_cancelling_a_running_task_stops_the_executor_without_a_notice(self):
        dispatch = self.dispatch()

        def respond(body):
            if self.is_worker(body):
                return self.held_worker(body)
            if self.text(body) == "算了，取消那个任务":
                results = self.results(body)
                if not results:
                    [shown] = self.context(body)["tasks"]
                    self.assertEqual(shown["status"], "执行中")
                    return calls(("cancel", "cancel_background_task", {"task_id": shown["id"]}))
                [result] = results
                self.assertFalse(result["cancelled"], "执行中的任务先记下取消请求")
                self.assertEqual(result["task"]["status"], "正在取消")
                return text_response("好，已经让它停下。")
            return dispatch(body)
        self.callback = respond
        self.run_eve([*self.send(self.message("ask", ASK, expected=DISPATCHED)),
                      *self.checkpoint("running"),
                      *self.send(self.message("cancel", "算了，取消那个任务", expected="好，已经让它停下。")),
                      *self.checkpoint("cancelled"), *self.checkpoint("quiet")],
                     checkpoints={
                         "running": lambda: self.wait_task(lambda t: t["status"] == "Running", "task did not start"),
                         "cancelled": lambda: self.wait_task(lambda t: t["status"] == "Cancelled",
                                                             "task was not cancelled"),
                         "quiet": self.quiet})
        self.release.set()
        [task] = self.tasks()
        self.assertTrue(task["cancel_requested"])
        self.assertIsNone(task["notice"], "用户取消的任务不再通知")
        self.assertIsNone(task["result"])
        self.assertEqual(self.pushes(), [])

    def test_a_legacy_task_without_checkpoint_interrupts_without_rerunning_and_tells_the_owner(self):
        dispatch = self.dispatch()

        def respond(body):
            if self.is_worker(body):
                return self.held_worker(body)
            return dispatch(body)
        self.callback = respond
        stop = self.gate("killed")
        self.run_eve([*self.send(self.message("ask", ASK, expected=DISPATCHED)), *self.checkpoint("running"),
                      {"touch": str(stop)}, {"wait_file": str(self.gate("never-released"))}],
                     checkpoints={"running": lambda: self.wait_task(
                         lambda t: t["status"] == "Running" and len(self.worker_requests()) == 1,
                         "task did not start")}, stop_at=stop)
        self.release.set()
        # 模拟升级前没有执行检查点的历史任务；不能对未知旧操作自动重放。
        state = json.loads(self.state_path.read_text())
        ledger = json.loads(bytes(state["entries"]["eve.task"]["tasks.v1"]))
        ledger["tasks"][0].pop("execution", None)
        state["entries"]["eve.task"]["tasks.v1"] = list(json.dumps(ledger).encode())
        self.state_path.write_text(json.dumps(state))
        self.callback = self.chat
        self.run_eve([*self.checkpoint("delivered"), *self.checkpoint("quiet")],
                     checkpoints={"delivered": lambda: self.wait_task(
                         lambda t: (t.get("notice") or {}).get("status") == "Delivered", "notice was not delivered"),
                         "quiet": self.quiet})
        [task] = self.tasks()
        self.assertEqual(task["status"], "Interrupted")
        self.assertIsNone(task["usage"])
        self.assertEqual(len(self.worker_requests()), 1, "重启后不自动重跑")
        [push] = self.pushes()
        self.assertEqual(push["text"], "后台任务「整理日志」因 Eve 重启中断了，没有完成；需要的话可以让我重新派发。")

    def test_a_subagent_does_a_subtask_within_the_turn_and_hands_back_its_result(self):
        task = "查一下 echo 是否可用，给出结论。"

        def respond(body):
            if self.is_worker(body):
                self.assertEqual(self.text(body), task)
                self.assertEqual([m["role"] for m in body["messages"]].count("user"), 1, "子代理看不到对话")
                if not body.get("tools"):
                    return text_response("echo 可用。")
                self.assertEqual(self.definitions(body), PLATFORM_TOOLS | {"echo"})
                if not self.results(body):
                    return calls(("probe", "echo", {"text": "ping"}))
                [echoed] = self.results(body)
                self.assertEqual(echoed, {"echo": "ping"})
                return text_response("echo 可用。")
            self.assertEqual(self.definitions(body), CHAT_TOOLS)
            results = self.results(body)
            if not results:
                return calls(("delegate", "run_subagent", {"task": task, "max_steps": 2}))
            [result] = results
            self.assertEqual(result, {"completed": True, "result": "echo 可用。", "include_context": False,
                                      "usage": {"steps_used": 1, "max_steps": 2, "model_requests": 2, "shared_steps_used": 2,"shared_steps_remaining":48,"agent_id":"general"}})
            return text_response("子代理查过了：echo 可用。")
        self.callback = respond
        self.run_eve(self.send(self.message("ask", "帮我确认一下 echo 能不能用", expected="子代理查过了：echo 可用。")))
        self.assertEqual(len(self.worker_requests()), 2)
        self.assertEqual(self.tasks(), [], "子代理不进入后台任务账本")

    def test_ten_subagents_execute_concurrently_in_one_tool_batch(self):
        condition = threading.Condition()
        entered = []
        def respond(body):
            if self.is_worker(body):
                with condition:
                    entered.append(self.text(body))
                    condition.notify_all()
                    self.assertTrue(condition.wait_for(lambda: len(entered) == 10, timeout=12),
                                    "ten subagents did not overlap")
                return text_response("checked " + self.text(body))
            results = self.results(body)
            if not results:
                return calls(*[(f"parallel-{i}", "run_subagent", {"task": f"check-{i}"}) for i in range(10)])
            self.assertEqual(len(results), 10)
            self.assertTrue(all(result["completed"] for result in results))
            return text_response("十个子代理全部完成")
        self.callback = respond
        self.run_eve(self.send(self.message("parallel", "并行核对十项", expected="十个子代理全部完成")))
        self.assertEqual(len(entered), 10)

    def test_subagents_can_run_serially_with_the_previous_result_as_input(self):
        def respond(body):
            if self.is_worker(body):
                return text_response("first-result" if self.text(body) == "first" else "second-result")
            results = self.results(body)
            if not results:
                return calls(("serial-1", "run_subagent", {"task": "first"}))
            if len(results) == 1:
                self.assertEqual(results[0]["result"], "first-result")
                return calls(("serial-2", "run_subagent", {"task": "use first-result"}))
            self.assertEqual(results[1]["result"], "second-result")
            return text_response("串行完成")
        self.callback = respond
        self.run_eve(self.send(self.message("serial", "依次核对", expected="串行完成")))
        self.assertEqual([self.text(r) for r in self.worker_requests()], ["first", "use first-result"])

    def test_both_are_off_by_default(self):
        def respond(body):
            self.assertEqual(self.definitions(body), PLATFORM_TOOLS | {"echo"})
            self.assertIsNone(self.context(body))
            return text_response("我现在不能派发后台任务。")
        self.callback = respond
        self.run_eve(self.send(self.message("ask", ASK, expected="我现在不能派发后台任务。")),
                     tasks=False, subagents=False)
        self.assertNotIn("eve.task", self.documents())
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_TASK_DEFAULT_STEPS="300")
        result = subprocess.run([str(BINARY), "--state-dir", str(self.work / "refused"),
                                 "--agent", str(ROOT / "AGENT.md"), "--background-tasks"],
                                env=env, capture_output=True, text=True, timeout=60)
        self.assertNotEqual(result.returncode, 0, "默认步数大于上限时拒绝启动")
        self.assertIn("runtime.tasks", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
