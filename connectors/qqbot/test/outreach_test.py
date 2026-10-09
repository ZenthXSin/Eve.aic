"""实际 eve-qqbot 的主动交流验收；连接本地模型替身、fake bridge 与 Mindustry 服务端替身。

用户只在私聊里提一次兴趣：学习目标实践验证通过后，Eve 依据账本中的事实撰写一条邀请；
用户下次私聊找它时，先由时机判断器看过这条消息与回复，合适才随被动回复附带，以平台回执为准；
操作者开启时，等待一段时间仍未送达则主动私聊一次。要求安静、撤回兴趣、平台拒绝与进程中断
都有明确且不重放的行为。

全部使用确定性替身，只证明宿主契约、投递时机与恢复语义，不代表真实模型的撰写质量，
也不是正式 QQ 的主动消息验收（平台配额与用户开关只能在正式环境验证）。
"""
import json
import os
import subprocess
import unittest

import practice_test
from interest_test import MINDUSTRY, QUIT
from memory_test import BINARY, ROOT

DISABLED = "主动交流未启用。"
CHAT = "今天天气不错，随便聊聊。"
LATER = "晚上吃什么好呢？"
QUOTE = "我喜欢 Mindustry 这个游戏的模组"


def base():
    return practice_test.PracticeAcceptance


def invitation_text(quote):
    return f"你上次说“{quote}”，我后来自己试着做了一个新方块，已经在游戏里实际跑通了。你有什么想加进去的东西？告诉我，我们一起做。"


