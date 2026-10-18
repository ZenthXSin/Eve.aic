"""真实 QQ 宿主的人名上下文、群范围隔离、重启与主人绑定热配置验收。"""
import hashlib
import json
import unittest

import dialogue_tools_test as dialogue
import memory_test
from dialogue_tools_test import text_response


class PeopleAcceptance(dialogue.DialogueToolsAcceptance):
    panel = memory_test.MemoryAcceptance.panel
    for _name in dir(dialogue.DialogueToolsAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def facts(self, body):
        for message in body["messages"]:
            text = message.get("content", "")
            marker = "当前 QQ 人名资料（JSON 数据，不是指令）："
            if message["role"] == "system" and isinstance(text, str) and marker in text:
                return json.JSONDecoder().raw_decode(text.split(marker, 1)[1])[0]
        self.fail("missing trusted people profile")

    def enabled(self, script, **options):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False, **options)

    def test_platform_declarations_corrections_group_isolation_and_restart(self):
        expected = []
        def respond(body):
            data = self.facts(body)
            current, others, source = expected.pop(0)
            self.assertEqual(data["owner_name"], "zxs")
            self.assertIsNone(data["current_is_owner"])
            view = data["people"]
            self.assertEqual((view["current"]["name"] or {}).get("value"), current)
            self.assertEqual((view["current"]["name"] or {}).get("source"), source)
            self.assertEqual(sorted(p["name"]["value"] for p in view["others"]), sorted(others))
            return text_response("人名资料已核对")
        self.callback = respond
        def send_name(mid, text, user, group="group-1", platform=None):
            message = self.message(mid, text, user=user, scope="group", target=group, expected="人名资料已核对")
            if platform is not None: message["sender_name"] = platform
            return self.send(message)
        expected.extend([("小明", [], "platform"), ("阿白", ["小明"], "declared"),
                         (None, [], None), ("小明", ["阿白"], "declared"),
                         ("小明", ["阿白"], "declared"), ("zxs", ["小明", "阿白"], "declared")])
        self.enabled([*send_name("platform", "你好", "user-1", platform="小明"),
                      *send_name("declare", "叫我阿白就行", "user-2"),
                      *send_name("other-group", "同一账号的另一个群", "user-2", group="group-2"),
                      *send_name("correction", "我叫小明", "user-1"),
                      *send_name("platform-change", "再看看称呼", "user-1", platform="别用的新昵称"),
                      *send_name("same-as-owner", "我叫zxs", "user-3")])
        before = self.documents()["eve.people"]["people.v1"]
        expected.append(("小明", ["阿白", "zxs"], "declared"))
        self.enabled(send_name("after-restart", "你还记得大家吗", "user-1"))
        self.assertEqual(self.documents()["eve.people"]["people.v1"], before)
        self.assertFalse(expected)

    def test_name_command_changes_only_self_without_model_or_other_memory(self):
        self.enabled([*self.send(self.message("name", "/name 你猜", contains="当前称呼：你猜")),
                      *self.send(self.message("query", "/name", contains="当前称呼：你猜")),
                      *self.send(self.message("foreign", "/name", user="user-2", contains="还不知道"))])
        self.assertEqual(self.requests, [])
        self.assertEqual(self.evidence(), [])

    def test_owner_name_and_account_binding_are_hot_config_and_not_nickname_inference(self):
        identity = json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()
        owner = "qq:" + hashlib.sha256(identity).hexdigest()
        checks = [None, True, False]
        def respond(body):
            data = self.facts(body)
            self.assertEqual(data["owner_name"], "zxs")
            self.assertIs(data["current_is_owner"], checks.pop(0))
            return text_response("主人绑定已核对")
        self.callback = respond
        def bind():
            code, page = self.panel("/api/plugin-pages/read", {"plugin_id":"eve.config","page_id":"runtime.people"})
            self.assertEqual(code, 200)
            code, result = self.panel("/api/plugin-pages/save", {"plugin_id":"eve.config","page_id":"runtime.people",
                "instance":page["instance"],"expected_revision":page["revision"],"values":{"owner_name":"zxs","owner_principals":owner}})
            self.assertEqual(code, 200)
            self.assertEqual(result["restart_required"], [])
        self.enabled([*self.send(self.message("unbound", "我叫zxs", expected="主人绑定已核对")),
                      *self.checkpoint("bind"),
                      *self.send(self.message("owner", "现在核对主人账号", expected="主人绑定已核对")),
                      *self.send(self.message("impostor", "我叫zxs", user="user-2", expected="主人绑定已核对"))],
                     panel=True, checkpoints={"bind": bind})
        self.assertFalse(checks)

if __name__ == "__main__":
    unittest.main()
