"""隔离宿主→本地模型协议→真实 read_file/edit_file→策略/世界账本验收。

EVE_QQBOT_BINARY 必须指向 cargo build -p eve-app --example task_strategy 的产物。
固定目录只属于临时目录中的合成身份，不启用生产策略、不连接真实 QQ/模型。
模型替身只验证完整宿主链路，不证明真实模型的策略泛化质量。
"""
import hashlib
import json
import unittest

import background_task_test as background
import reminder_test
from dialogue_tools_test import text_response

CASES = ("train", "holdout-a", "holdout-b")
INITIAL = "incorrect\n"
CORRECT = "correct\n"
STEPS = 4


class TaskStrategyProcessAcceptance(background.BackgroundTaskAcceptance):
    for _name in dir(background.BackgroundTaskAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.source = self.work / "source"
        self.source.mkdir()
        for name in CASES:
            (self.source / f"{name}.txt").write_text(INITIAL, encoding="utf8")
        (self.work / "task-strategy-fixture.marker").write_bytes(b"isolated-task-strategy-v1")
        self.developer_config = self.work / "developer.json"
        self.developer_config.write_text(json.dumps({
            "workspace": str(self.source), "operators": [reminder_test.identity("user-1")],
            "ordinary_access": False, "commands": False, "github": False, "self_upgrade": False,
            "max_tool_rounds": 4,
        }), encoding="utf8")
        self.callback = self.respond

    def run_fixture(self, script, **options):
        return self.run_eve(script, subagents=False, timeout=60,
                            env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config),
                                       "EVE_TASK_STRATEGY_FIXTURE": "1"}, **options)

    def strategies(self):
        return (self.documents().get("eve.task-strategy", {}).get("strategies.v1") or {}).get("snapshot", {})

    def world(self):
        return (self.documents().get("eve.world-model", {}).get("world-model.v1") or {}).get("snapshot", {})

    def respond(self, body):
        if self.is_worker(body):
            return self.repair(body)
        text = self.text(body)
        if text == "restart-check":
            return text_response("fixture restored")
        # 每轮只有测试脚本显式发送的合成任务可以派发；没有自造任务。
        label, name = text.split(":", 1)
        self.assertIn(name, CASES)
        results = self.results(body)
        if not results:
            return background.calls(("dispatch-fixture", "dispatch_background_task", {
                "title": label, "instructions": f"task-strategy-fixture:{name}",
                "max_steps": STEPS, "include_context": False,
            }))
        self.assertTrue(results[-1]["dispatched"], results)
        return text_response("fixture queued")

    def repair(self, body):
        text = self.text(body)
        name = next(name for name in CASES if f"task-strategy-fixture:{name}" in text)
        arguments = {"path": f"{name}.txt", "offset": 0}
        results = self.results(body)
        definitions = self.definitions(body)
        self.assertIn("read_file", definitions)
        self.assertIn("edit_file", definitions)
        self.assertNotIn("run_command", definitions)
        if not results:
            self.assertEqual((self.source / f"{name}.txt").read_text(), INITIAL,
                             "每个控制/试验开始前必须由测试宿主还原同一初态")
            self.assertIn('"evaluation"', text, "运行中实际装配执行前冻结的评价条件")
            return background.calls(("read-before", "read_file", arguments))
        if len(results) == 1:
            self.assertEqual(results[0]["content"], INITIAL)
            self.assertEqual(results[0]["sha256"], hashlib.sha256(INITIAL.encode()).hexdigest())
            return background.calls(("repair", "edit_file", {
                "path": f"{name}.txt", "old_string": INITIAL, "new_string": CORRECT,
                "expected_sha256": results[0]["sha256"],
            }))
        if len(results) == 2:
            self.assertTrue(results[-1]["success"], results)
            return background.calls(("read-after", "read_file", arguments))
        self.assertEqual(len(results), 3)
        self.assertEqual(results[-1]["sha256"], hashlib.sha256(CORRECT.encode()).hexdigest())
        return text_response("fixture verified")

    def settled(self, count):
        def ready():
            tasks = self.tasks()
            strategy = self.strategies()
            trials = self.world().get("trials", [])
            return (len(tasks) == count and all(t["status"] == "Completed" and
                    (t.get("notice") or {}).get("status") == "Delivered" for t in tasks)
                    and len(trials) == count and all(t["resolution"] is not None for t in trials)
                    and len(strategy.get("versions", [])) == 1
                    and all(s["observation"] is not None for s in strategy.get("sources", []) + strategy.get("selections", [])))
        self.wait_for(ready, f"第 {count} 个实际任务/策略/世界结果没有结算", seconds=30)
        for task in self.tasks():
            self.assertEqual(task["usage"], {"tool_calls": 3, "model_requests": 4})
            self.assertIsNone(task["spec"].get("context"))
        for trial in self.world()["trials"]:
            self.assertEqual(trial["resolution"]["effect"], "Verified")
            receipt = trial["resolution"]["receipt"]
            self.assertEqual(receipt["tool"], "read_file")
            self.assertEqual(list(receipt["output"]), ["observed"])
            self.assertEqual(list(receipt["output"]["observed"].values()),
                             [hashlib.sha256(CORRECT.encode()).hexdigest()])
            self.assertNotIn("content", receipt["output"])

    def test_real_training_two_held_out_pairs_and_restart_do_not_replay_tools(self):
        order = [("train", "train"), ("control-a", "holdout-a"), ("trial-a", "holdout-a"),
                 ("control-b", "holdout-b"), ("trial-b", "holdout-b")]
        script, checkpoints = [], {}
        for index, (label, name) in enumerate(order):
            before, after = f"reset-{index}", f"settle-{index}"
            checkpoints[before] = lambda name=name: (self.source / f"{name}.txt").write_text(INITIAL, encoding="utf8")
            checkpoints[after] = lambda count=index + 1: self.settled(count)
            script += self.checkpoint(before)
            script += self.send(self.message(f"task-{index}", f"{label}:{name}", expected="fixture queued"))
            script += self.checkpoint(after)
        summary = self.run_fixture(script, checkpoints=checkpoints)
        self.assertEqual(summary["task_strategy_fixture"]["views"],
                         [{"version": 1, "status": "Active", "paired_successes": 2}])
        strategy = self.strategies()
        self.assertEqual(len(strategy["sources"]), 1)
        self.assertTrue(strategy["sources"][0]["extracted"])
        self.assertEqual(len(strategy["selections"]), 4)
        self.assertEqual([next(iter(s["assignment"]["arm"])) for s in strategy["selections"]],
                         ["Control", "Trial", "Control", "Trial"])
        inputs = [s["input_sha256"] for s in strategy["selections"]]
        self.assertEqual(inputs[0], inputs[1])
        self.assertEqual(inputs[2], inputs[3])
        self.assertNotEqual(inputs[0], inputs[2])
        self.assertNotIn(strategy["sources"][0]["input_sha256"], inputs)
        self.assertEqual(len(self.worker_requests()), 20)
        world = self.world()
        tasks = self.tasks()
        files = {name: (self.source / f"{name}.txt").read_bytes() for name in CASES}
        operation_log = self.work / "state/developer-tools/operations.jsonl"
        before_log = operation_log.read_bytes()
        summary = self.run_fixture(self.send(self.message("restart", "restart-check", expected="fixture restored")))
        self.assertEqual(summary["task_strategy_fixture"]["views"][0]["status"], "Active")
        self.assertEqual(self.strategies(), strategy)
        self.assertEqual(self.world(), world)
        self.assertEqual(self.tasks(), tasks)
        self.assertEqual(len(self.worker_requests()), 20)
        self.assertEqual(operation_log.read_bytes(), before_log)
        self.assertEqual({name: (self.source / f"{name}.txt").read_bytes() for name in CASES}, files)


if __name__ == "__main__":
    unittest.main()