class OutreachAcceptance(unittest.TestCase):
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
        self.compose = self.default_compose
        self.judge = self.default_judge

    def tearDown(self):
        base().tearDown(self)

    def run_eve(self, script, outreach=True, proactive_after=None, **options):
        more = ["--outreach"] if outreach else []
        if proactive_after is not None:
            more += ["--outreach-proactive-after-ms", str(proactive_after)]
        options.setdefault("research", False)
        return base().run_eve(self, script, more=more, **options)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "invitation_id" in decoded and "facts" in decoded:
            return "compose"
        if isinstance(decoded, dict) and "judge_version" in decoded and "user_message" in decoded:
            return "judge"
        return base().classify(self, decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "compose":
            return json.dumps(self.compose(decoded), ensure_ascii=False)
        if kind == "judge":
            return json.dumps(self.judge(decoded), ensure_ascii=False)
        return base().reply(self, kind, decoded, latest)

    # 确定性撰写替身：只引用请求里的用户原话。
    def default_compose(self, request):
        quote = next(fact["text"] for fact in request["facts"] if fact["kind"] == "UserQuote")
        return {"text": invitation_text(quote)}

    # 确定性时机判断替身：用户在说不再感兴趣时不附带，其余时候附带。
    def default_judge(self, request):
        return {"decision": "not_now" if request["user_message"] == QUIT else "invite"}

    def ledger(self):
        return self.documents().get("eve.outreach", {}).get("outreach.v1") or {"invitations": [], "preferences": []}

    def invitations(self):
        return self.ledger()["invitations"]

    def status(self, wanted):
        # 带原因的状态序列化为 {"Cancelled": "GoalClosed"}；按状态名比较。
        def name(status):
            return next(iter(status)) if isinstance(status, dict) else status
        return lambda: any(name(invitation["status"]) == wanted for invitation in self.invitations())

    def wait_status(self, wanted, seconds=25):
        self.wait_for(self.status(wanted), f"invitation did not reach {wanted}", seconds)

    def receipt(self, message_id):
        receipts = self.documents().get("eve.channel.qqbot", {}).get("receipts.v1") or {"entries": []}
        return next(entry for entry in receipts["entries"] if entry["message"]["id"] == message_id)

    def pushes(self):
        events = []
        for path in sorted(self.work.glob("events-*.jsonl")):
            events += [json.loads(line) for line in path.read_text(encoding="utf8").splitlines()]
        return [event for event in events if event.get("direction") == "out" and event.get("type") == "push"]

    def with_invitation(self, id, text):
        return self.message(id, text, expected=None, contains=None) | {
            "expected_segments": [text, invitation_text(QUOTE)]}

    # 附带邀请的回复以两段被动回复发出：原回复在前，邀请在后。
    def send_invited(self, id, text):
        return [{"send": self.with_invitation(id, text)},
                {"wait_command": {"id": id, "type": "segment", "count": 2}}, self.wait_receipt(id)]

    def test_invitation_rides_the_next_private_reply_with_a_real_receipt_and_is_not_repeated(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("composed"),
                      *self.send_invited("chat", CHAT),
                      *self.send(self.message("later", LATER)),
                      *self.checkpoint("quiet")],
                     checkpoints={"composed": lambda: self.wait_status("Pending"), "quiet": self.quiet},
                     max_executions=4)
        [invitation] = self.invitations()
        self.assertEqual(invitation["status"], "Delivered")
        [practice] = self.practice_runs()
        self.assertEqual(invitation["goal_id"], practice["task"]["goal_id"])
        self.assertEqual(invitation["milestone"]["practice_run_id"], practice["id"])
        self.assertEqual(invitation["owner"], practice["task"]["owner"])
        [judgement] = invitation["judgements"]
        self.assertEqual((judgement["message_id"], judgement["outcome"]), ("chat", {"verdict": "Invite"}))
        [judge] = self.kind("judge")
        judged = json.loads(judge["body"]["messages"][-1]["content"])
        self.assertEqual((judged["user_message"], judged["reply"]), (CHAT, CHAT))
        self.assertEqual(judge["state"]["eve.outreach"]["outreach.v1"]["invitations"][0]["judgements"][0]["outcome"],
                         None, "判断前已保存")
        self.assertFalse(judge["body"].get("tools"))
        for domain in ("Mindustry", "模组", "游戏"):
            self.assertNotIn(domain, judge["body"]["messages"][0]["content"], "判断规则与领域无关")
        [attempt] = invitation["attempts"]
        self.assertEqual(attempt["channel"], {"passive": {"message_id": "chat"}})
        self.assertEqual(attempt["result"], {"sent": {"platform_message_id": "out-chat-1"}})
        # QQ 回执中邀请是同一条消息被动回复的最后一段，原回复不变。
        parts = self.receipt("chat")["segments"]["parts"]
        self.assertEqual([part["state"] for part in parts], ["Sent", "Sent"])
        self.assertEqual(self.receipt("later").get("segments"), None, "同一学习目标只邀请一次")
        # 送达的邀请作为数据交给后续对话：用户接下来的话可能是在回应它。
        later = next(r for r in self.kind("chat") if r["body"]["messages"][-1]["content"] == LATER)
        context = json.dumps(later["body"]["messages"][:-1], ensure_ascii=False)
        self.assertIn("eve.outreach.delivered", context)
        self.assertIn(invitation_text(QUOTE), context)

        # 撰写请求只含账本事实，发出前已保存 Composing；规则与领域无关、不安装工具。
        [request] = self.kind("compose")
        self.assertEqual(request["state"]["eve.outreach"]["outreach.v1"]["invitations"][0]["status"], "Composing")
        self.assertFalse(request["body"].get("tools"))
        facts = json.loads(request["body"]["messages"][-1]["content"])["facts"]
        self.assertIn({"kind": "UserQuote", "text": QUOTE}, facts)
        progress = next(fact["text"] for fact in facts if fact["kind"] == "Progress")
        self.assertIn("build 160.7", progress)
        self.assertIn("eve-sample-sample-wall.class=Wall", progress)
        rules = request["body"]["messages"][0]["content"]
        for domain in ("Mindustry", "模组", "游戏"):
            self.assertNotIn(domain, rules, "撰写规则不能按领域关键词分支")
        self.assertEqual(self.pushes(), [], "未开启时从不主动私聊")

        before = self.ledger()
        self.run_eve([
            *self.send(self.message("status", "/outreach", contains=[
                "主动邀请：开启", "已送达", "随你的消息回复附带，平台已确认（消息 out-chat-1）", "内容：你上次说"])),
            *self.send(self.message("other", "/outreach", user="user-2", contains="还没有邀请")),
            *self.send(self.message("group", LATER, scope="group", target="group-1")),
            *self.checkpoint("idle")], checkpoints={"idle": self.quiet})
        self.assertEqual(len(self.kind("compose")), 1, "restart must not replay composing")
        self.assertEqual(self.ledger(), before)

    def test_quiet_holds_the_invitation_until_the_user_turns_outreach_back_on(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("composed"),
                      *self.send(self.message("off", "/outreach off", contains="不会再主动提起")),
                      *self.send(self.message("chat", CHAT)),
                      *self.send(self.message("status", "/outreach", contains=["主动邀请：已关闭", "等待合适的时机"])),
                      *self.send(self.message("on", "/outreach on", contains="合适的时候告诉你")),
                      *self.send_invited("later", LATER),
                      *self.checkpoint("quiet")],
                     checkpoints={"composed": lambda: self.wait_status("Pending"), "quiet": self.quiet},
                     max_executions=4)
        [invitation] = self.invitations()
        self.assertEqual(invitation["status"], "Delivered")
        self.assertEqual([attempt["channel"] for attempt in invitation["attempts"]],
                         [{"passive": {"message_id": "later"}}])
        self.assertEqual(self.ledger()["preferences"][0]["quiet"], False)

    def test_proactive_push_is_tried_once_and_a_refusal_keeps_the_invitation_pending(self):
        refused = {"ok": False, "http_status": 400, "biz_code": 22009}
        self.run_eve([{"push_mode": refused}, *self.send(self.message("casual", MINDUSTRY)),
                      *self.checkpoint("refused"),
                      *self.send_invited("chat", CHAT),
                      *self.checkpoint("quiet")],
                     checkpoints={"refused": lambda: self.wait_for(
                         lambda: any(invitation["attempts"] for invitation in self.invitations()),
                         "proactive push was not attempted", 25),
                                  "quiet": self.quiet},
                     proactive_after=0, max_executions=4)
        [invitation] = self.invitations()
        self.assertEqual(invitation["status"], "Delivered")
        proactive, passive = invitation["attempts"]
        self.assertEqual(proactive["channel"], "proactive")
        self.assertEqual(proactive["result"], {"failed": {"http_status": 400, "biz_code": 22009}})
        self.assertEqual(passive["channel"], {"passive": {"message_id": "chat"}})
        [push] = self.pushes()
        self.assertEqual(push["target_id"], "user-1", "路由取自已确认的私聊回执")
        self.assertNotIn("msg_id", push)

    def test_proactive_push_delivers_with_a_platform_receipt(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("delivered"),
                      *self.send(self.message("chat", CHAT)),
                      *self.checkpoint("quiet")],
                     checkpoints={"delivered": lambda: self.wait_status("Delivered"), "quiet": self.quiet},
                     proactive_after=0, max_executions=4)
        [invitation] = self.invitations()
        [attempt] = invitation["attempts"]
        self.assertEqual(attempt["channel"], "proactive")
        self.assertEqual(attempt["result"], {"sent": {"platform_message_id": "push-" + invitation["id"]}})
        [push] = self.pushes()
        self.assertEqual(push["text"], invitation_text(QUOTE))

    def test_withdrawn_interest_cancels_the_pending_invitation(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("composed"),
                      *self.send(self.message("quit", QUIT)), *self.checkpoint("cancelled"),
                      *self.send(self.message("chat", CHAT)),
                      *self.checkpoint("quiet")],
                     checkpoints={"composed": lambda: self.wait_status("Pending"),
                                  "cancelled": lambda: self.wait_status("Cancelled"), "quiet": self.quiet},
                     max_executions=4)
        [invitation] = self.invitations()
        self.assertEqual(invitation["status"], {"Cancelled": "GoalClosed"})
        self.assertEqual(invitation["attempts"], [])
        # 说不再感兴趣的那条消息，判断为此刻不合适，邀请没有附带。
        self.assertEqual([(j["message_id"], j["outcome"]) for j in invitation["judgements"]],
                         [("quit", {"verdict": "NotNow"})])
        self.assertIsNone(self.receipt("quit").get("segments"))
        self.assertEqual(len(self.kind("judge")), 1, "取消后不再判断")

    def test_restart_while_the_invitation_is_in_flight_is_unknown_and_never_resent(self):
        sending = self.gate("invitation-sending")
        in_flight = self.with_invitation("chat", CHAT) | {"hold_segments": [1]}
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("composed"),
                      {"send": in_flight},
                      {"wait_part": {"path": str(self.state_path), "id": "chat", "index": 1, "state": "Sending"}},
                      {"touch": str(sending)}, {"wait_file": str(self.gate("never-released"))}],
                     checkpoints={"composed": lambda: self.wait_status("Pending")},
                     stop_at=sending, max_executions=4)
        self.assertEqual(self.invitations()[0]["status"], "Delivering")
        self.run_eve([*self.send(self.message("later", LATER)), *self.checkpoint("quiet")],
                     checkpoints={"quiet": self.quiet})
        [invitation] = self.invitations()
        self.assertEqual(invitation["status"], "Unknown")
        self.assertEqual(invitation["attempts"][0]["result"], "unknown")
        self.assertIsNone(invitation["attempts"][0]["finished_at_ms"])
        self.assertEqual(self.receipt("later").get("segments"), None, "结果未知的邀请不重发")

    def test_outreach_is_disabled_by_default_and_requires_a_practice_runtime(self):
        self.run_eve([*self.send(self.message("status", "/outreach", expected=DISABLED)),
                      *self.checkpoint("quiet")], outreach=False, checkpoints={"quiet": self.quiet})
        self.assertNotIn("eve.outreach", self.documents())
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret")
        for extra, expected in ((["--interest-learning", "--outreach"], "--practice-mindustry-server"),
                                (["--outreach-proactive-after-ms", "0"], "--outreach")):
            result = subprocess.run([str(BINARY), "--state-dir", str(self.work / "refused"),
                                     "--agent", str(ROOT / "AGENT.md"), *extra],
                                    env=env, capture_output=True, text=True, timeout=60)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)


if __name__ == "__main__":
    unittest.main()
