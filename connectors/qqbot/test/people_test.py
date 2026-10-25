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

    def identity(self, body):
        marker = "当前发言者身份卡"
        for message in body["messages"]:
            text = message.get("content", "")
            if message["role"] == "system" and isinstance(text, str) and marker in text:
                return json.loads(text.split("\n", 1)[1])
        self.fail("missing identity card")

    def authorization(self, body):
        for message in body["messages"]:
            text = message.get("content", "")
            if message["role"] == "system" and isinstance(text, str) and '"eve.host.authorization"' in text:
                # 授权是固定资料，并入第二条系统消息，独占一行。
                [line] = [line for line in text.splitlines() if '"eve.host.authorization"' in line]
                data = json.loads(line)
                return "owner" if data["owner"] else "administrator" if data["administrator"] else "member"
        self.fail("missing authorization")

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
            self.assertTrue(all("presence" not in p for p in view["others"]), "别人的来访情况不给看")
            card = self.identity(body)
            self.assertEqual((card["scene"], card["role"]), ("group", self.authorization(body)))
            self.assertEqual((card["name"], card["name_source"]), (current, source))
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
        def names():
            return {p["scope"]["participant"]: (p.get("name"), p["revision"]) for p in self.people_state()["people"]}
        def visits():
            return {p["scope"]["participant"]: p["presence"]["messages"] for p in self.people_state()["people"]}
        before, counted = names(), visits()
        self.assertEqual(self.people_state()["version"], 3)
        self.assertEqual(sorted(counted.values()), [1, 1, 1, 3], "每条消息记一次来访，群与身份分开计数")
        expected.append(("小明", ["阿白", "zxs"], "declared"))
        self.enabled(send_name("after-restart", "你还记得大家吗", "user-1"))
        self.assertEqual(names(), before, "重启后称呼不变")
        after = visits()
        self.assertEqual(sum(after.values()), sum(counted.values()) + 1, "重启后接着计数")
        self.assertFalse(expected)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and set(decoded) == {"messages", "known"}:
            return "impression"
        if isinstance(decoded, dict) and set(decoded) == {"sources"}:
            return "profile"
        return super().classify(decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "impression":
            return json.dumps(self.impression_reply(decoded), ensure_ascii=False)
        if kind == "profile":
            return json.dumps(self.profile_reply(decoded), ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def people_state(self):
        return self.documents()["eve.people"]["people.v1"]

    def wait_learned(self, count):
        def done():
            people = self.people_state()["people"]
            return sum(len(p.get("impressions", [])) for p in people) == count
        self.wait_until(done, "印象没有保存")

    def test_impressions_come_only_from_own_words_reach_only_the_speaker_and_can_be_forgotten(self):
        talk = ["我最近在学Rust", "我养了一只橘猫", "今天好累啊我", "我下周要去成都出差"]
        batches = []
        def impressions(batch):
            batches.append(batch)
            if len(batches) == 1:
                self.assertEqual(batch, {"messages": talk, "known": []})
                return {"impressions": [{"text": "在学 Rust", "quote": "我最近在学Rust", "replaces": None},
                                        {"text": "养了一只橘猫", "quote": "我养了一只橘猫", "replaces": None}]}
            self.assertEqual([k["text"] for k in batch["known"]], ["在学 Rust", "养了一只橘猫"])
            old, cat = batch["known"][0]["id"], batch["known"][1]["id"]
            return {"impressions": [{"text": "改学 Go 了", "quote": "我不学Rust了改学Go", "replaces": old},
                                    {"text": "养了一只橘猫", "quote": "我家橘猫又胖了", "replaces": None, "reaffirms": cat}]}
        self.impression_reply = impressions
        seen = {}
        def respond(body):
            data = self.facts(body)
            seen[body["messages"][-1]["content"]] = data["people"]
            return text_response("好")
        self.callback = respond
        say = lambda mid, text, user="user-1": self.send(self.message(mid, text, user=user, scope="group",
                                                                      target="group-1", expected="好"))
        self.enabled([*[step for i, text in enumerate(talk) for step in say(f"t{i}", text)],
                      *self.checkpoint("learned"),
                      *say("ask", "你还记得我在学什么吗"),
                      *say("other", "大家好呀", user="user-2"),
                      *self.send(self.message("list", "/impressions", user="user-1", scope="group", target="group-1",
                                              contains=["在学 Rust", "养了一只橘猫", "我最近在学Rust"]))],
                     checkpoints={"learned": lambda: self.wait_learned(2)})
        self.assertEqual(len(self.kind("impression")), 1)
        self.assertEqual([i["text"] for i in seen["你还记得我在学什么吗"]["current"]["impressions"]],
                         ["在学 Rust", "养了一只橘猫"])
        self.assertNotIn("pending", seen["你还记得我在学什么吗"]["current"])
        other = seen["大家好呀"]
        self.assertNotIn("impressions", other["current"])
        self.assertTrue(all("impressions" not in p for p in other["others"]), "别人的印象不进入他人的对话")
        self.assertEqual(self.people_state()["version"], 3, "有来访记录时为格式 3，印象照常保存")
        # 重启后不重放已取出的批次；新的原话更新旧印象。
        later = ["我不学Rust了改学Go", "我觉得Go挺好上手", "我周末打算写个小工具", "我家橘猫又胖了"]
        self.enabled([*[step for i, text in enumerate(later) for step in say(f"l{i}", text)],
                      *self.checkpoint("updated")],
                     checkpoints={"updated": lambda: self.wait_until(
                         lambda: any(i["text"] == "改学 Go 了" for p in self.people_state()["people"]
                                     for i in p.get("impressions", [])), "印象没有更新")})
        self.assertEqual(len(self.kind("impression")), 2)
        mine = next(p for p in self.people_state()["people"] if p.get("impressions"))["impressions"]
        self.assertEqual([i["text"] for i in mine], ["改学 Go 了", "养了一只橘猫"])
        self.assertEqual(mine[1]["reaffirmed"], 1, "又说到同一件事只计次，不重复新增")
        self.assertEqual(mine[1]["quote"], "我养了一只橘猫")
        self.assertEqual(self.people_state()["version"], 5)
        self.enabled([*self.send(self.message("list-again", "/impressions", user="user-1", scope="group",
                                              target="group-1", contains=["之后又提到 1 次"]))])
        self.enabled([*self.send(self.message("forget-other", f"/impressions forget {mine[1]['id']}", user="user-2",
                                              scope="group", target="group-1", contains="没有你的印象")),
                      *self.send(self.message("forget", f"/impressions forget {mine[1]['id']}", user="user-1",
                                              scope="group", target="group-1", contains="已删除"))])
        mine = next(p for p in self.people_state()["people"] if p.get("impressions"))["impressions"]
        self.assertEqual([i["text"] for i in mine], ["改学 Go 了"])

    def save(self, namespace, **values):
        code, page = self.panel("/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": namespace})
        self.assertEqual(code, 200)
        code, saved = self.panel("/api/plugin-pages/save", {"plugin_id": "eve.config", "page_id": namespace,
                                 "instance": page["instance"], "expected_revision": page["revision"],
                                 "values": values})
        self.assertEqual(code, 200)

    def test_impressions_follow_the_same_person_across_sessions_with_memory_visibility(self):
        group_talk = ["我最近在学Rust", "我养了一只橘猫", "今天好累啊我", "我下周要去成都出差"]
        private_talk = ["我其实是护士", "我上夜班", "我有点累", "我先睡了"]
        def impressions(batch):
            if batch["messages"] == group_talk:
                return {"impressions": [{"text": "在学 Rust", "quote": "我最近在学Rust", "replaces": None}]}
            self.assertEqual(batch["messages"], private_talk)
            return {"impressions": [{"text": "是护士", "quote": "我其实是护士", "replaces": None}]}
        self.impression_reply = impressions
        seen = {}
        def respond(body):
            seen[body["messages"][-1]["content"]] = self.facts(body)
            return text_response("好")
        self.callback = respond
        def say(mid, text, user="user-1", scope="group", target="group-1"):
            return self.send(self.message(mid, text, user=user, scope=scope, target=target, expected="好"))
        route = lambda scope, target: "qq:" + hashlib.sha256(json.dumps(
            ["1904159860", scope, target, "user-1"], separators=(",", ":")).encode()).hexdigest()
        bound = " ".join([route("c2c", "user-1"), route("group", "group-1"), route("group", "group-2")])
        self.enabled([*[step for i, text in enumerate(group_talk) for step in say(f"g{i}", text)],
                      *self.checkpoint("group-learned"),
                      *say("other-group", "又见面了", target="group-2"),
                      *say("someone-else", "大家好", user="user-2", target="group-2"),
                      *[step for i, text in enumerate(private_talk)
                        for step in say(f"p{i}", text, scope="c2c", target=None)],
                      *self.checkpoint("private-learned"),
                      *say("private-unbound", "私聊问一下", scope="c2c", target=None),
                      *self.checkpoint("bind"),
                      *say("private-bound", "绑定后私聊", scope="c2c", target=None),
                      *say("group-bound", "绑定后在群里", target="group-2"),
                      *self.checkpoint("disable"),
                      *say("group-disabled", "关掉以后", target="group-2")],
                     checkpoints={"group-learned": lambda: self.wait_learned(1),
                                  "private-learned": lambda: self.wait_learned(2),
                                  "bind": lambda: self.save("runtime.people", owner_principals=bound),
                                  "disable": lambda: self.save("runtime.memory_continuity", enabled=False)},
                     panel=True)
        linked = lambda text: [(i["text"], i.get("scene")) for i in
                               seen[text].get("linked_impressions", {}).get("items", [])]
        # 同一平台账号在另一个群：读到前一个群里自述的事，但印象仍留在原会话。
        self.assertEqual(linked("又见面了"), [("在学 Rust", "group")])
        self.assertNotIn("impressions", seen["又见面了"]["people"]["current"])
        self.assertEqual(linked("大家好"), [], "别人读不到")
        # 群与私聊的平台标识属于不同命名空间，没有明确绑定前不关联。
        self.assertEqual(linked("私聊问一下"), [])
        self.assertEqual(linked("绑定后私聊"), [("在学 Rust", "group")], "私聊可读同一人在群里说的")
        self.assertEqual(linked("绑定后在群里"), [("在学 Rust", "group")], "私聊里说的默认不进群")
        self.assertEqual(linked("关掉以后"), [], "关闭跨会话记忆后同时停止合并印象")
        mine = [p for p in self.people_state()["people"] if p.get("impressions")]
        self.assertEqual(sorted(i["text"] for p in mine for i in p["impressions"]), ["在学 Rust", "是护士"])
        self.assertEqual(len(mine), 2, "只读合并，不搬迁或复制印象")

    def test_enough_impressions_across_sessions_become_a_cited_profile_that_expires_with_its_sources(self):
        first = ["我最近在学Rust", "我养了一只橘猫", "我住北京", "今天好累啊我"]
        second = ["我其实是护士", "我上夜班", "我喜欢爬山", "我先去忙了"]
        learned = {tuple(first): [("在学 Rust", "我最近在学Rust"), ("养了一只橘猫", "我养了一只橘猫"), ("住北京", "我住北京")],
                   tuple(second): [("是护士", "我其实是护士"), ("上夜班", "我上夜班"), ("喜欢爬山", "我喜欢爬山")]}
        self.impression_reply = lambda batch: {"impressions": [
            {"text": text, "quote": quote, "replaces": None} for text, quote in learned[tuple(batch["messages"])]]}
        consolidations = []
        def profile(request):
            consolidations.append(request)
            texts = [s["text"] for s in request["sources"]]
            self.assertEqual(sorted(texts), sorted(t for items in learned.values() for t, _ in items))
            self.assertTrue(all("quote" in s and "here" in s for s in request["sources"]))
            return {"facts": [{"text": "在学 Rust 的护士，上夜班",
                               "from": [texts.index("在学 Rust"), texts.index("是护士"), texts.index("上夜班")]}]}
        self.profile_reply = profile
        seen = {}
        def respond(body):
            seen[body["messages"][-1]["content"]] = self.facts(body)
            return text_response("好")
        self.callback = respond
        def say(mid, text, target):
            return self.send(self.message(mid, text, user="user-1", scope="group", target=target, expected="好"))
        def has_profile():
            return any(p.get("profile") for p in self.people_state()["people"])
        self.enabled([*[step for i, text in enumerate(first) for step in say(f"a{i}", text, "group-1")],
                      *self.checkpoint("first"),
                      *[step for i, text in enumerate(second) for step in say(f"b{i}", text, "group-2")],
                      *self.checkpoint("second"),
                      *say("before", "整理之前", "group-2"),
                      *self.checkpoint("profiled"),
                      *say("after", "整理之后", "group-2")],
                     checkpoints={"first": lambda: self.wait_learned(3), "second": lambda: self.wait_learned(6),
                                  "profiled": lambda: self.wait_until(has_profile, "画像没有保存")})
        before = seen["整理之前"]
        self.assertNotIn("profile", before)
        self.assertEqual(len(before["linked_impressions"]["items"]), 3, "整理前照常给逐条印象")
        after = seen["整理之后"]
        self.assertEqual(after["profile"]["facts"],
                         [{"text": "在学 Rust 的护士，上夜班", "scenes": ["group", "group", "group"]}])
        self.assertNotIn("linked_impressions", after, "画像代替逐条印象")
        self.assertNotIn("impressions", after["people"]["current"])
        self.assertEqual(len(consolidations), 1, "依据不变不重复整理")
        self.assertEqual(self.people_state()["version"], 4)
        # 删除一条依据：画像随之删除，下一轮退回逐条印象。
        mine = next(p for p in self.people_state()["people"] if p.get("profile"))
        self.enabled([*self.send(self.message("forget", f"/impressions forget {mine['impressions'][0]['id']}",
                                              user="user-1", scope="group", target="group-2", contains="已删除")),
                      *say("forgotten", "删掉以后", "group-2")])
        self.assertNotIn("profile", seen["删掉以后"])
        self.assertIn("linked_impressions", seen["删掉以后"])
        self.assertFalse(has_profile())

    def test_unverifiable_impressions_are_rejected_and_disabled_learning_sends_nothing(self):
        talk = ["我是做前端的", "我喜欢爬山", "我最近睡得很晚", "我想换个工作"]
        self.impression_reply = lambda batch: {"impressions": [{"text": "是医生", "quote": "我是医生", "replaces": None}]}
        say = lambda mid, text: self.send(self.message(mid, text, expected="完成"))
        self.enabled([*[step for i, text in enumerate(talk) for step in say(f"t{i}", text)],
                      *self.checkpoint("asked")],
                     checkpoints={"asked": lambda: self.wait_until(lambda: self.kind("impression"), "没有学习请求")})
        self.wait_until(lambda: not any(p.get("pending") for p in self.people_state()["people"]), "批次未取出")
        self.assertFalse(any(p.get("impressions") for p in self.people_state()["people"]))
        self.enabled([*[step for i, text in enumerate(talk) for step in say(f"off{i}", text)]],
                     env_extra={"EVE_PEOPLE_IMPRESSIONS": "false"})
        self.assertEqual(len(self.kind("impression")), 1)
        self.assertEqual(len(next(p for p in self.people_state()["people"])["pending"]), 4, "关闭学习时原话留在有界缓冲")

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
        roles = [(False, "这是第一次来"), (True, "刚刚"), (True, "这是第一次来")]
        def respond(body):
            data = self.facts(body)
            self.assertEqual(data["owner_name"], "zxs")
            self.assertIs(data["current_is_owner"], checks.pop(0))
            card = self.identity(body)
            self.assertEqual((card["owner_bound"], card["last_visit"]), roles.pop(0))
            self.assertEqual(card["role"], self.authorization(body), "角色与宿主授权一致，不看自称")
            self.assertEqual(card["role"] == "owner", data["current_is_owner"] is True, "只有绑定的主人账号才是主人")
            self.assertEqual(card["scene"], "private")
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
        self.assertFalse(roles)

if __name__ == "__main__":
    unittest.main()
