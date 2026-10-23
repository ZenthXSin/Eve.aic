"""实际 QQ 宿主的说法学习：训练中的会话攒够一批原话后学习一次，核对摘录出自原话再保存。

模型与 QQ 都是本地替身。验证学到的词语进入下一轮对话上下文、可查看可删除、
不合格结果整批不保存、取出的批次重启后不重试，关闭学习或停止训练时不发学习请求。
"""
import json
import time
import unittest

import developer_tools_test as developer
import dialogue_tools_test as dialogue

LEARNER_MARK = "从一位用户自己发的聊天原话里学习说法"
WORDS = ["这游戏yyds", "确实确实，我也这么想", "今天又摸鱼了", "晚上开黑吗", "这个boss好难打",
         "确实确实，太对了", "周末去看电影", "那家店的奶茶yyds", "我先去吃饭", "回来再聊",
         "你觉得呢", "就这样吧"]


class PhraseAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.callback = lambda body: dialogue.text_response("好的")
        self.learned = {"phrases": [
            {"kind": "term", "text": "yyds", "meaning": "永远的神，夸东西特别好", "example": "这游戏yyds"},
            {"kind": "expression", "text": "表示赞同时", "meaning": "常连说两遍“确实”", "example": "确实确实"},
        ]}

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and set(decoded) == {"messages"}:
            return "phrase"
        return super().classify(decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "phrase":
            self.assertEqual(decoded["messages"], WORDS)
            return json.dumps(self.learned, ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def provider_response(self, body, kind, fallback):
        if kind == "phrase":
            self.assertEqual(body.get("response_format"), {"type":"json_object"})
            self.assertFalse(body.get("tools"))
            self.assertEqual(body["messages"][-2]["role"],"user")
            self.assertIn("待分析的用户原话",body["messages"][-2]["content"])
        return super().provider_response(body,kind,fallback)

    def phrase_state(self):
        return self.documents().get("eve.training", {}).get("phrases.v1")

    def wait_for(self, predicate, message, timeout=15):
        deadline = time.monotonic() + timeout
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            time.sleep(0.05)

    def chat(self, script, env=None, **options):
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     more=["--training"], env_extra=env or {}, timeout=60, **options)

    def words(self, prefix="w"):
        return [step for index, text in enumerate(WORDS)
                for step in self.send(self.message(f"{prefix}{index}", text, expected="好的"))]

    def test_batch_learns_terms_that_reach_the_next_turn_and_can_be_listed_and_forgotten(self):
        def learned():
            self.wait_for(lambda: len((self.phrase_state() or {}).get("learned", [])) == 2, "说法没有保存")
        seen = []
        styles = []
        def respond(body):
            styles.append(body["messages"][-3]["content"])
            seen.append("\n".join(str(m.get("content", "")) for m in body["messages"] if m["role"] == "system"))
            return dialogue.text_response("好的")
        self.callback = respond
        self.chat([*self.words(), *self.checkpoint("learned"),
                   *self.send(self.message("list", "/train words", contains=["yyds", "永远的神", "表示赞同时", "p"])),
                   *self.send(self.message("use", "这首歌yyds", expected="好的"))],
                  checkpoints={"learned": learned})
        self.assertEqual(len(self.kind("phrase")), 1)
        state = self.phrase_state()
        self.assertEqual([p["text"] for p in state["pending"]], ["这首歌yyds"], "取出成批后原话移出缓冲，之后的新原话重新攒批")
        self.assertIn("词语「yyds」：永远的神", seen[-1])
        self.assertIn("表示赞同时", seen[-1])
        self.assertIn("eve.current-expression-style-v1",styles[-1])
        self.assertIn("yyds",styles[-1])
        self.assertIn("表示赞同时",styles[-1])
        term = next(s["phrase"] for s in state["learned"] if s["phrase"]["kind"] == "term")
        self.chat(self.send(self.message("forget", f"/train forget {term['id']}", contains="已删除")))
        self.assertEqual([s["phrase"]["text"] for s in self.phrase_state()["learned"]], ["表示赞同时"])
        self.assertEqual(len(self.kind("phrase")), 1, "重启不重新学习已取出的批次")

    def test_unverifiable_output_saves_nothing_and_is_not_retried_after_restart(self):
        self.learned = {"phrases": [{"kind": "term", "text": "绝绝子", "meaning": "非常好", "example": "绝绝子"}]}
        self.chat([*self.words(), *self.checkpoint("asked")],
                  checkpoints={"asked": lambda: self.wait_for(lambda: self.kind("phrase"), "没有学习请求")})
        self.wait_for(lambda: self.phrase_state() is not None, "状态未写入")
        self.chat(self.send(self.message("list", "/train words", contains="还没有学到说法")))
        self.assertEqual(self.phrase_state()["learned"], [])
        self.assertEqual(self.phrase_state()["pending"], [])
        self.assertEqual(len(self.kind("phrase")), 1)

    def test_disabled_learning_or_stopped_training_sends_no_learning_request(self):
        self.chat([*self.words("off")], env={"EVE_PHRASE_LEARNING": "false"})
        self.assertEqual(self.kind("phrase"), [])
        self.assertEqual(len(self.phrase_state()["pending"]), len(WORDS), "关闭学习时原话留在有界缓冲")
        self.chat([*self.send(self.message("reset", "/train reset", contains="已重置")),
                   *self.send(self.message("stop", "/train stop", contains="已结束")),
                   *self.words("stopped")], env={"EVE_PHRASE_LEARNING": "false"})
        self.assertEqual(self.phrase_state()["pending"], [], "重置删除待学原话，停止训练后不再缓冲")
        # 重新开启学习也没有可取的批次。
        self.chat(self.send(self.message("words", "/train words", contains="还没有学到说法")))
        self.assertEqual(self.kind("phrase"), [])


if __name__ == '__main__':
    unittest.main()
