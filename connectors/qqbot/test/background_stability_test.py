"""真实后台执行：只读错误可纠正、5000 步上限、压缩后检查点与重启不重放。"""
import json
import sys
import unittest

import auto_recovery_test as recovery
from background_task_test import calls
from dialogue_tools_test import text_response


class BackgroundStabilityAcceptance(recovery.AutoRecoveryAcceptance):
    for _name in dir(recovery.AutoRecoveryAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def finished(self):
        self.wait_for(lambda: self.tasks() and self.tasks()[0]["status"] in ("Completed", "Failed")
            and (self.tasks()[0].get("notice") or {}).get("status") == "Delivered",
            "任务及通知未结束", 20)
        self.assertEqual(self.tasks()[0]["status"], "Completed", self.tasks()[0].get("failure"))

    def command(self):
        return {"program":sys.executable, "args":["-c",
            "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('ACTUAL_ONCE_RECEIPT')"]}

    def foreground(self, body, steps):
        if self.text(body) == "重启后核对":
            return text_response("原任务保持完成")
        if not self.results(body):
            return calls(("dispatch", "dispatch_background_task", {
                "title":"后台稳定性核验", "instructions":"核对文件并执行一次命令，然后汇报", "max_steps":steps}))
        self.assertTrue(self.results(body)[-1]["dispatched"])
        return text_response("后台已派发")

    def exercise(self, steps):
        self.run_recovery([*self.send(self.message("dispatch", "派发稳定性核验", expected="后台已派发")),
            *self.checkpoint("finished")], checkpoints={"finished":self.finished})
        [task] = self.tasks()
        self.assertEqual(task["spec"]["max_steps"], steps)
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.run_recovery(self.send(self.message("restart", "重启后核对", expected="原任务保持完成")))
        self.assertEqual(self.tasks(), [task])
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        return task

    def test_5000_step_task_corrects_read_only_error_and_preserves_write_once(self):
        def respond(body):
            if not self.is_worker(body):
                definition = next(t for t in body["tools"] if t["function"]["name"] == "dispatch_background_task")
                self.assertEqual(definition["function"]["parameters"]["properties"]["max_steps"]["maximum"], 5000)
                return self.foreground(body, 5000)
            results = self.results(body)
            if not results:
                return calls(("wrong-path", "search_files", {"path":"README.md", "query":"真实"}))
            self.assertEqual(results[0]["error"]["code"], "InvalidArguments")
            if len(results) == 1:
                return calls(("correct-path", "search_files", {"path":".", "query":"真实"}))
            if len(results) == 2:
                self.assertTrue(results[1]["matches"])
                return calls(("write-once", "run_command", self.command()))
            return text_response("已纠正搜索范围并核验完成")
        self.callback = respond
        task = self.exercise(5000)
        self.assertEqual(task["usage"]["tool_calls"], 3)
        self.assertEqual(task["execution"]["recoveries"], 0)

    def test_background_compaction_keeps_full_checkpoint_and_resumes_without_replay(self):
        content = "中文结果" * 2700
        (self.source / "big.txt").write_text(content, encoding="utf8")
        self.worker_calls = 0
        self.digests = 0
        def respond(body):
            if body["messages"][0]["content"].startswith("你是对话历史压缩器"):
                self.digests += 1
                request = json.loads(body["messages"][1]["content"])
                covered = request["previous_turns"] + len(request["turns"])
                return text_response(json.dumps({"points":[{"text":"八次读取已经执行，接着核验命令", "turns":[covered]}]}, ensure_ascii=False))
            if not self.is_worker(body):
                return self.foreground(body, 20)
            self.worker_calls += 1
            if self.worker_calls == 1:
                return calls(*[(f"read-{i}", "read_file", {"path":"big.txt"}) for i in range(8)])
            self.assertTrue(any("宿主上下文压缩摘要" in (m.get("content") or "") for m in body["messages"]))
            if self.worker_calls == 2:
                return calls(("write-once", "run_command", self.command()))
            return text_response("压缩后继续核验完成")
        self.callback = respond
        task = self.exercise(20)
        self.assertGreater(self.digests, 0)
        checkpoint = task["execution"]["checkpoint"]
        self.assertEqual(checkpoint["used_tool_calls"], 9)
        self.assertEqual(sum(len(m["tool_calls"]) for m in checkpoint["messages"]), 9)
        self.assertTrue(content in json.dumps(checkpoint, ensure_ascii=False))
        self.assertEqual(task["usage"]["tool_calls"], 9)


if __name__ == "__main__":
    unittest.main(verbosity=2)
