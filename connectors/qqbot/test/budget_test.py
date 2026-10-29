"""实际 eve-qqbot 的自主运行预算验收；只连接本地模型替身和 fake bridge。

后台兴趣观察的每次模型请求先把计数保存到预算账本，再发出请求；余额不够时后台不开始新的观察，
证据留在记忆中，重启不清零，调高上限后从剩余额度继续。回复用户消息不计入。账本损坏时拒绝启动并保留原字节。
确定性替身只证明宿主契约与恢复语义。
"""
from fixture_auth import configure_fixture_progress
import json
import os
import subprocess
import unittest

import interest_test
from interest_test import MINDUSTRY, WATERCOLOR
from memory_test import BINARY, FAKE, ROOT

DISABLED = "自主运行预算未启用"
ASK = "后台预算还剩多少？"
# 30 天窗口按 Unix 纪元对齐，测试期间几乎不会跨窗口。
WINDOW = ["--autonomy-budget-window-hours", "720"]


def base():
    return interest_test.InterestAcceptance


def tool_names(body):
    return {entry["function"]["name"] for entry in body.get("tools", [])}


def tool_results(body):
    messages = body["messages"]
    current = max(index for index, message in enumerate(messages) if message["role"] == "user")
    return [json.loads(m["content"]) for m in messages[current + 1:] if m["role"] == "tool"]


def tool_call(name):
    return {"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
        "role": "assistant", "content": None, "tool_calls": [
            {"id": f"call-{name}", "type": "function", "function": {"name": name, "arguments": "{}"}}]}}]}


def text(content):
    return {"choices": [{"index": 0, "finish_reason": "stop", "message": {
        "role": "assistant", "content": content}}]}


