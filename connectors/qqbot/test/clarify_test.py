"""实际 eve-qqbot 的请求澄清验收；连接本地模型替身、fake bridge 与 Mindustry 服务端替身。

用户只在私聊里提一次兴趣：学习目标的实践尝试用尽仍未验证时，Eve 依据账本中的事实撰写一个具体问题；
用户下次私聊找它时，先由时机判断器看过这条消息与回复，合适才随被动回复附带，以平台回执为准；
之后用户的下一轮私聊记为回复。每个目标版本只问一次，撤回兴趣时取消尚未送达的问题，重启不重放。

全部使用确定性替身，只证明宿主契约、投递时机与恢复语义，不代表真实模型的提问质量。
"""
import json
import unittest

import practice_test
from interest_test import MINDUSTRY, QUIT
from practice_test import mod

DISABLED = "请求澄清未启用。"
CHAT = "今天天气不错，随便聊聊。"
ANSWER = "想做一个会发光的墙，颜色是蓝色。"
LATER = "晚上吃什么好呢？"
QUOTE = "我喜欢 Mindustry 这个游戏的模组"
QUESTION = "我照着你说的兴趣试做了几次，都没在游戏里跑通。你最想让这个方块有什么效果？"


def base():
    return practice_test.PracticeAcceptance


