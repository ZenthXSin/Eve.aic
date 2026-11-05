"""实际 eve-qqbot 的提示缓存统计：模型替身在响应里报告输入与缓存命中，
面板按用途显示最近命中率与本地估算的可复用前缀；同一会话连续几轮的请求前缀应只在历史末尾之后变化。
本地 JSON/SSE 夹具报告预设用量，只证明真实进程读取、归集与展示，不代表上游缓存行为。
"""
import json
import unittest

import model_health_test


def first_difference(a, b):
    for index, (x, y) in enumerate(zip(a, b)):
        if x != y:
            return index
    return min(len(a), len(b))


class CacheMetricsAcceptance(model_health_test.ModelHealthAcceptance):
    for _name in dir(model_health_test.ModelHealthAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def http_status(self, kind, number):
        return None

    def provider_response(self, body, kind, response):
        if kind == "chat":
            size = len(json.dumps(body, ensure_ascii=False))
            response["usage"] = {"prompt_tokens": size, "completion_tokens": 3,
                                 "prompt_tokens_details": {"cached_tokens": size * 3 // 4}}
        return response

    def test_invalid_and_missing_usage_are_not_counted_as_hits_and_zero_is_reported(self):
        replies = iter([
            {"prompt_tokens": 1000, "prompt_tokens_details": {"cached_tokens": 1500}},
            {"prompt_tokens": 1000},
            {"prompt_tokens": 1000, "prompt_tokens_details": {"cached_tokens": 0}},
        ])

        def response(body, kind, fallback):
            if kind == "chat":
                fallback["usage"] = next(replies)
            return fallback

        self.provider_response = response
        seen = {}

        def inspect():
            seen.update(self.dialogue(self.readiness())["cache"])

        self.run_eve([
            *self.send(self.message("invalid", "第一句")),
            *self.send(self.message("missing", "第二句")),
            *self.send(self.message("zero", "第三句")),
            *self.checkpoint("usage"),
        ], interest=False, panel=True, checkpoints={"usage": inspect})
        self.assertEqual((seen["reported_requests"], seen["unreported_requests"]), (1, 2))
        self.assertEqual((seen["input_tokens"], seen["cached_tokens"]), (1000, 0))
        self.assertEqual(seen["recent_reported_ratio"], 0)

    def test_cache_hits_and_local_prefix_reuse_are_shown_per_use(self):
        seen = {}

        def inspect():
            seen["dialogue"] = self.dialogue(self.readiness())

        self.run_eve([
            *self.send(self.message("one", "第一句")),
            *self.send(self.message("two", "第二句")),
            *self.send(self.message("three", "第三句")),
            *self.checkpoint("cache"),
        ], interest=False, panel=True, checkpoints={"cache": inspect})
        cache = seen["dialogue"]["cache"]
        self.assertEqual(cache["reported_requests"], 3)
        self.assertEqual(cache["unreported_requests"], 0)
        self.assertGreaterEqual(cache["recent_reported_ratio"], 7400)
        self.assertLessEqual(cache["recent_reported_ratio"], 7500)
        self.assertEqual(cache["local_requests"], 3)
        self.assertGreater(cache["recent_local_ratio"], 0)
        # 同一会话的后续请求：系统提示、工具清单与此前历史保持原样，变化只出现在历史之后。
        bodies = [r["body"] for r in self.kind("chat")]
        for previous, current in zip(bodies, bodies[1:]):
            self.assertEqual(previous["tools"], current["tools"], "工具定义每轮不变")
            history = len([m for m in previous["messages"] if m["role"] in ("user", "assistant")])
            changed = first_difference(previous["messages"], current["messages"])
            self.assertGreaterEqual(changed, 2, "系统提示与工具清单保持不变")
        # 宿主事实、工具说明、授权与训练开关提示属于固定资料：并在工具清单那条系统消息里，位于历史之前。
        for body in bodies:
            fixed = body["messages"][1]["content"]
            for kind in ("eve.host.capabilities", "eve.host.authorization", "eve.host-facts"):
                self.assertIn(kind, fixed)
                self.assertFalse(any(kind in str(m.get("content")) for m in body["messages"][2:]), kind)
        broken = cache["last_break"]
        self.assertIsNotNone(broken)
        self.assertFalse(broken["tools_changed"])


class StreamingCacheMetricsAcceptance(CacheMetricsAcceptance):
    for _name in dir(CacheMetricsAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    # 声明在 Content-Length 内但暂不发送，确保 DONE/统计期限不依赖 HTTP EOF。
    HELD = b": fixture connection remains open\n\n"

    def setUp(self):
        super().setUp()
        self.stream_replies = []
        self.stream_plans = {}
        self.held_requests = set()

    def provider_response(self, body, kind, response):
        self.assertEqual(kind, "chat")
        self.assertTrue(self.stream_replies, "出现未预期的额外请求或重试")
        plan = self.stream_replies.pop(0)
        plan["number"] = len(self.stream_plans) + 1
        self.stream_plans[id(body)] = plan
        if plan.get("tool"):
            response["choices"][0] = {
                "index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": None, "tool_calls": [{
                        "id": "cache-echo-once", "type": "function", "function": {
                            "name": "echo", "arguments": json.dumps({"text": "cache-tail-receipt"})}}]}}
        elif "text" in plan:
            response["choices"][0]["message"]["content"] = plan["text"]
        return response

    def encode_provider_response(self, body, response):
        self.assertTrue(body["stream"])
        plan = self.stream_plans[id(body)]
        stream_id = f"cache-stream-{plan['number']}"
        choice = response["choices"][0]
        message = choice["message"]
        deltas = [{"role": "assistant"}]
        if message.get("tool_calls"):
            for index, call in enumerate(message["tool_calls"]):
                deltas.append({"tool_calls": [{"index": index, **call}]})
        else:
            deltas.append({"content": message["content"]})
        frames = [{"id": stream_id, "choices": [{"index": 0, "delta": delta,
                                                       "finish_reason": None}]} for delta in deltas]
        frames.append({"id": stream_id, "choices": [{"index": 0, "delta": {},
                                                       "finish_reason": choice["finish_reason"]}]})

        def event(frame):
            return ("data: " + json.dumps(frame, ensure_ascii=False) + "\r\n\r\n").encode()

        prefix = b"".join(event(frame) for frame in frames)
        tail = b""
        if "usage" in plan:
            tail += event({"id": stream_id, "choices": [], "usage": plan["usage"]})
        mode = plan.get("tail", "done")
        if mode == "done":
            tail += b"data: [DONE]\r\n\r\n"
        elif mode == "invalid":
            tail += b"data: {invalid-tail\r\n\r\n"
        else:
            self.assertEqual(mode, "hold")
        plan["prefix_bytes"] = len(prefix)
        return "text/event-stream", prefix + tail + self.HELD

    def write_provider_response(self, handler, body, encoded):
        plan = self.stream_plans[id(body)]
        payload = encoded[:-len(self.HELD)]
        with self.lock:
            self.held_requests.add(plan["number"])
        try:
            if plan.get("split"):
                # 终态与用量分别写入；另一用例将两者放在同一次写入中。
                boundary = plan["prefix_bytes"]
                handler.wfile.write(payload[:boundary])
                handler.wfile.flush()
                if self.run_release.wait(0.05):
                    return
                payload = payload[boundary:]
            handler.wfile.write(payload)
            handler.wfile.flush()
            self.run_release.wait(20)
        finally:
            with self.lock:
                self.held_requests.discard(plan["number"])

    def streamed(self, script, checkpoints):
        self.run_eve(script, interest=False, panel=True, checkpoints=checkpoints,
                     env_extra={"EVE_OPENAI_STREAM_COMPLETIONS": "true",
                                "EVE_OPENAI_REASONING_EFFORT": "omit",
                                "EVE_OPENAI_TRANSIENT_RETRIES": "true"})
        self.assertFalse(self.stream_replies)

    def assert_stream_requests(self):
        for request in self.kind("chat"):
            body = request["body"]
            self.assertEqual(body.get("stream_options"), {"include_usage": True})
            self.assertNotIn("temperature", body)
            self.assertNotIn("reasoning_effort", body)

    @staticmethod
    def usage(input_tokens, cached_tokens):
        return {"prompt_tokens": input_tokens, "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": cached_tokens}}

    def test_finish_then_usage_then_done_reports_hits_missing_and_explicit_zero(self):
        self.stream_replies = [
            {"usage": self.usage(2048, 1024)},
            {"usage": {"prompt_tokens": 3072}, "split": True},
            {"usage": self.usage(4096, 0), "split": True},
        ]
        seen = {}

        def inspect(name, requests):
            seen[name] = self.dialogue(self.readiness())
            self.assertEqual((seen[name]["requests"], seen[name]["failures"]), (requests, 0))
            with self.lock:
                self.assertIn(requests, self.held_requests, "检查时本地 HTTP 响应尚未 EOF")

        self.streamed([
            *self.send(self.message("stream-hit", "确认缓存统计")),
            *self.checkpoint("hit"),
            *self.send(self.message("stream-missing", "确认缺失统计")),
            *self.checkpoint("missing"),
            *self.send(self.message("stream-zero", "确认零命中")),
            *self.checkpoint("zero"),
        ], {"hit": lambda: inspect("hit", 1), "missing": lambda: inspect("missing", 2),
            "zero": lambda: inspect("zero", 3)})
        hit, missing, zero = (seen[name]["cache"] for name in ("hit", "missing", "zero"))
        self.assertEqual((hit["reported_requests"], hit["unreported_requests"]), (1, 0))
        self.assertEqual((hit["input_tokens"], hit["cached_tokens"], hit["recent_reported_ratio"]),
                         (2048, 1024, 5000))
        self.assertEqual((missing["reported_requests"], missing["unreported_requests"]), (1, 1))
        self.assertEqual((missing["input_tokens"], missing["cached_tokens"]), (2048, 1024))
        self.assertEqual((zero["reported_requests"], zero["unreported_requests"]), (2, 1))
        self.assertEqual((zero["input_tokens"], zero["cached_tokens"], zero["recent_reported_ratio"]),
                         (6144, 1024, 1666))
        self.assertEqual(len(self.kind("chat")), 3)
        self.assert_stream_requests()

    def test_tool_terminal_tail_is_counted_and_real_echo_executes_once(self):
        self.stream_replies = [
            {"tool": True, "usage": self.usage(2048, 1024), "split": True},
            {"text": "已核对真实回执", "usage": self.usage(4096, 2048)},
        ]
        seen = {}
        self.streamed([
            *self.send(self.message("stream-tool", "执行一次本地回显并核对", expected="已核对真实回执")),
            *self.checkpoint("tool"),
        ], {"tool": lambda: seen.update(self.dialogue(self.readiness()))})
        self.assertEqual((seen["requests"], seen["failures"]), (2, 0))
        cache = seen["cache"]
        self.assertEqual((cache["reported_requests"], cache["unreported_requests"]), (2, 0))
        self.assertEqual((cache["input_tokens"], cache["cached_tokens"], cache["recent_reported_ratio"]),
                         (6144, 3072, 5000))
        bodies = [request["body"] for request in self.kind("chat")]
        self.assertEqual(len(bodies), 2)
        receipts = [message for message in bodies[1]["messages"] if message["role"] == "tool"]
        self.assertEqual(len(receipts), 1)
        self.assertEqual(receipts[0]["tool_call_id"], "cache-echo-once")
        self.assertEqual(json.loads(receipts[0]["content"]), {"echo": "cache-tail-receipt"})
        status = self.turn("执行一次本地回显并核对")["status"]
        self.assertEqual(status["state"], "Completed")
        self.assertEqual(sum(len(message.get("tool_calls", [])) for message in status["messages"]), 1)
        self.assert_stream_requests()

    def test_invalid_or_stalled_tail_keeps_completed_reply_without_retry(self):
        self.stream_replies = [{"tail": "invalid"}, {"tail": "hold"}]
        seen = {}

        def inspect():
            seen.update(self.dialogue(self.readiness()))
            with self.lock:
                self.assertIn(2, self.held_requests, "模型终态后停顿不能等到 HTTP EOF 才完成")

        self.streamed([
            *self.send(self.message("bad-tail", "坏尾帧仍保留完整回复")),
            *self.send(self.message("held-tail", "停顿尾流仍保留完整回复")),
            *self.checkpoint("tail"),
        ], {"tail": inspect})
        self.assertEqual((seen["requests"], seen["failures"]), (2, 0))
        self.assertEqual((seen["cache"]["reported_requests"], seen["cache"]["unreported_requests"]), (0, 2))
        self.assertIsNone(seen["cache"]["recent_reported_ratio"])
        self.assertEqual(len(self.kind("chat")), 2)
        for text in ("坏尾帧仍保留完整回复", "停顿尾流仍保留完整回复"):
            self.assertEqual(self.turn(text)["status"]["state"], "Completed")
        self.assert_stream_requests()


if __name__ == "__main__":
    unittest.main()
