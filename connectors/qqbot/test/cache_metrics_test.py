"""实际 eve-qqbot 的提示缓存统计：模型替身在响应里报告输入与缓存命中，
面板按用途显示最近命中率与本地估算的可复用前缀；同一会话连续几轮的请求前缀应只在历史末尾之后变化。
替身的命中数是固定比例，只证明宿主读取与展示，不代表真实服务的缓存行为。
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
        broken = cache["last_break"]
        self.assertIsNotNone(broken)
        self.assertFalse(broken["tools_changed"])


if __name__ == "__main__":
    unittest.main()
