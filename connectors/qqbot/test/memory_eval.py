"""跨会话记忆与提示缓存的离线评估：用实际 eve-qqbot 跑一段多会话剧本，按模型替身收到的请求计算指标。

指标：
- 可见性正确率：同一个人在别的会话里该看到的印象与约定是否出现，别人、未绑定的私聊、默认不进群的私聊资料是否没有出现。
- 提示前缀可复用率：同一会话相邻两次请求，按“工具定义 + 消息序列”的公共前缀长度占后一次请求的比例，与服务商提示缓存的口径一致。
- 上下文规模：每次请求的总字符数，以及人名资料、约定、跨会话记忆三类记忆数据各占多少。

模型替身确定性地应答与调用工具，指标只衡量宿主组装上下文的行为，不代表真实模型的理解质量。
报告写到 EVE_MEMORY_EVAL_REPORT 指定的文件（默认不写），并在标准输出打印一行 EVE_MEMORY_EVAL。

真实模型模式（`RealModelEvaluation`）：设置 EVE_MEMORY_EVAL_BASE_URL（OpenAI 兼容服务，如
https://api.openai.com/v1）、EVE_MEMORY_EVAL_API_KEY 与 EVE_MEMORY_EVAL_MODEL 后运行，本地替身把每个请求
原样转发给该服务，记录服务商报告的缓存命中 token，并检查模型是否用上跨会话资料与约定工具。
结果打印为一行 EVE_MEMORY_EVAL_REAL；会消耗模型额度，CI 不运行。
"""
import json
import os
import statistics
import time
import unittest
import urllib.request

import commitment_test
import people_test
from dialogue_tools_test import text_response, tool_response

# 门槛按当前实现的实测值留出余量；改动记忆或上下文组装后用同一剧本比较前后结果。
MIN_STEADY_PREFIX_REUSE = 0.6
MEMORY_MARKERS = {
    "people": "当前 QQ 人名资料",
    "commitments": '"kind":"eve.commitments.v1"',
    "continuity": '"kind":"eve-person-memory-v1"',
}


def parts(body):
    return [json.dumps(body.get("tools", []), ensure_ascii=False, sort_keys=True)] + [
        json.dumps(m, ensure_ascii=False, sort_keys=True) for m in body["messages"]]


def reuse(previous, current):
    """后一次请求与前一次请求的公共前缀字符数占后一次请求的比例。"""
    a, b = "".join(parts(previous)), "".join(parts(current))
    common = 0
    for x, y in zip(a, b):
        if x != y:
            break
        common += 1
    return common / len(b)


