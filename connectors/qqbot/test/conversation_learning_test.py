"""真实 QQ/Web 进程的连续交流学习、应用、纠错和恢复。

模型及平台用确定性替身，证明生产调用链与状态变化；不据此宣称真人主观感受已达标。
"""
import importlib.util
import json
from pathlib import Path
import sys
import time
import unittest

from group_chat_test import GroupChatAcceptance
from dialogue_tools_test import text_response
from memory_test import MemoryAcceptance

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location("web_learning_fixture", ROOT / "scripts/test-web-autonomy.py")
web_fixture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(web_fixture)

SITUATION = "对方只是分享疲惫而未求助时"
RESPONSE = "先回应眼下经历，陪对方聊两句"
AVOID = "明确求助时仍给具体帮助；不把所有短消息都解释成疲惫"


def lesson(batch, assessment, indices, lesson_id=None):
    return {"lesson_id": lesson_id, "situation": SITUATION, "response": RESPONSE,
            "avoid": AVOID, "assessment": assessment,
            "evidence": [{"message_id": batch["messages"][i]["id"],
                          "quote": batch["messages"][i]["text"]} for i in indices]}


class WebConversationLearning(web_fixture.WebAutonomyAcceptance):
    for _name in dir(web_fixture.WebAutonomyAcceptance):
        if _name.startswith("test_"): locals()[_name] = None
    del _name

    def lessons(self):
        return [lesson for scope in self.documents().get("eve.training", {}).get("conversation.v1", {}).get("scopes", [])
                for lesson in scope["lessons"]]

    def reply(self, kind, decoded, latest):
        if kind == "conversation_learning":
            self.assertTrue(all(m["role"] in ("human", "eve") for m in decoded["messages"]))
            if decoded["lessons"]:
                return json.dumps({"lessons": []})
            return json.dumps({"lessons": [lesson(decoded, "correction", [1, 2])]}, ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def test_correction_learns_without_commands_applies_to_new_topic_and_survives_restart(self):
        replies = {"今天跑了一整天，有点累": "建议你列个休息计划，要我帮你安排吗？",
                   "不用帮我安排，就陪我吐槽两句，别每次都提建议": "这一天够折腾的，先歇会儿"}

        def respond(body):
            current = self.current_text(body)
            if current in replies: return text_response(replies[current])
            context = json.dumps(body, ensure_ascii=False)
            if current == "另一个会话不套用我的习惯":
                self.assertNotIn("eve.conversation-lessons-v1", context)
                return text_response("按当前情况聊")
            self.assertIn(RESPONSE, context)
            self.assertIn(AVOID, context)
            return text_response("今天也够折腾的")

        self.callback = respond
        self.start_web()
        self.send_and_finish(list(replies)[0], 1)
        self.send_and_finish(list(replies)[1], 2)
        self.wait_until(lambda: self.lessons() and self.lessons()[0]["active"], "真实交流没有形成可用经验")
        self.assertEqual(self.lessons()[0]["corrections"], 1)
        self.assertEqual(self.send_and_finish("今天会议排满了", 3)["text"], "今天也够折腾的")
        self.send_and_finish("另一个会话不套用我的习惯", 4, conversation="c" * 32)
        self.stop_web(graceful=True)
        self.start_web()
        self.assertEqual(self.lessons()[0]["corrections"], 1)
        self.assertEqual(self.send_and_finish("今天通勤也很折腾", 5)["text"], "今天也够折腾的")


class GroupConversationLearning(GroupChatAcceptance):
    for _name in dir(GroupChatAcceptance):
        if _name.startswith("test_"): locals()[_name] = None
    del _name

    def learned(self):
        return [s for s in self.documents().get("eve.training", {}).get("conversation.v1", {}).get("scopes", [])
                if s["lessons"]]

    def reply(self, kind, decoded, latest):
        if kind == "conversation_learning":
            self.assertTrue(all(m["role"] == "human" for m in decoded["messages"]))
            ids = [i for i, m in enumerate(decoded["messages"]) if "累" in m["text"] or "歇" in m["text"]]
            old = decoded["lessons"][0]["id"] if decoded["lessons"] else None
            return json.dumps({"lessons": [lesson(decoded, "observation", ids[-2:], old)]}, ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def test_watching_other_people_learns_without_reply_then_informs_current_group_only(self):
        def pending():
            self.wait_for(lambda: self.learned(), "旁观交流没有候选", 20)
            self.assertFalse(self.learned()[0]["lessons"][0]["active"])
            status, readiness = MemoryAcceptance.panel(self, "/api/readiness")
            self.assertEqual(status, 200)
            role = next(r for r in readiness["roles"] if r["role"] == "conversation_learning")
            self.assertGreaterEqual(role["requests"], 1)
            self.assertEqual(role["failures"], 0)

        def active():
            self.wait_for(lambda: self.learned() and self.learned()[0]["lessons"][0]["active"], "独立新证据没有确认经验", 20)

        first = [self.observation("a1", "今天跑了一整天，有点累", user="member-2"),
                 self.observation("a2", "这一天够折腾的，先歇会儿", user="member-3"),
                 self.observation("a3", "这边终于结束了", user="member-2"),
                 self.observation("a4", "那就早点回去", user="member-3")]
        second = [self.observation("b1", "今天也是跑一天，好累", user="member-2"),
                  self.observation("b2", "先歇会儿，真够累的", user="member-3"),
                  self.observation("b3", "刚刚坐下", user="member-2"),
                  self.observation("b4", "喘口气吧", user="member-3")]
        self.run_eve([*first, *self.checkpoint("pending"), *second, *self.checkpoint("active")], checkpoints={"pending":pending, "active":active}, panel=True)
        self.assertEqual(self.pushes(), [], "学习他人交流不应制造回复")
        self.assertEqual(self.kind("chat"), [], "旁观不伪造一次与 Eve 的对话")
        self.assertEqual(self.learned()[0]["lessons"][0]["observations"], 2)
        def respond(body):
            self.assertIn(RESPONSE, json.dumps(body, ensure_ascii=False))
            self.assertIn("不是当前用户个人偏好", json.dumps(body, ensure_ascii=False))
            return text_response("今天也挺折腾")
        self.callback = respond
        self.run_eve([self.observation("now", "今天会议排满了", mentioned=True),
                      *self.send(self.message("now", "今天会议排满了", scope="group", expected="今天也挺折腾"))])


if __name__ == "__main__":
    suite = unittest.TestSuite(unittest.defaultTestLoader.loadTestsFromTestCase(case)
                               for case in (WebConversationLearning, GroupConversationLearning))
    sys.exit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
