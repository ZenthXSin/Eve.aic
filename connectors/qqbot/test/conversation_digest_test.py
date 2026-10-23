"""实际宿主的抽取式情景视图与表达示例；仅用合成消息和本地替身。"""
import json
import unittest
import memory_test as memory

KIND = "eve.conversation-digest-v1"
FIRST = "最近下班有点累，想轻松聊聊"
SECOND = "嗯，随便说两句就行啦"


class ConversationDigestAcceptance(memory.MemoryAcceptance):
    for _name in dir(memory.MemoryAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def data(self, request):
        data = []
        for message in request["body"]["messages"]:
            text = message.get("content") or ""
            if KIND in text:
                data.append(json.loads(text[text.index("{"):]))
        return data

    def save(self, **values):
        self.save_page("runtime.memory_digest", values)

    def save_page(self, namespace, values):
        status, page = self.panel("/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": namespace})
        self.assertEqual(status, 200)
        status, result = self.panel("/api/plugin-pages/save", {"plugin_id": "eve.config", "page_id": namespace,
            "instance": page["instance"], "expected_revision": page["revision"], "values": values})
        self.assertEqual(status, 200, result)
        self.assertEqual(result["restart_required"], [])

    def test_current_style_policy_hot_toggle_adds_no_calls_and_is_not_persisted_in_conversation(self):
        self.run_eve([*self.send(self.message("on", "普通聊天")), *self.checkpoint("off"),
                      *self.send(self.message("off", "继续聊天")), *self.checkpoint("on"),
                      *self.send(self.message("again", "再聊两句"))], memory=False, panel=True,
                     checkpoints={"off": lambda: self.save_page("runtime.dialogue", {"natural_style": False}),
                                  "on": lambda: self.save_page("runtime.dialogue", {"natural_style": True})})
        self.assertEqual(len(self.requests), 3)
        for index, request in enumerate(self.requests):
            messages = request["body"]["messages"]
            policies = [i for i, m in enumerate(messages) if (m.get("content") or "").startswith("本轮自然接话约束")]
            self.assertEqual(bool(policies), index != 1)
            self.assertIn("eve.runtime.tool-capabilities", messages[-2]["content"])
            if policies:
                self.assertEqual(policies, [len(messages) - 4])
                self.assertEqual(messages[-3]["role"],"user")
                self.assertTrue(messages[-3]["content"].startswith("宿主表达方式"))
        for session in self.documents()["eve.session"]["sessions.v1"]["sessions"].values():
            for turn in session["turns"]:
                self.assertNotIn("本轮自然接话约束", json.dumps(turn["status"], ensure_ascii=False))
                self.assertNotIn("宿主表达方式", json.dumps(turn["status"], ensure_ascii=False))
                self.assertNotIn("宿主表达方式",turn["input"])

    def test_short_user_messages_are_valid_style_examples_without_copying_old_assistants(self):
        self.run_eve([
            *self.send(self.message("brief1","行")),
            *self.send(self.message("brief2","试试")),
            *self.send(self.message("view","看看")),
            *self.send(self.message("other","另一会话",user="other-user")),
        ],training=True)
        request=next(r for r in self.requests if r["body"]["messages"][-1]["content"] == "看看")
        [view]=self.data(request)
        self.assertEqual([e["user"]["text"] for e in view["expression_examples"]],["行","试试"])
        guard=request["body"]["messages"][-3]["content"]
        self.assertIn("eve.current-expression-style-v1",guard)
        self.assertIn("试试",guard)
        foreign=next(r for r in self.requests if r["body"]["messages"][-1]["content"] == "另一会话")
        self.assertEqual(self.data(foreign),[])

    def test_scoped_sources_training_reset_pause_and_restart_keep_the_original_memory(self):
        self.run_eve([
            *self.send(self.message("first", FIRST)),
            *self.send(self.message("other", "另一个用户的私密表达", user="other-user")),
            *self.send(self.message("second", SECOND)),
            *self.send(self.message("reset", "/train reset", contains="重置")),
            *self.send(self.message("after-reset", "重置后先正常聊聊就好")),
            *self.send(self.message("stop", "/train stop", contains="已结束")),
            *self.send(self.message("after-stop", "现在先停止表达学习")),
            *self.checkpoint("disable"),
            *self.send(self.message("disabled", "这条不带蒸馏视图"))],
            training=True, panel=True, checkpoints={"disable": lambda: self.save(enabled=False)})
        find = lambda text: next(r for r in self.requests if r["body"]["messages"][-1]["content"] == text)
        self.assertEqual(self.data(find("另一个用户的私密表达")), [])
        [view] = self.data(find(SECOND))
        self.assertEqual(view["method"], "extractive-v1")
        self.assertEqual([e["user"]["text"] for e in view["episodes"]], [FIRST])
        self.assertEqual([e["user"]["text"] for e in view["expression_examples"]], [FIRST])
        self.assertNotIn("另一个用户", json.dumps(view, ensure_ascii=False))
        [reset] = self.data(find("重置后先正常聊聊就好"))
        self.assertEqual(reset["expression_examples"], [])
        [stopped] = self.data(find("现在先停止表达学习"))
        self.assertTrue(stopped["episodes"])
        self.assertEqual(stopped["expression_examples"], [])
        self.assertEqual(self.data(find("这条不带蒸馏视图")), [])
        original = next(e for e in self.evidence() if e["source"].get("user_text") == FIRST)
        self.run_eve([*self.send(self.message("restart", "重启后配置仍关闭")), *self.checkpoint("enable"),
                      *self.send(self.message("restored", "恢复查看原来的交流"))],
                     training=True, panel=True, checkpoints={"enable": lambda: self.save(enabled=True, max_episodes=2)})
        self.assertEqual(self.data(find("重启后配置仍关闭")), [])
        [restored] = self.data(find("恢复查看原来的交流"))
        self.assertEqual(len(restored["episodes"]), 2)
        self.assertEqual(restored["expression_examples"], [])  # /train stop 跨重启保留。
        self.assertEqual(next(e for e in self.evidence() if e["id"] == original["id"]), original)
        self.assertEqual(len(self.requests), 8)  # 配置、停止、重置及抽取没有额外模型请求。


if __name__ == "__main__":
    unittest.main(verbosity=2)
