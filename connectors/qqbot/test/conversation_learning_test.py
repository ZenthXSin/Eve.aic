"""真实 QQ/Web 进程的连续交流学习、应用、纠错和恢复。

模型及平台用确定性替身，证明生产调用链与状态变化；不据此宣称真人主观感受已达标。
"""
import importlib.util
from contextlib import closing
import json
from pathlib import Path
import sqlite3
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


def conversation_scopes(work):
    path = work / "state/training-expressions.sqlite3"
    if not path.exists():
        return []
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as db:
        return [dict(json.loads(checkpoint), scope={"session_id": session, "user_id": user})
                for session, user, checkpoint in db.execute(
                    "SELECT session_id,user_id,checkpoint FROM conversation_scopes "
                    "WHERE checkpoint IS NOT NULL ORDER BY session_id,user_id")]


def conversation_revisions(work, scope, lesson_id):
    path = work / "state/training-expressions.sqlite3"
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as db:
        return [json.loads(revision) for revision, in db.execute(
            "SELECT revision_json FROM conversation_revisions "
            "WHERE session_id=? AND user_id=? AND lesson_id=? ORDER BY sequence",
            (scope["session_id"], scope["user_id"], lesson_id))]


def completed_learning_batches(work, scope):
    path = work / "state/training-expressions.sqlite3"
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as db:
        return [json.loads(batch) for batch, in db.execute(
            "SELECT batch_json FROM conversation_batches "
            "WHERE session_id=? AND user_id=? AND outcome='completed' ORDER BY sequence",
            (scope["session_id"], scope["user_id"]))]


def lesson(batch, assessment, indices, lesson_id=None):
    return {"lesson_id": lesson_id, "situation": SITUATION, "response": RESPONSE,
            "avoid": AVOID, "assessment": assessment,
            "evidence": [{"message_id": batch["messages"][i]["id"],
                          "quote": batch["messages"][i]["text"]} for i in indices]}


