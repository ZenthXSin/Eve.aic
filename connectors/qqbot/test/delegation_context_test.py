"""真实 QQ 进程验收委派上下文开关；模型与 QQ 均为本地替身。"""
import json
import unittest

import developer_tools_test as developer
from background_task_test import WORKER, calls
from dialogue_tools_test import text_response


class DelegationContextAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def text(self, body):
        return next(m["content"] for m in reversed(body["messages"]) if m["role"] == "user")

    def is_worker(self, body):
        return WORKER in body["messages"][0]["content"]

    def tasks(self):
        return self.documents().get("eve.task", {}).get("tasks.v1", {}).get("tasks", [])

    def enabled(self, script, **options):
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                            more=["--background-tasks", "--subagents"],
                            env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config)}, **options)

    def check_worker(self, body, include):
        names = self.definitions(body)
        leaf="你是 Eve 的子代理" in body["messages"][0]["content"]
        self.assertFalse(names & ({"dispatch_background_task", "set_wakeup_trigger", "create_subagent"} | ({"run_subagent"} if leaf else set())))
        text = self.text(body)
        self.assertEqual("eve.delegated_context" in text, include)
        if include:
            data = json.loads(text[text.index('{"kind":"eve.delegated_context"'):])
            encoded = json.dumps(data, ensure_ascii=False)
            self.assertIn("PARENT_FACT_735", encoded)
            self.assertIn("开始委派", encoded)
            self.assertNotIn("OTHER_USER_PRIVATE_928", encoded)
            self.assertFalse(any(c["name"] in {"run_subagent", "dispatch_background_task"}
                                 for m in data["messages"] for c in m["tool_calls"]))
        else:
            self.assertNotIn("PARENT_FACT_735", text)
        return text_response("委派核查完成")

    def exercise(self, tool, include):
        workers = []
        def respond(body):
            if self.is_worker(body):
                workers.append(body)
                return self.check_worker(body, include)
            if self.text(body) != "开始委派":
                return text_response("背景已收到")
            if not self.results(body):
                arguments = {"title":"核查", "instructions":"核查背景并交付结论"} if tool == "dispatch_background_task" else {"task":"核查背景并交付结论"}
                if include:
                    arguments["include_context"] = True
                return calls(("delegate", tool, arguments))
            [result] = self.results(body)
            self.assertTrue(result.get("dispatched", result.get("completed")), result)
            self.assertEqual(result.get("task", result)["include_context"], include)
            return text_response("委派成功")
        self.callback = respond
        script = [*self.send(self.message("other", "OTHER_USER_PRIVATE_928", user="user-2", expected="背景已收到")),
                  *self.send(self.message("fact", "PARENT_FACT_735", expected="背景已收到")),
                  *self.send(self.message("delegate", "开始委派", expected="委派成功"))]
        options = {}
        if tool == "dispatch_background_task":
            script += self.checkpoint("completed")
            options["checkpoints"] = {"completed": lambda: self.wait_for(
                lambda: any(t["status"] == "Completed" for t in self.tasks()), "background did not finish", 20)}
        self.enabled(script, **options)
        self.assertEqual(len(workers), 1)
        if tool == "dispatch_background_task":
            [task] = self.tasks()
            self.assertEqual("context" in task["spec"], include)
            self.assertEqual(task["spec"]["max_steps"], 100)

    def test_subagent_context_is_opt_in_and_scope_isolated(self):
        self.exercise("run_subagent", True)

    def test_subagent_defaults_to_no_context(self):
        self.exercise("run_subagent", False)

    def test_background_context_is_durable_and_scope_isolated(self):
        self.exercise("dispatch_background_task", True)

    def test_background_defaults_to_no_context(self):
        self.exercise("dispatch_background_task", False)

    def test_non_boolean_flag_is_rejected_before_worker_or_task_creation(self):
        def respond(body):
            self.assertFalse(self.is_worker(body))
            if not self.results(body):
                return calls(("bad", "dispatch_background_task", {
                    "title":"无效", "instructions":"不得执行", "include_context":"true"}))
            self.assertIn("include_context", json.dumps(self.results(body)))
            return text_response("参数错误，未派发")
        self.callback = respond
        self.enabled(self.send(self.message("bad", "无效开关", expected="参数错误，未派发")))
        self.assertEqual(self.tasks(), [])


if __name__ == "__main__":
    unittest.main()