class ClarifyAcceptance(unittest.TestCase):
    # 复用实践验收的进程、模型替身、服务端替身与持久状态读取；只覆盖分类、应答与启动参数。
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "inspect_run", "interest_state", "interests", "cognition", "goals", "learning_goal",
                  "kind", "quiet", "cognition_user", "default_observe", "observe_three_domains",
                  "default_select", "default_extract", "knowledge", "research_runs", "default_draft",
                  "practice", "practice_runs", "launches", "practiced", "wait_for"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)
        # 草稿一直拼错类型：三次尝试都实际运行但不能验证，目标在当前版本卡住。
        self.draft = lambda request: mod("Walll")
        self.judge = lambda request: {"decision": "not_now" if request["user_message"] == QUIT else "invite"}

    def tearDown(self):
        base().tearDown(self)

    def run_eve(self, script, clarify=True, **options):
        more = ["--clarify"] if clarify else []
        options.setdefault("research", False)
        options.setdefault("max_executions", 4)
        return base().run_eve(self, script, more=more, **options)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "request_id" in decoded and "stall" in decoded:
            return "clarify"
        if isinstance(decoded, dict) and "judge_version" in decoded and "user_message" in decoded:
            return "judge"
        return base().classify(self, decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "clarify":
            return json.dumps({"question": QUESTION}, ensure_ascii=False)
        if kind == "judge":
            return json.dumps(self.judge(decoded), ensure_ascii=False)
        return base().reply(self, kind, decoded, latest)

    def ledger(self):
        return self.documents().get("eve.clarify", {}).get("clarify.v1") or {"requests": [], "preferences": []}

    def clarifications(self):
        return self.ledger()["requests"]

    def status(self, wanted):
        # 带原因的状态序列化为 {"Cancelled": "GoalClosed"}；按状态名比较。
        def name(status):
            return next(iter(status)) if isinstance(status, dict) else status
        return lambda: any(name(request["status"]) == wanted for request in self.clarifications())

    def wait_status(self, wanted, seconds=25):
        self.wait_for(self.status(wanted), f"clarification did not reach {wanted}", seconds)

    def receipt(self, message_id):
        receipts = self.documents().get("eve.channel.qqbot", {}).get("receipts.v1") or {"entries": []}
        return next(entry for entry in receipts["entries"] if entry["message"]["id"] == message_id)

    # 附带问题的回复以两段被动回复发出：原回复在前，问题在后。
    def send_asked(self, id, text):
        message = self.message(id, text, expected=None, contains=None) | {"expected_segments": [text, QUESTION]}
        return [{"send": message}, {"wait_command": {"id": id, "type": "segment", "count": 2}},
                self.wait_receipt(id)]

    def test_a_stalled_goal_asks_once_on_the_next_private_reply_and_records_the_answer(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("composed"),
                      *self.send_asked("chat", CHAT),
                      *self.send(self.message("answer", ANSWER)),
                      *self.checkpoint("answered"),
                      *self.send(self.message("later", LATER)),
                      *self.checkpoint("quiet")],
                     checkpoints={"composed": lambda: self.wait_status("Pending"),
                                  "answered": lambda: self.wait_status("Answered"), "quiet": self.quiet})
        [run] = self.practice_runs()
        self.assertEqual(run["status"], "Unverified")
        [request] = self.clarifications()
        self.assertEqual(request["status"], "Answered")
        self.assertEqual((request["goal_id"], request["goal_revision"]),
                         (run["task"]["goal_id"], run["task"]["goal_revision"]))
        self.assertEqual(request["stall"], {"kind": "practice_unverified", "run_id": run["id"]})
        self.assertEqual(request["owner"], run["task"]["owner"])
        # 撰写请求只含账本事实，发出前已保存 Composing；规则与领域无关、不安装工具。
        [compose] = self.kind("clarify")
        self.assertEqual(compose["state"]["eve.clarify"]["clarify.v1"]["requests"][0]["status"], "Composing")
        self.assertFalse(compose["body"].get("tools"))
        body = json.loads(compose["body"]["messages"][-1]["content"])
        self.assertEqual(body["stall"], "practice_unverified")
        self.assertIn({"kind": "UserQuote", "text": QUOTE}, body["facts"])
        attempt = next(fact["text"] for fact in body["facts"] if fact["kind"] == "Attempt")
        self.assertIn("实际做了 3 次", attempt)
        self.assertTrue(any(fact["kind"] == "Capability" for fact in body["facts"]))
        for domain in ("Mindustry", "模组", "游戏"):
            self.assertNotIn(domain, compose["body"]["messages"][0]["content"], "提问规则不能按领域分支")
        # 判断先保存再请求；附带在被判断为合适的那条消息的回复后，以平台回执为准。
        [judge] = self.kind("judge")
        judged = json.loads(judge["body"]["messages"][-1]["content"])
        self.assertEqual((judged["invitation"], judged["user_message"]), (QUESTION, CHAT))
        self.assertIsNone(judge["state"]["eve.clarify"]["clarify.v1"]["requests"][0]["judgements"][0]["outcome"])
        [attempt] = request["attempts"]
        self.assertEqual(attempt["message_id"], "chat")
        self.assertEqual(attempt["result"], {"sent": {"platform_message_id": "out-chat-1"}})
        self.assertEqual([part["state"] for part in self.receipt("chat")["segments"]["parts"]], ["Sent", "Sent"])
        # 回复是送达后的下一轮私聊；之后的消息不改变记录，也不再附带。
        self.assertEqual((request["answer"]["message_id"], request["answer"]["text"]), ("answer", ANSWER))
        self.assertEqual(self.receipt("later").get("segments"), None)
        self.assertEqual(len(self.kind("judge")), 1, "送达后不再判断")

        before = self.ledger()
        self.run_eve([
            *self.send(self.message("status", "/clarify", contains=[
                "实践尝试用尽仍未验证", "你已回复", f"问题：{QUESTION}", f"你的回复：{ANSWER}"])),
            *self.send(self.message("other", "/clarify", user="user-2", contains="还没有因为卡住而问过你")),
            *self.send(self.message("off", "/clarify off", contains="不再附带")),
            *self.send(self.message("help", "/clarify what", contains="用法：/clarify")),
            *self.checkpoint("idle")], checkpoints={"idle": self.quiet})
        self.assertEqual(len(self.kind("clarify")), 1, "重启不重放撰写")
        self.assertEqual(self.ledger()["requests"], before["requests"])
        self.assertTrue(self.ledger()["preferences"][0]["quiet"])

    def test_not_now_waits_and_withdrawing_the_interest_cancels_the_pending_question(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("composed"),
                      *self.send(self.message("quit", QUIT)),
                      *self.checkpoint("cancelled")],
                     checkpoints={"composed": lambda: self.wait_status("Pending"),
                                  "cancelled": lambda: self.wait_status("Cancelled")})
        [request] = self.clarifications()
        self.assertEqual(request["status"], {"Cancelled": "GoalClosed"})
        [judgement] = request["judgements"]
        self.assertEqual((judgement["message_id"], judgement["outcome"]), ("quit", "not_now"))
        self.assertEqual(request["attempts"], [], "不合适时不附带")
        self.assertEqual(self.receipt("quit").get("segments"), None)
        self.run_eve([*self.send(self.message("chat", CHAT)), *self.checkpoint("quiet")],
                     checkpoints={"quiet": self.quiet})
        self.assertEqual(len(self.kind("clarify")), 1)
        self.assertEqual(len(self.kind("judge")), 1, "取消后不再判断")

    def test_clarify_is_disabled_by_default(self):
        self.run_eve([*self.send(self.message("off", "/clarify", contains=DISABLED)),
                      *self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("quiet")],
                     clarify=False, checkpoints={"quiet": lambda: (
                         self.wait_for(self.practiced, "practice did not finish", 20), self.quiet())})
        self.assertEqual(self.kind("clarify"), [])
        self.assertIsNone(self.documents().get("eve.clarify"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