class WebConversationLearning(web_fixture.WebAutonomyAcceptance):
    for _name in dir(web_fixture.WebAutonomyAcceptance):
        if _name.startswith("test_"): locals()[_name] = None
    del _name

    @staticmethod
    def learning_scope(conversation=web_fixture.CONVERSATION):
        return {"session_id": "web:chat:" + conversation, "user_id": web_fixture.WEB_OWNER}

    def lessons(self, conversation=web_fixture.CONVERSATION):
        return [lesson for record in conversation_scopes(self.work)
                if record["scope"] == self.learning_scope(conversation)
                for lesson in record["lessons"]]

    def lesson_revisions(self, lesson_id):
        return conversation_revisions(self.work, self.learning_scope(), lesson_id)

    def completed_batches(self):
        return completed_learning_batches(self.work, self.learning_scope())

    def reply(self, kind, decoded, latest):
        if kind == "conversation_learning":
            self.assertTrue(all(m["role"] in ("human", "eve") for m in decoded["messages"]))
            if hasattr(self, "learning_reply"):
                return json.dumps(self.learning_reply(decoded), ensure_ascii=False)
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

    def test_all_scenes_reach_requests_and_old_lesson_evolves_with_versions_across_restart(self):
        """经真实入站、批次、上下文与重启验证，不用关键词模拟主模型的语义能力。"""
        replies, proposals, expected_context = {}, {}, {}
        used_evidence = set()
        prompts, observed_context = [], {}
        number = 0

        def context_lessons(body):
            for message in body["messages"]:
                text = message.get("content")
                if message["role"] == "system" and isinstance(text, str) and text.startswith("memory: "):
                    if '"kind":"eve.conversation-lessons-v1"' in text:
                        return json.loads(text[text.index("{"):])["lessons"]
            return []

        def respond(body):
            current = self.current_text(body)
            if current in expected_context:
                learned = context_lessons(body)
                self.assertEqual({item["response"] for item in learned}, expected_context[current])
                self.assertTrue(all("evidence" not in item and "evaluated_message_ids" not in item
                                    for item in learned), "主对话只带原则，不复制训练原话")
                observed_context[current] = learned
                return text_response("按眼下的场景继续")
            self.assertIn(current, replies)
            return text_response(replies[current])

        def learn(batch):
            prompts.append(batch)
            self.assertLessEqual(len(json.dumps(batch, ensure_ascii=False,
                                                separators=(",", ":")).encode()), 65536)
            self.assertTrue(all("evaluated_message_ids" not in item for item in batch["lessons"]))
            human = next(message for message in reversed(batch["messages"]) if message["role"] == "human")
            plan = proposals.get(human["text"])
            if not plan:
                return {"lessons": []}
            candidate = {key: value for key, value in plan.items() if key != "sources"}
            if candidate["lesson_id"]:
                self.assertTrue(any(item["id"] == candidate["lesson_id"] for item in batch["lessons"]),
                                "更新必须收到同范围旧经验，而非只处理新文本")
            candidate["evidence"] = []
            for role, text in plan["sources"]:
                source = next(message for message in reversed(batch["messages"])
                              if message["role"] == role and message["text"] == text)
                candidate["evidence"].append({"message_id": source["id"], "quote": text})
            # 相同旧窗口或重试不另造经验；主机仍需以来源 ID 自己执行去重。
            if human["id"] in used_evidence:
                return {"lessons": []}
            used_evidence.add(human["id"])
            return {"lessons": [candidate]}

        def exchange(first, answer, feedback, situation, response, avoid, assessment="correction", old=None):
            nonlocal number
            replies[first], replies[feedback] = answer, "按当前反馈继续"
            proposals[feedback] = {"lesson_id": old, "situation": situation, "response": response,
                                   "avoid": avoid, "assessment": assessment,
                                   "sources": [("human", first), ("human", feedback)]
                                   if assessment == "observation" else [("eve", answer), ("human", feedback)]}
            number += 1
            self.send_and_finish(first, number)
            number += 1
            self.send_and_finish(feedback, number)
            self.wait_until(lambda: any(any(message["role"] == "human" and message["text"] == feedback
                                            for message in batch["messages"])
                                       for batch in self.completed_batches()),
                            "本组消息未完成真实持久批次")

        def probe_context(text, expected):
            nonlocal number
            expected_context[text] = expected
            number += 1
            self.send_and_finish(text, number)
            followup = f"{text}，按当前阶段继续"
            expected_context[followup] = expected
            number += 1
            self.send_and_finish(followup, number)
            self.wait_until(lambda: any(any(message["role"] == "human" and message["text"] == followup
                                            for message in batch["messages"])
                                       for batch in self.completed_batches()),
                            "上下文探针的消息未完成空提炼批次")

        def current_lesson(id):
            return next((item for item in self.lessons() if item["id"] == id), None)

        self.callback, self.learning_reply = respond, learn
        self.start_web()
        old_responses = set()
        for index in range(6):
            response = f"只在历史场景{index}里使用这条回应原则"
            exchange(f"历史场景{index}开始", f"历史场景{index}的旧表达",
                     f"场景{index}请按当前情况回应，不固定问候",
                     f"历史场景{index}的具体交流", response, "其他场景重新判断")
            old_responses.add(response)
            self.wait_until(lambda: old_responses <= {item["response"] for item in self.lessons()
                                                       if item["active"]}, "旧纠正没有进入持久经验")
        self.assertEqual(sum(item["corrections"] > 0 for item in self.lessons()), 6)

        scene = "已授权排查开始及等待结果时"
        response = "先自然回应再查证，有实际发现时再分条说"
        avoid = "需要完整交付或明确要安静时重新判断"
        exchange("排查开始时，可以先接一句再查", "我先看这一步",
                 "换成另一件事，也可以先说明打算再继续处理",
                 scene, response, avoid, "observation")
        self.wait_until(lambda: any(item["situation"] == scene for item in self.lessons()),
                        "新场景没有形成候选")
        target = next(item for item in self.lessons() if item["situation"] == scene)
        lesson_id = target["id"]
        self.assertFalse(target["active"])
        exchange("这次也可以先说一声，再查具体原因", "我再核对一下",
                 "等待时继续做事，有发现再说就好", scene, response, avoid, "observation", lesson_id)
        self.wait_until(lambda: current_lesson(lesson_id)["active"], "独立新证据没有支持同一经验")
        self.assertEqual(current_lesson(lesson_id)["observations"], 2)
        self.assertEqual(current_lesson(lesson_id)["corrections"], 0)
        all_responses = old_responses | {response}
        probe = "开始新的排查，用眼下的信息决定如何接话"
        probe_context(probe, all_responses)
        self.assertEqual(len(observed_context[probe]), 7, "六条旧纠正不能挤掉第七种场景")

        revised = "结果已确认后，完整说明结论和依据"
        exchange("现在排查已经结束", "我先回应一句，然后继续查",
                 "已经有结果了，别再说要查，直接把证据讲完整",
                 "排查结束且对方要求核验证据时", revised, "没有实际结果时不能冒称完成",
                 old=lesson_id)
        self.wait_until(lambda: current_lesson(lesson_id)["response"] == revised,
                        "新反馈没有更新原经验")
        self.assertEqual(len(self.lessons()), 7, "修正已有原则不得生成重复经验")
        self.assertEqual(current_lesson(lesson_id)["corrections"], 1)
        revisions_before = self.lesson_revisions(lesson_id)
        self.assertTrue(any(item["lesson"]["response"] == response for item in revisions_before))
        self.assertTrue(any(item["lesson"]["response"] == revised for item in revisions_before))
        self.assertGreaterEqual(len({item["sequence"] for item in revisions_before}), 3)
        self.assertTrue(all(item.get("batch_id") and item["lesson"]["evidence"] for item in revisions_before))

        self.stop_web(graceful=True)
        self.start_web()
        self.assertEqual(self.lesson_revisions(lesson_id), revisions_before)
        latest_probe = "重启后继续看完整排查证据"
        probe_context(latest_probe, old_responses | {revised})

        exchange("这会儿只想聊两句日常", "我给你完整排查结论和证据",
                 "现在不是报告结果，别把那套用于所有聊天",
                 current_lesson(lesson_id)["situation"], revised,
                 current_lesson(lesson_id)["avoid"], "counterexample", lesson_id)
        self.wait_until(lambda: not current_lesson(lesson_id)["active"], "新反例没有停用旧经验")
        self.assertEqual(current_lesson(lesson_id)["counterexamples"], 1)
        versions_after = self.lesson_revisions(lesson_id)
        self.assertGreater(len(versions_after), len(revisions_before))
        self.assertEqual(versions_after[:len(revisions_before)], revisions_before,
                         "停用保留历史版本及其原始证据")
        self.stop_web(graceful=True)
        self.start_web()
        self.assertFalse(current_lesson(lesson_id)["active"])
        self.assertEqual(self.lesson_revisions(lesson_id), versions_after)
        prompt_count = len(prompts)
        final_probe = "恢复后只根据当前日常话题接话"
        probe_context(final_probe, old_responses)
        self.wait_until(lambda: any(any(item["id"] == lesson_id and not item["active"]
                                       and item["counterexamples"] == 1 and item["response"] == revised
                                       for item in prompt["lessons"])
                                   for prompt in prompts[prompt_count:]),
                        "反例停用的经验没有在重启后进入新批次核对")
        self.assertTrue(any(prompt.get("lesson_history") for prompt in prompts),
                        "后续批次应带归档版本，不能只看当前结论")


class GroupConversationLearning(GroupChatAcceptance):
    for _name in dir(GroupChatAcceptance):
        if _name.startswith("test_"): locals()[_name] = None
    del _name

    def learned(self):
        return [s for s in conversation_scopes(self.work)
                if s["scope"]["user_id"] == "eve-chime" and s["lessons"]]

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
