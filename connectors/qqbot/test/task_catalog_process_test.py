"""正式 CLI 的可信目录验收：仅本地模型、假桥和临时文件，不访问正式QQ。"""
import hashlib
import json
import unittest
import memory_test
import reminder_test
import task_strategy_process_test as fixture


class CatalogAcceptance(fixture.TaskStrategyProcessAcceptance):
    test_real_training_two_held_out_pairs_and_restart_do_not_replay_tools = None
    panel = memory_test.MemoryAcceptance.panel

    def setUp(self):
        super().setUp()
        self.catalog_path = self.work / "trusted-catalog.json"
        owner = reminder_test.identity("user-1")
        cases = []
        for name in fixture.CASES:
            data = [f"task-strategy-fixture:{name}", fixture.STEPS, None]
            cases.append({"owner": {"session_id": owner, "user_id": owner}, "family": "file-check",
                "input_sha256": hashlib.sha256(json.dumps(data, separators=(",", ":")).encode()).hexdigest(),
                "kind": "train" if name == "train" else "holdout",
                "contract": {"require_final_batch": True, "checks": [{"tool": "read_file",
                    "arguments": {"path": f"{name}.txt", "offset": 0}, "output_pointer": "/sha256",
                    "expected": hashlib.sha256(fixture.CORRECT.encode()).hexdigest()}]}})
        self.catalog_path.write_text(json.dumps({"format_version": 1, "revision": 1, "cases": cases}))

    def run_catalog(self, script, **options):
        self.worker_subagents = False
        return reminder_test.base().run_eve(self, script, interest=False, research=False, practice=False,
            more=["--max-tool-rounds", "4", "--background-tasks", "--task-strategy-catalog", str(self.catalog_path)],
            timeout=60, env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config)}, **options)

    def test_empty_catalog_starts_plugins_without_inventing_learning(self):
        self.catalog_path.write_text(json.dumps({"format_version": 1, "revision": 1, "cases": []}))
        def inspect():
            code, result = self.panel("/api/plugins")
            self.assertEqual(code, 200)
            states = {item["id"]: item["state"] for item in result["items"]}
            self.assertEqual(states["eve.task-strategy"], "Active")
            self.assertEqual(states["eve.world-model"], "Active")
        self.run_catalog([*self.checkpoint("plugins"), *self.send(self.message("ordinary", "restart-check", expected="fixture restored"))],
            panel=True, checkpoints={"plugins": inspect})
        self.assertFalse(self.strategies().get("versions"))
        self.assertFalse(self.strategies().get("sources"))
        self.assertFalse(self.world().get("trials"))

    def test_catalog_runs_real_training_pairs_apply_and_restart(self):
        order = [("train", "train"), ("control-a", "holdout-a"), ("trial-a", "holdout-a"),
            ("control-b", "holdout-b"), ("trial-b", "holdout-b"), ("apply", "holdout-a")]
        script, checkpoints = [], {}
        for index, (label, name) in enumerate(order):
            before, after = f"reset-{index}", f"settle-{index}"
            checkpoints[before] = lambda name=name: (self.source / f"{name}.txt").write_text(fixture.INITIAL)
            checkpoints[after] = lambda count=index + 1: self.settled(count)
            script += self.checkpoint(before) + self.send(self.message(f"task-{index}", f"{label}:{name}", expected="fixture queued")) + self.checkpoint(after)
        self.run_catalog(script, checkpoints=checkpoints)
        strategies = self.strategies()
        self.assertEqual([next(iter(s["assignment"]["arm"])) for s in strategies["selections"]],
            ["Control", "Trial", "Control", "Trial", "Apply"])
        self.assertEqual(strategies["selections"][-1]["selected_version"], 1)
        world, tasks = self.world(), self.tasks()
        calls = len(self.worker_requests())
        self.assertEqual(calls, 24)
        self.run_catalog(self.send(self.message("restart", "restart-check", expected="fixture restored")))
        self.assertEqual(self.strategies(), strategies)
        self.assertEqual(self.world(), world)
        self.assertEqual(self.tasks(), tasks)
        self.assertEqual(len(self.worker_requests()), calls)


if __name__ == "__main__":
    unittest.main()
