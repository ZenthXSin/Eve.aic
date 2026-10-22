"""压缩流式超时与会话提示的真实进程验收；只使用本地模型和 QQ 替身。"""
import json
import time
import unittest
from collections import Counter

import context_compaction_test as compact
import history_digest_test as digest
from dialogue_tools_test import text_response, tool_response
import model_streaming_test as streaming

NOTICE = "正在进行上下文压缩，请稍等"


class CompactionStreamAcceptance(compact.ContextCompactionAcceptance):
    for _name in dir(compact.ContextCompactionAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name
    truncate = False
    encode_provider_response = streaming.ModelStreamingAcceptance.encode_provider_response

    def write_provider_response(self, handler, body, encoded):
        compressing = body["messages"][0]["content"].startswith(digest.DIGEST_PROMPT)
        try:
            if compressing and self.stall:
                handler.wfile.write(encoded[:20]); handler.wfile.flush()
                time.sleep(.6)
                return
            width = max(1, (len(encoded) + 7) // 8) if compressing else len(encoded)
            for offset in range(0, len(encoded), width):
                handler.wfile.write(encoded[offset:offset+width]); handler.wfile.flush()
                if compressing and offset + width < len(encoded):
                    time.sleep(.2)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def exercise(self, stall, invalid=0):
        self.stall = stall
        self.remaining_invalid_digests = invalid
        failed = stall or invalid >= 2
        self.dialogue_calls = 0
        (self.source / "README.md").write_text("中文结果" * 700, encoding="utf8")
        def respond(body):
            if body["messages"][0]["content"].startswith(digest.DIGEST_PROMPT):
                self.assertEqual(body.get("response_format"), {"type": "json_object"})
                if self.remaining_invalid_digests:
                    self.remaining_invalid_digests -= 1
                    return text_response("我会继续帮你核验文件。")
                request = json.loads(body["messages"][1]["content"])
                covered = request["previous_turns"] + len(request["turns"])
                return text_response(json.dumps({"points":[{"text":"35 次文件核验已执行，保留当前目标并交付结果", "turns":[covered]}]}, ensure_ascii=False))
            self.dialogue_calls += 1
            if self.dialogue_calls <= 3:
                response = tool_response([("read_file", {"path":"README.md"})] * [1, 14, 20][self.dialogue_calls-1])
                for call in response["choices"][0]["message"]["tool_calls"]:
                    call["id"] += f"-batch-{self.dialogue_calls}"
                return response
            self.assertTrue(any("宿主上下文压缩摘要" in (m.get("content") or "") for m in body["messages"]))
            return text_response("压缩后已继续并完成核验")
        self.callback = respond
        expected = {"contains":["上下文压缩未能完成", "已启动 35 次工具调用"]} if failed else {"expected":"压缩后已继续并完成核验"}
        self.run_window(self.send(self.message("compact", "核验文件后继续完成", **expected)), panel=True,
            env_extra={"EVE_CONTEXT_COMPACT_AT_TOKENS":"150000", "EVE_CONTEXT_COMPACT_TO_TOKENS":"100000",
                       "EVE_OPENAI_STREAM_COMPLETIONS":"true", "EVE_OPENAI_TIMEOUT_SECONDS":"1", "EVE_TRACE_DIALOGUE_TIMING":"true",
                       "EVE_MODELS_PRIMARY_TIMEOUT_MS":"1000", "EVE_MODEL_FIRST_RESPONSE_TIMEOUT_MS":"350",
                       "EVE_MODEL_STREAM_IDLE_TIMEOUT_MS":"350", "EVE_MODEL_STREAM_TOTAL_TIMEOUT_MS":"3000"})
        receipt = self.receipt("compact")
        self.assertEqual([p["text"] for p in receipt["progress"]].count(NOTICE), 1)
        self.assertEqual(len(receipt["progress"]), 3, "两个工具里程碑加一次压缩提示，为最终回复保留名额")
        self.assertTrue(all(p["state"] == "Sent" for p in receipt["progress"]))
        self.assertTrue(self.digest_requests())
        self.assertTrue(all(r["stream"] for r in self.digest_requests()))
        self.assertEqual(receipt["state"], "Sent")
        status = self.turn("核验文件后继续完成")["status"]
        if failed:
            self.assertEqual(status["failure"]["code"], "Context")
            self.assertEqual(status["failure"]["started_tools"], 35)
        else:
            self.assertEqual(status["state"], "Completed")
            self.assertEqual(sum(len(m.get("tool_calls", [])) for m in status["messages"]), 35)
        log = self.panel_stderr.read_text()
        self.assertIn('"use_name":"digest"', log)
        self.assertEqual(self.dialogue_calls, 3 if failed else 4)
        if invalid:
            batches = Counter(r["messages"][1]["content"] for r in self.digest_requests())
            self.assertEqual(list(batches.values()).count(2), 1, "只有无效摘要所在批次纠错一次")
            self.assertLessEqual(max(batches.values()), 2, "同一批摘要不无限重试")
            if failed:
                self.assertEqual(len(self.digest_requests()), 2)

    def test_healthy_digest_stream_outlives_old_total_timeout_and_notifies_once(self):
        self.exercise(False)

    def test_stalled_digest_keeps_evidence_and_sends_no_partial_result(self):
        self.exercise(True)

    def test_plain_chat_digest_is_repaired_without_repeating_tools(self):
        self.exercise(False, invalid=1)

    def test_invalid_digest_after_one_repair_reports_compaction_and_preserves_tools(self):
        self.exercise(False, invalid=2)


if __name__ == "__main__":
    unittest.main()
