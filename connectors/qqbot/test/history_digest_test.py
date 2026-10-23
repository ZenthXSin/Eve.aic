"""实际 QQ 宿主的历史摘要：超出窗口的较早轮次压缩成有来源的摘要，重启后沿用，可热关闭；只连接本地替身。"""
from request_messages import conversation_messages
import json
import unittest

from dialogue_tools_test import text_response
import history_window_test as window

DIGEST_PROMPT = "你是对话历史压缩器"
POINT = "用户早先说最喜欢蓝色"


class HistoryDigestAcceptance(window.HistoryWindowAcceptance):
    for _name in dir(window.HistoryWindowAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def digest_dir(self):
        return self.work / "state/history-digest"

    def digest_files(self):
        directory = self.digest_dir()
        return sorted(directory.glob("scope-*.json")) if directory.exists() else []

    def digest_requests(self):
        return [r["body"] for r in self.requests if r["body"]["messages"][0]["content"].startswith(DIGEST_PROMPT)]

    def dialogue(self, text):
        return next(r["body"] for r in self.requests
                    if r["body"]["messages"][-1].get("content") == text
                    and not r["body"]["messages"][0]["content"].startswith(DIGEST_PROMPT))

    def respond(self, body):
        if body["messages"][0]["content"].startswith(DIGEST_PROMPT):
            request = json.loads(body["messages"][1]["content"])
            covered = request["previous_turns"] + len(request["turns"])
            return text_response(json.dumps({"points": [{"text": POINT, "turns": [1]},
                {"text": f"已经聊到第 {covered} 轮", "turns": [covered]}]}, ensure_ascii=False))
        return text_response(body["messages"][-1]["content"])

    def test_omitted_turns_become_cited_digest_reused_after_restart_and_hot_disable_falls_back(self):
        self.callback = self.respond
        env = {"EVE_HISTORY_DIGEST_MIN_UNCOVERED_TURNS": "2"}
        wait_digest = lambda: self.wait_until(lambda: self.digest_files(), "历史摘要未生成")
        self.run_window([
            *self.send(self.message("t1", "我最喜欢蓝色")),
            *self.send(self.message("t2", "第二轮")),
            *self.send(self.message("t3", "第三轮")),
            *self.send(self.message("t4", "第四轮")),
            *self.send(self.message("t5", "第五轮")),
            *self.checkpoint("digested"),
            *self.send(self.message("t6", "第六轮"))], panel=True, env_extra=env,
            checkpoints={"digested": wait_digest})
        [first] = self.digest_requests()[:1]
        request = json.loads(first["messages"][1]["content"])
        self.assertEqual((request["previous_turns"], [t["number"] for t in request["turns"]]), (0, [1, 2]))
        self.assertEqual(request["turns"][0]["user"], "我最喜欢蓝色")
        self.assertEqual(first.get("tools", []), [])
        sixth = self.dialogue("第六轮")
        users = [m["content"] for m in conversation_messages(sixth["messages"]) if m["role"] == "user"]
        self.assertEqual(users, ["第四轮", "第五轮", "第六轮"])
        notice = next(m["content"] for m in sixth["messages"] if "eve.runtime.history-window" in (m.get("content") or ""))
        self.assertIn(f"{POINT}（第 1 轮）", notice)
        self.assertIn('"digested_turns":2', notice)
        self.assertIn("第 3 至 3 轮尚未压缩", notice)
        # 原始会话完整保留：摘要不替换、不删除任何轮次。
        original = json.dumps(self.turn("我最喜欢蓝色")["status"], ensure_ascii=False)
        self.assertIn("Completed", original)
        self.assertIn('"text": "我最喜欢蓝色"', original)
        stored = json.loads(self.digest_files()[0].read_text())
        self.assertEqual(stored["format_version"], 1)
        self.assertEqual(stored["versions"][-1]["points"][0], {"text": POINT, "turns": [1]})

        # 重启后沿用磁盘上的摘要；热关闭后退回省略提示且不再请求生成。
        def disable():
            status, page = self.panel("/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": "runtime.history_digest"})
            self.assertEqual(status, 200)
            status, saved = self.panel("/api/plugin-pages/save", {"plugin_id": "eve.config", "page_id": "runtime.history_digest",
                "instance": page["instance"], "expected_revision": page["revision"], "values": {"enabled": False}})
            self.assertEqual(status, 200, saved)
        self.run_window([*self.send(self.message("t7", "重启后")), *self.checkpoint("disable"),
                         *self.send(self.message("t8", "关闭后"))], panel=True, env_extra=env,
                        checkpoints={"disable": disable})
        restarted = self.dialogue("重启后")
        self.assertTrue(any(POINT in (m.get("content") or "") for m in restarted["messages"]))
        disabled = self.dialogue("关闭后")
        notice = next(m["content"] for m in disabled["messages"] if "eve.runtime.history-window" in (m.get("content") or ""))
        self.assertIn('"digested_turns":0', notice)
        self.assertNotIn(POINT, notice)
        self.assertTrue(self.digest_files())

    def test_corrupt_digest_file_is_kept_and_dialogue_continues_with_plain_notice(self):
        self.callback = self.respond
        env = {"EVE_HISTORY_DIGEST_MIN_UNCOVERED_TURNS": "1"}
        def corrupt():
            self.wait_until(lambda: self.digest_files(), "历史摘要未生成")
            self.digest_files()[0].write_text("{broken", encoding="utf8")
        self.run_window([*self.send(self.message("a", "一")), *self.send(self.message("b", "二")),
                         *self.send(self.message("c", "三")), *self.send(self.message("d", "四")),
                         *self.checkpoint("corrupt"), *self.send(self.message("e", "五"))], panel=True, env_extra=env,
                        checkpoints={"corrupt": corrupt})
        notice = next(m["content"] for m in self.dialogue("五")["messages"] if "eve.runtime.history-window" in (m.get("content") or ""))
        self.assertIn('"digested_turns":0', notice)
        self.assertEqual(self.digest_files()[0].read_text(encoding="utf8"), "{broken")


if __name__ == "__main__":
    unittest.main()