class MemoryEvaluation(people_test.PeopleAcceptance):
    for _name in dir(people_test.PeopleAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def test_cross_session_visibility_prefix_reuse_and_context_size(self):
        group_talk = ["我最近在学Rust", "我养了一只橘猫", "今天好累啊我", "我下周要去成都出差"]
        private_talk = ["我其实是护士", "我上夜班", "我有点累", "我先睡了"]
        steady = [f"随便聊聊第{i}句" for i in range(6)]

        def impressions(batch):
            # 一批里可能还带着别的原话（如请 Eve 帮忙的那句），按是否包含自述原话判断。
            if "我最近在学Rust" in batch["messages"]:
                return {"impressions": [{"text": "在学 Rust", "quote": "我最近在学Rust", "replaces": None}]}
            if "我其实是护士" in batch["messages"]:
                return {"impressions": [{"text": "是护士", "quote": "我其实是护士", "replaces": None}]}
            return {"impressions": []}
        self.impression_reply = impressions
        seen, order = {}, []

        def respond(body):
            messages = body["messages"]
            current = max(i for i, m in enumerate(messages) if m["role"] == "user")
            text = messages[current]["content"]
            results = [m for m in messages[current + 1:] if m["role"] == "tool"]
            order.append((text, body))
            if not results:
                seen[text] = body
            if text == "下次帮我看看地图吧" and not results:
                return tool_response([("note_commitment", {"text": "帮他看看地图"})])
            return text_response("好")
        self.callback = respond
        sessions = {}

        def say(mid, text, label, user="user-1", scope="group", target="group-1"):
            sessions[text] = label
            return self.send(self.message(mid, text, user=user, scope=scope, target=target, expected="好"))
        route = lambda scope, target: "qq:" + __import__("hashlib").sha256(json.dumps(
            ["1904159860", scope, target, "user-1"], separators=(",", ":")).encode()).hexdigest()
        bound = " ".join([route("c2c", "user-1"), route("group", "group-1"), route("group", "group-2")])
        a = dict(label="A")
        self.enabled([*[s for i, t in enumerate(group_talk) for s in say(f"a{i}", t, **a)],
                      *say("promise", "下次帮我看看地图吧", **a),
                      *self.checkpoint("learned"),
                      *say("b", "又见面了", "B", target="group-2"),
                      *say("c", "大家好", "C", user="user-2", target="group-2"),
                      *[s for i, t in enumerate(private_talk) for s in say(f"p{i}", t, "P", scope="c2c", target=None)],
                      *self.checkpoint("private-learned"),
                      *say("p-unbound", "私聊问一下", "P", scope="c2c", target=None),
                      *self.checkpoint("bind"),
                      *say("p-bound", "绑定后私聊", "P", scope="c2c", target=None),
                      *say("b-bound", "绑定后在群里", "B", target="group-2"),
                      *[s for i, t in enumerate(steady) for s in say(f"s{i}", t, **a)]],
                     checkpoints={"learned": lambda: self.wait_learned(1),
                                  "private-learned": lambda: self.wait_learned(2),
                                  "bind": lambda: self.save("runtime.people", owner_principals=bound)},
                     panel=True)

        def visible(text):
            body = seen[text]
            facts = self.facts(body)
            people = json.dumps(facts, ensure_ascii=False)
            open_ = (commitment_test.commitments(body) or {}).get("open", [])
            return {"rust": "在学 Rust" in people, "nurse": "是护士" in people,
                    "promise": any(c["text"] == "帮他看看地图" for c in open_)}
        probes = [
            ("同一账号在另一个群看到群里的印象", "又见面了", "rust", True),
            ("同一账号在另一个群看到答应过的事", "又见面了", "promise", True),
            ("别人看不到这个人的印象", "大家好", "rust", False),
            ("别人看不到答应这个人的事", "大家好", "promise", False),
            ("私聊未绑定时看不到群里的印象", "私聊问一下", "rust", False),
            ("私聊未绑定时看不到群里答应的事", "私聊问一下", "promise", False),
            ("私聊绑定后看到群里的印象", "绑定后私聊", "rust", True),
            ("私聊绑定后看到群里答应的事", "绑定后私聊", "promise", True),
            ("私聊里说的事默认不进群", "绑定后在群里", "nurse", False),
        ]
        results = [{"probe": name, "expected": expected, "actual": visible(text)[key]}
                   for name, text, key, expected in probes]
        correct = sum(r["expected"] == r["actual"] for r in results)

        by_session = {}
        for text, body in order:
            by_session.setdefault(sessions.get(text, "?"), []).append(body)
        ratios = {label: [reuse(p, c) for p, c in zip(bodies, bodies[1:])] for label, bodies in by_session.items()}
        steady_bodies = [seen[t] for t in steady]
        steady_ratios = [reuse(p, c) for p, c in zip(steady_bodies, steady_bodies[1:])]

        def memory_sizes(body):
            sizes = {name: 0 for name in MEMORY_MARKERS}
            for m in body["messages"]:
                content = m.get("content")
                if m["role"] == "system" and isinstance(content, str):
                    for name, marker in MEMORY_MARKERS.items():
                        if marker in content:
                            sizes[name] += len(content)
            return sizes
        sizes = [len("".join(parts(body))) for _, body in order]
        report = {
            "visibility": {"correct": correct, "total": len(results), "probes": results},
            "prefix_reuse": {
                "steady_mean": round(statistics.mean(steady_ratios), 4),
                "steady_min": round(min(steady_ratios), 4),
                "per_session_mean": {k: round(statistics.mean(v), 4) for k, v in ratios.items() if v},
            },
            "context_chars": {
                "requests": len(sizes), "mean": round(statistics.mean(sizes)), "max": max(sizes),
                "memory_data_in_last_steady_request": memory_sizes(steady_bodies[-1]),
            },
        }
        line = json.dumps(report, ensure_ascii=False)
        print("EVE_MEMORY_EVAL " + line)
        if os.environ.get("EVE_MEMORY_EVAL_REPORT"):
            with open(os.environ["EVE_MEMORY_EVAL_REPORT"], "w", encoding="utf8") as f:
                json.dump(report, f, ensure_ascii=False, indent=2)
        self.assertEqual(correct, len(results), [r for r in results if r["expected"] != r["actual"]])
        self.assertGreaterEqual(report["prefix_reuse"]["steady_mean"], MIN_STEADY_PREFIX_REUSE)


REAL_BASE = os.environ.get("EVE_MEMORY_EVAL_BASE_URL", "").rstrip("/")
REAL_KEY = os.environ.get("EVE_MEMORY_EVAL_API_KEY")
REAL_MODEL = os.environ.get("EVE_MEMORY_EVAL_MODEL")


def cached_tokens(usage):
    """服务商报告的命中缓存的输入 token；OpenAI 与兼容实现的两种常见字段。"""
    details = usage.get("prompt_tokens_details") or {}
    return details.get("cached_tokens") or usage.get("prompt_cache_hit_tokens") or 0


@unittest.skipUnless(REAL_BASE and REAL_KEY and REAL_MODEL,
                     "真实模型模式需要 EVE_MEMORY_EVAL_BASE_URL、EVE_MEMORY_EVAL_API_KEY、EVE_MEMORY_EVAL_MODEL")
class RealModelEvaluation(people_test.PeopleAcceptance):
    for _name in dir(people_test.PeopleAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.calls = []
        # 替身自己的应答在转发前就会生成，随后被真实服务的应答替换。
        self.impression_reply = lambda batch: {"impressions": []}
        self.profile_reply = lambda request: {"facts": []}

    def provider_response(self, body, kind, fallback):
        # 每个请求（对话、印象整理等）都原样交给真实服务，只替换模型名。
        forwarded = dict(body, model=REAL_MODEL)
        request = urllib.request.Request(
            REAL_BASE + "/chat/completions", data=json.dumps(forwarded).encode(),
            headers={"Content-Type": "application/json", "Authorization": f"Bearer {REAL_KEY}"})
        started = time.monotonic()
        with urllib.request.urlopen(request, timeout=180) as response:
            answer = json.loads(response.read())
        messages = body["messages"]
        current = max((i for i, m in enumerate(messages) if m["role"] == "user"), default=-1)
        self.calls.append({"kind": kind, "text": messages[current]["content"] if current >= 0 else "",
                           "after_tool": any(m["role"] == "tool" for m in messages[current + 1:]),
                           "usage": answer.get("usage") or {}, "seconds": round(time.monotonic() - started, 2)})
        return answer

    def replies(self):
        events = []
        for path in sorted(self.work.glob("events-*.jsonl")):
            events += [json.loads(line) for line in path.read_text(encoding="utf8").splitlines()]
        return {e["id"]: e.get("text", "") for e in events if e.get("direction") == "out" and e.get("type") == "reply"}

    def test_real_model_uses_cross_session_memory_and_reports_cache_hits(self):
        group_talk = ["我最近在学Rust，周末想写个小工具", "我养了一只橘猫", "今天好累啊我", "我下周要去成都出差"]
        steady = ["今天天气怎么样", "你平时喜欢干嘛", "推荐个电影吧", "有点无聊", "说个冷笑话", "好吧那我去忙了"]

        def say(mid, text, user="user-1", target="group-1"):
            return self.send({**self.message(mid, text, user=user, scope="group", target=target), "accept_any": True})

        def learned():
            # 真实模型可能不整理出印象；最多等一分钟，不因此失败。
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                people = self.people_state()["people"]
                if any(p.get("impressions") for p in people):
                    return
                time.sleep(1)
        self.enabled([*[s for i, t in enumerate(group_talk) for s in say(f"a{i}", t)],
                      *say("promise", "下次帮我看看我做的 Mindustry 地图吧"),
                      *self.checkpoint("learned"),
                      *say("recall", "你还记得我在学什么吗？", target="group-2"),
                      *say("promised", "你之前答应过我什么事来着？", target="group-2"),
                      *say("other", "群里有人在学什么吗？", user="user-2", target="group-3"),
                      *[s for i, t in enumerate(steady) for s in say(f"s{i}", t)]],
                     checkpoints={"learned": learned}, timeout=900,
                     env_extra={"EVE_OPENAI_MODEL": REAL_MODEL})
        replies = self.replies()
        people = json.dumps(self.people_state(), ensure_ascii=False)
        commitments = commitment_test.CommitmentAcceptance.ledger(self)["commitments"]
        chat = [c for c in self.calls if c["kind"] == "chat"]
        steady_calls = [c for c in chat if c["text"] in steady and not c["after_tool"]]

        def ratio(calls):
            prompt = sum(c["usage"].get("prompt_tokens", 0) for c in calls)
            return round(sum(cached_tokens(c["usage"]) for c in calls) / prompt, 4) if prompt else None
        report = {
            "model": REAL_MODEL,
            "commitment_tool_used": any("地图" in c["text"] for c in commitments),
            "impression_learned": "Rust" in people or "rust" in people.lower(),
            "recalls_learning_in_other_group": "rust" in replies.get("recall", "").lower(),
            "recalls_promise_in_other_group": "地图" in replies.get("promised", ""),
            "no_leak_to_other_person": "rust" not in replies.get("other", "").lower(),
            "cache_hit_ratio": {"steady_chat": ratio(steady_calls), "all_chat": ratio(chat),
                                "all_requests": ratio(self.calls)},
            "requests": {"chat": len(chat), "total": len(self.calls),
                         "mean_seconds": round(statistics.mean(c["seconds"] for c in self.calls), 2)},
            "replies": {k: replies.get(k, "") for k in ("recall", "promised", "other")},
        }
        line = json.dumps(report, ensure_ascii=False)
        print("EVE_MEMORY_EVAL_REAL " + line)
        if os.environ.get("EVE_MEMORY_EVAL_REPORT"):
            with open(os.environ["EVE_MEMORY_EVAL_REPORT"], "w", encoding="utf8") as f:
                json.dump(report, f, ensure_ascii=False, indent=2)
        # 只有隐私边界是硬性要求；其余是模型表现，按报告比较。
        self.assertTrue(report["no_leak_to_other_person"], report["replies"]["other"])


if __name__ == "__main__":
    unittest.main()
