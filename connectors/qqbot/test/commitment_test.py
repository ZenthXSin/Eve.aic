"""实际 eve-qqbot 的约定验收：Eve 答应别人之后要做的事用工具记下，同一个人在另一个群里也记得，
做完后了结；别人看不到，重启后保留，群与私聊未绑定时不关联。

模型替身按请求里的上下文与工具结果确定性地调用工具，只证明宿主契约，不代表真实模型的理解能力。
"""
import json
import unittest

import people_test
from dialogue_tools_test import text_response, tool_response

MARKER = '{"kind":"eve.commitments.v1"'


def commitments(body):
    found = [m["content"] for m in body["messages"]
             if m["role"] == "system" and isinstance(m.get("content"), str) and MARKER in m["content"]]
    assert len(found) <= 1
    return json.loads(found[0][found[0].index(MARKER):]) if found else None


class CommitmentAcceptance(people_test.PeopleAcceptance):
    for _name in dir(people_test.PeopleAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def ledger(self):
        return self.documents().get("eve.commitment", {}).get("commitments.v1") or {"commitments": []}

    def test_a_promise_is_kept_across_sessions_of_the_same_person_and_settled_once(self):
        seen = {}

        def respond(body):
            messages = body["messages"]
            current = max(i for i, m in enumerate(messages) if m["role"] == "user")
            text = messages[current]["content"]
            results = [json.loads(m["content"]) for m in messages[current + 1:] if m["role"] == "tool"]
            names = {t["function"]["name"] for t in body.get("tools", [])}
            self.assertTrue({"note_commitment", "settle_commitment"} <= names)
            if not results:
                seen[text] = commitments(body)
            if text == "下次帮我看看地图吧":
                if not results:
                    return tool_response([("note_commitment", {"text": "帮他看看地图"})])
                self.assertTrue(results[0]["saved"])
                return text_response("好，下次帮你看")
            if text == "地图看了吗":
                if not results:
                    [open_one] = seen[text]["open"]
                    return tool_response([("settle_commitment", {"commitment_id": open_one["id"], "outcome": "done",
                                                                  "note": "看完了，没问题"})])
                self.assertTrue(results[0]["settled"])
                return text_response("看过了，没问题")
            return text_response("好")
        self.callback = respond

        def say(mid, text, user="user-1", scope="group", target="group-1", expected="好"):
            return self.send(self.message(mid, text, user=user, scope=scope, target=target, expected=expected))
        self.enabled([*say("promise", "下次帮我看看地图吧", expected="好，下次帮你看"),
                      *say("other-group", "在吗", target="group-2"),
                      *say("someone-else", "大家好", user="user-2", target="group-2"),
                      *say("settle", "地图看了吗", target="group-2", expected="看过了，没问题"),
                      *say("after", "谢谢", target="group-2")])
        self.assertIsNone(seen["下次帮我看看地图吧"], "没有约定时不加这段数据")
        [made] = seen["在吗"]["open"]
        self.assertEqual((made["text"], made["made_here"]), ("帮他看看地图", False),
                         "同一个人在另一个群里也记得，并标明不是在这里答应的")
        self.assertIsNone(seen["大家好"], "别人看不到")
        after = seen["谢谢"]
        self.assertEqual(after["open"], [])
        self.assertEqual([(s["text"], s["outcome"], s["note"]) for s in after["recently_settled"]],
                         [("帮他看看地图", "done", "看完了，没问题")])
        [saved] = self.ledger()["commitments"]
        self.assertEqual(saved["status"], "done")
        # 重启后保留；群与私聊的平台标识属于不同命名空间，未绑定时不关联。
        self.enabled([*say("restart", "我又来了"),
                      *say("private", "私聊一下", scope="c2c", target=None)])
        self.assertEqual([s["text"] for s in seen["我又来了"]["recently_settled"]], ["帮他看看地图"])
        self.assertIsNone(seen["私聊一下"])
        self.assertEqual(len(self.ledger()["commitments"]), 1, "了结后不重复记下")


if __name__ == "__main__":
    unittest.main()