class BudgetAcceptance(unittest.TestCase):
    # 复用兴趣验收的进程、模型替身与持久状态读取，不继承其用例。
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "classify", "reply", "default_observe", "interest_state", "jobs", "interests",
                  "cognition", "goals", "learning_goal", "reflection", "kind", "quiet", "run_eve",
                  "inspect_run"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)

    def tearDown(self):
        base().tearDown(self)

    def ledger(self):
        return self.documents().get("eve.budget", {}).get("usage.v1")

    def budget(self, limit):
        return ["--autonomy-budget", str(limit), *WINDOW]

    def topics(self):
        return sorted(interest["topic"] for interest in self.interests())

    def test_each_observation_is_counted_before_its_request_and_exhaustion_survives_restart(self):
        def observed():
            return self.topics() == ["Mindustry 模组创作"]

        def exhausted():
            # 另一位用户的聊天已保存为证据；余额用尽，不开始新的观察。
            self.wait_until(lambda: len(self.interactions()) == 2, "second interaction was not saved")
            self.quiet()

        self.run_eve([
            *self.send(self.message("casual", MINDUSTRY)),
            *self.checkpoint("observed"),
            *self.send(self.message("other", WATERCOLOR, user="user-2")),
            *self.checkpoint("exhausted"),
            *self.send(self.message("status", "/budget", contains=[
                "每 720 小时至多 1 次", "已用 1 次、剩余 0 次", "兴趣观察 1", "余额用尽", "回复用户消息不计入"]))],
            extra=self.budget(1), checkpoints={
                "observed": lambda: self.wait_until(observed, "first interest was not observed"),
                "exhausted": exhausted})
        [observation] = self.kind("interest")
        saved = observation["state"]["eve.budget"]["usage.v1"]
        self.assertEqual(saved["current"]["used"], 1, "请求前已保存计数")
        self.assertEqual(saved["current"]["by_category"], {"interest": 1})
        self.assertEqual(self.topics(), ["Mindustry 模组创作"])
        self.assertEqual(len(self.kind("chat")), 2, "回复用户消息不受预算限制")
        ledger = self.ledger()
        self.assertEqual((ledger["policy"], ledger["current"]["used"]),
                         ({"window_ms": 720 * 3_600_000, "limit": 1}, 1))

        # 重启不清零：同一上限下仍不观察。
        self.inspect_run(self.quiet, extra=self.budget(1))
        self.assertEqual(len(self.kind("interest")), 1)

        # 调高上限：从剩余额度继续，观察此前保留的证据；计数仍先于请求保存。
        self.inspect_run(lambda: self.wait_until(lambda: len(self.topics()) == 2, "second interest was not observed"),
                         extra=self.budget(2))
        self.assertEqual(self.topics(), ["Mindustry 模组创作", "水彩画调色"])
        second = self.kind("interest")[1]
        self.assertEqual(second["state"]["eve.budget"]["usage.v1"]["current"]["used"], 2)
        self.assertEqual(self.ledger()["current"]["by_category"], {"interest": 2})
        self.run_eve([*self.send(self.message("again", "/budget", contains=["已用 2 次、剩余 0 次", "兴趣观察 2"])),
                      *self.checkpoint("quiet-full")], extra=self.budget(2), checkpoints={"quiet-full": self.quiet})
        self.assertEqual(len(self.kind("interest")), 2)

    def test_chat_answers_budget_questions_with_the_read_only_tool(self):
        seen = []

        def asked(body, kind, fallback):
            if kind != "chat":
                return fallback
            results = tool_results(body)
            if not results:
                self.assertIn("show_budget", tool_names(body))
                return tool_call("show_budget")
            seen.append(results[0])
            return text(f"后台预算本窗口还剩 {results[0]['remaining']} 次")

        self.provider_response = asked
        self.run_eve([*self.send(self.message("ask", ASK, contains=["后台预算本窗口还剩"]))],
                     extra=self.budget(3))
        [result] = seen
        self.assertEqual((result["limit"], result["window_ms"]), (3, 720 * 3_600_000))
        self.assertEqual(result["used"] + result["remaining"], 3)
        self.assertGreater(result["resets_in_minutes"], 0)
        self.assertLessEqual(set(result["by_category"]), {"interest"})
        # 查看不记账也不写状态：账本只可能来自这条消息引起的至多一次兴趣观察。
        ledger = self.ledger()
        self.assertTrue(ledger is None or ledger["current"]["used"] <= 1, ledger)

        # 未开启预算时不注册该工具，模型也就无从声称看到了预算。
        offered = []

        def plain(body, kind, fallback):
            if kind == "chat":
                offered.append(tool_names(body))
            return fallback

        self.provider_response = plain
        self.run_eve([*self.send(self.message("ask-off", ASK))])
        self.assertTrue(offered)
        self.assertTrue(all("show_budget" not in names for names in offered))

    def test_disabled_budget_is_explicit_and_corrupt_ledger_refuses_to_start_and_is_preserved(self):
        self.run_eve([*self.send(self.message("off", "/budget", contains=DISABLED))])
        self.assertIsNone(self.ledger())
        self.run_eve([*self.send(self.message("on", "/budget", contains="已用 0 次、剩余 3 次"))],
                     extra=self.budget(3))
        state = json.loads(self.state_path.read_text())
        state["entries"].setdefault("eve.budget", {})["usage.v1"] = list(
            b'{"format_version":1,"format_version":1}')
        self.state_path.write_text(json.dumps(state))
        before = self.state_path.read_bytes()
        scenario = self.work / "refused.json"
        scenario.write_text(json.dumps({"script": [], "events_file": str(self.work / "refused.jsonl"),
                                        "error_file": str(self.work / "refused-error.txt")}))
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID="1904159860",
                   EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        child = subprocess.run(
            [str(BINARY), "--state-dir", str(self.work / "state"), "--agent", str(ROOT / "AGENT.md"),
             "--bridge-script", str(FAKE), "--bridge-arg", str(scenario), "--interest-learning",
             *self.budget(3)], env=configure_fixture_progress(env), capture_output=True, text=True, timeout=30)
        self.assertNotEqual(child.returncode, 0)
        self.assertIn("自主运行预算记录损坏", child.stderr)
        self.assertNotIn("test-model-secret", child.stdout + child.stderr)
        self.assertEqual(self.state_path.read_bytes(), before, "损坏的账本原样保留")


if __name__ == "__main__":
    unittest.main(verbosity=2)
