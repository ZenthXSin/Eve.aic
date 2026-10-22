"""真实 QQ 宿主 + 本地 Chat SSE：完整提交后投递、工具仅执行一次、截断不提交。"""
import json
import sys
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response


class ModelStreamingAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.truncate = False

    def enabled(self, script):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                     env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config),
                                "EVE_OPENAI_STREAM_COMPLETIONS": "true"})

    def encode_provider_response(self, body, response):
        self.assertTrue(body["stream"])
        choice = response["choices"][0]
        message = choice["message"]
        chunks = [{"role": "assistant", "reasoning_content": "PRIVATE_REASONING_NEVER_DELIVER"}]
        if message.get("tool_calls"):
            chunks.append({"content": "TOOL_DRAFT_NEVER_DELIVER"})
            for index, call in enumerate(message["tool_calls"]):
                function = call["function"]
                chunks.append({"tool_calls": [{"index": index, "id": call["id"], "type": "function",
                                               "function": {"name": function["name"], "arguments": ""}}]})
                chunks.extend({"tool_calls": [{"index": index, "function": {"arguments": part}}]}
                              for part in function["arguments"])
        else:
            chunks.extend({"content": char} for char in message["content"])
        frames = [{"id": "fixture", "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
                  for delta in chunks]
        if not self.truncate:
            frames.append({"id": "fixture", "choices": [{"index": 0, "delta": {},
                                                          "finish_reason": choice["finish_reason"]}]})
        wire = "".join("data: " + json.dumps(frame, ensure_ascii=False) + "\r\n\r\n" for frame in frames)
        return "text/event-stream", wire.encode()

    def test_streamed_tools_and_text_commit_once_and_restart_never_replays(self):
        def respond(body):
            if not self.results(body):
                return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                    "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('一次完成')"]})])
            self.assertEqual(len(self.results(body)), 1)
            return text_response("流式接收，完整投递🌍")
        self.callback = respond
        self.enabled(self.send(self.message("stream", "执行一次检查", expected="流式接收，完整投递🌍")))
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        status = self.turn("执行一次检查")["status"]
        self.assertEqual(status["state"], "Completed")
        self.assertNotIn("NEVER_DELIVER", json.dumps(status))
        self.assertEqual(self.receipt("stream")["state"], "Sent")
        self.callback = lambda body: text_response("恢复后未重跑")
        duplicate = {**self.message("stream", "执行一次检查"), "expected_type": "finish"}
        self.enabled([{"send": duplicate}, {"wait_command": {"id": "stream", "type": "finish"}},
                      *self.send(self.message("restored", "检查状态", expected="恢复后未重跑"))])
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(self.chat_rounds, 3)

    def test_unfinished_tool_stream_never_executes_or_commits_draft(self):
        self.truncate = True
        self.callback = lambda body: tool_response([("write_file", {
            "path": "must-not-exist.txt", "content": "secret", "expected_sha256": ""})])
        self.enabled(self.send(self.message("partial", "验证中断", contains=["本轮已停止", "尚未启动工具调用"])))
        self.assertFalse((self.source / "must-not-exist.txt").exists())
        self.assertEqual(self.turn("验证中断")["status"]["state"], "Failed")
        self.assertEqual(self.chat_rounds, 1)
        self.assertEqual(len(self.interactions()), 0)

    def test_reused_tool_id_reports_protocol_failure_and_never_replays_the_command(self):
        self.callback = lambda body: tool_response([("run_command", {
            "program": sys.executable, "args": ["-c",
            "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x')"]})])
        self.enabled(self.send(self.message("duplicate-id", "验证重复标识", contains=[
            "工具调用标识被重复使用", "重复批次未执行", "已启动 1 次", "本轮已停止"])))
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(self.chat_rounds, 2)
        self.assertEqual(self.turn("验证重复标识")["status"]["failure"]["code"], "Protocol")
        self.assertEqual(len(self.interactions()), 0)
        self.callback = lambda body: text_response("未重复执行")
        self.enabled(self.send(self.message("after-duplicate", "核对已有结果", expected="未重复执行")))
        self.assertEqual((self.source / "once.txt").read_text(), "x")

    def test_interruption_after_execution_keeps_receipt_and_restart_does_not_replay(self):
        def respond(body):
            if not self.results(body):
                return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                    "from pathlib import Path; p=Path('executed.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('saved result')"]})])
            self.truncate = True
            return text_response("PARTIAL_FINAL_NEVER_DELIVER")
        self.callback = respond
        self.enabled(self.send(self.message("after-tool", "运行后中断", contains=["已启动 1 次", "本轮已停止"])))
        self.assertEqual((self.source / "executed.txt").read_text(), "x")
        status = self.turn("运行后中断")["status"]
        self.assertEqual(status["state"], "Failed")
        self.assertEqual(status["failure"]["evidence"]["tool_names"], ["run_command"])
        self.assertEqual(len(self.interactions()), 0)
        self.truncate = False
        self.callback = lambda body: text_response("已有执行回执，没有重跑")
        self.enabled(self.send(self.message("resume", "检查上次结果", expected="已有执行回执，没有重跑")))
        self.assertEqual((self.source / "executed.txt").read_text(), "x")
        self.assertEqual(self.chat_rounds, 3)


if __name__ == "__main__":
    unittest.main()
