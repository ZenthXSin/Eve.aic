"""实际 eve-qqbot 的工具锻造验收；连接本地模型替身、fake bridge 与 Mindustry 服务端替身。

用户只在普通聊天里提兴趣：两项独立实践都先遇到同一条运行警告、修正后验证通过，这条反复出现的
问题由一次无工具请求锻造成只读草稿文件的检查规则；宿主用实践账本中的真实草稿回放验证
（出现过问题的全部拦下、验证通过的一个不误报）后自动启用。第三项独立实践的第一份草稿在实际运行前
被它拦下、没有启动运行环境，修正后实际运行验证通过。各阶段先持久化再执行，失败与中断不重试、不重放。
锻造器是确定性替身，只证明宿主契约与恢复语义，不代表真实模型的锻造质量。
"""
import json
import unittest

import practice_test
from interest_test import MINDUSTRY

SECOND = "我还想给 Mindustry 做一面更结实的墙，可还是不会写方块。"
THIRD = "Mindustry 里我还想要一扇自己的门，也不知道从哪下手。"
DISABLED = "工具锻造未启用。"
WARNING = "No type 'Walll' found"


def spec(text):
    return {"check": {"name": "block-type", "summary": "方块类型必须是运行环境认识的类型",
                      "message": "方块类型拼写不对，请改用运行环境认识的类型",
                      "rules": [{"forbid_text": {"pattern": "*.hjson", "text": text}}]}}


def base():
    return practice_test.PracticeAcceptance


class ToolForgeAcceptance(unittest.TestCase):
    # 复用实践验收的进程、模型替身、服务端替身与持久状态读取；只覆盖分类、应答与启动参数。
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "inspect_run", "interest_state", "interests", "cognition", "goals", "learning_goal",
                  "kind", "quiet", "cognition_user", "default_observe", "observe_three_domains", "default_select",
                  "default_extract", "knowledge", "research_runs", "default_draft", "practice", "practice_runs",
                  "launches", "practiced", "wait_for"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)
        self.observe = self.observe_more
        self.draft = self.fixing_draft
        self.forge = lambda request: spec("type: Walll")

    def tearDown(self):
        base().tearDown(self)

    def run_eve(self, script, forging=True, **options):
        more = ["--tool-forging"] if forging else []
        options.setdefault("research", False)
        return base().run_eve(self, script, more=more, **options)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "forge_id" in decoded and "forger_version" in decoded:
            return "forge"
        return base().classify(self, decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "forge":
            return json.dumps(self.forge(decoded), ensure_ascii=False)
        return base().reply(self, kind, decoded, latest)

    def observe_more(self, batch):
        result = base().default_observe(self, batch)
        topics = {SECOND: ("Mindustry 结实的墙", "我还想给 Mindustry 做一面更结实的墙"),
                  THIRD: ("Mindustry 自己的门", "Mindustry 里我还想要一扇自己的门")}
        for evidence in batch["evidence"]:
            found = topics.get(evidence["source"]["user_text"])
            if found:
                result["updates"].append({
                    "target": {"new": {"topic": found[0]}},
                    "statements": [{"kind": "interest", "quote": found[1], "evidence_id": evidence["id"]}]})
        return result

    # 确定性草稿替身：第一份总把类型拼错；看到运行警告或运行前检查的问题后修正。
    def fixing_draft(self, request):
        previous = request.get("previous")
        if previous and (previous["issues"] or (previous["evidence"] and previous["evidence"]["warnings"])):
            return practice_test.mod("Wall")
        return base().default_draft(self, request)

    def toolforge(self):
        return (self.documents().get("eve.toolforge", {}).get("toolforge.v1")
                or {"tools": [], "forges": [], "calls": []})

    def tools(self):
        return self.toolforge()["tools"]

    def forged(self):
        forges = self.toolforge()["forges"]
        return len(forges) == 1 and forges[0]["status"] != "Running"

    def two_warned_practices(self):
        self.inspect_run(lambda: self.wait_for(self.practiced, "first goal was not practiced", 20),
                         prefix=self.send(self.message("casual", MINDUSTRY)), max_executions=4)
        self.assertEqual(self.kind("forge"), [], "只出现一次的问题不锻造")
        self.inspect_run(lambda: self.wait_for(self.forged, "repeated issue was not forged", 25),
                         prefix=self.send(self.message("second", SECOND)), max_executions=4)
        self.assertEqual([run["status"] for run in self.practice_runs()], ["Verified", "Verified"])

    def test_repeated_runtime_warning_becomes_a_tool_that_stops_the_next_draft_before_running(self):
        self.two_warned_practices()
        # 锻造请求不安装工具，只含问题原文与真实草稿文件，不含用户原话；发出前已保存 Running。
        [request] = self.kind("forge")
        self.assertFalse(request["body"].get("tools"))
        content = request["body"]["messages"][-1]["content"]
        body = json.loads(content)
        self.assertIn(WARNING, body["gap"]["summary"])
        self.assertEqual((len(body["failing"]), len(body["passing"])), (2, 2))
        self.assertNotIn("不会写方块", content)
        self.assertNotIn("brief", content)
        rules = request["body"]["messages"][0]["content"]
        for domain in ("Mindustry", "模组", "游戏"):
            self.assertNotIn(domain, rules, "锻造规则不能按领域关键词分支")
        self.assertEqual(request["state"]["eve.toolforge"]["toolforge.v1"]["forges"][0]["status"], "Running")
        [tool] = self.tools()
        self.assertEqual(tool["enabled"], 1)
        self.assertEqual(tool["changes"][0]["actor"], "Automatic")
        examples = tool["versions"][0]["verification"]["examples"]
        self.assertEqual(sorted((item["expect_flag"], bool(item["findings"])) for item in examples),
                         [(False, False), (False, False), (True, True), (True, True)])
        launches = len(self.launches())
        self.assertEqual(launches, 4)

        # 第三项独立实践：第一份草稿在运行前被拦下，没有启动运行环境；修正后实际运行验证通过。
        self.inspect_run(lambda: self.wait_for(lambda: self.practiced(3), "third goal was not practiced", 25),
                         prefix=self.send(self.message("third", THIRD)), max_executions=4)
        third = self.practice_runs()[2]
        self.assertEqual(third["status"], "Verified")
        first, second = third["attempts"]
        self.assertEqual(first["outcome"], "rejected")
        self.assertIsNone(first["evidence"])
        self.assertTrue(first["issues"][0].startswith("[预检] "))
        self.assertIn("工具 block-type 第 1 版", first["issues"][0])
        self.assertEqual(second["outcome"], "verified")
        self.assertEqual(len(self.launches()), launches + 1, "被拦下的草稿没有实际运行")
        retry = json.loads(self.kind("draft")[-1]["body"]["messages"][-1]["content"])
        self.assertTrue(retry["previous"]["issues"][0].startswith("[预检] "))
        calls = self.toolforge()["calls"]
        self.assertEqual([(call["attempt"], bool(call["findings"])) for call in calls], [(1, True), (2, False)])
        self.assertEqual(len(self.kind("forge")), 1, "被拦下的尝试不算新的缺口")

        # 查看与管理：只看自己的工具；停用、回退与启用只改变启用版本并留下记录。重启不重放锻造。
        before = self.toolforge()
        self.run_eve([
            *self.send(self.message("tools", "/tools", contains=[
                "block-type", "启用第 1 版", "1 个版本", "检查 2 次，拦下 1 次", tool["id"],
                "回放验证通过，成为第 1 版", WARNING])),
            *self.send(self.message("show", f"/tool {tool['id']}", contains=[
                "匹配 *.hjson 的文件不能包含“type: Walll”", "出现问题的 2 份拦下 2 份",
                "验证通过的 2 份没有误报 2 份", "验证通过后自动启用第 1 版", "拦下，没有运行",
                "通过，交给运行环境实际运行"])),
            *self.send(self.message("other", f"/tool {tool['id']}", user="user-2", contains="没有这个工具")),
            *self.send(self.message("disable", f"/tool disable {tool['id']}", contains="已停用「block-type」")),
            *self.send(self.message("rollback", f"/tool rollback {tool['id']}", contains="没有可以回退的更早版本")),
            *self.send(self.message("enable", f"/tool enable {tool['id']} 1",
                                    contains="已启用「block-type」第 1 版")),
            *self.send(self.message("missing", f"/tool enable {tool['id']} 9", contains="没有这个已验证版本")),
            *self.send(self.message("help", "/tool", contains="用法：/tools")),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        after = self.toolforge()
        self.assertEqual([(change["actor"], change["enabled"]) for change in after["tools"][0]["changes"]],
                         [("Automatic", 1), ("Owner", None), ("Owner", 1)])
        self.assertEqual((after["forges"], after["calls"]), (before["forges"], before["calls"]))
        self.assertEqual(len(self.kind("forge")), 1, "重启不重放锻造")

    def test_false_positive_is_not_enabled_and_disabled_forging_makes_no_requests(self):
        # 拦下所有方块文件的规则会误报验证通过的草稿：回放验证不通过，不成为工具。
        self.forge = lambda request: spec("type:")
        self.two_warned_practices()
        [forge] = self.toolforge()["forges"]
        self.assertEqual(forge["status"], "Rejected")
        self.assertEqual(self.tools(), [])
        self.run_eve([
            *self.send(self.message("tools", "/tools", contains=["共 0 个", "回放验证未通过，没有启用"])),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        self.assertEqual(len(self.kind("forge")), 1, "同一出现次数不再锻造")

        # 未开启工具锻造时命令如实说明，不请求锻造器，也不做运行前检查。
        self.run_eve([*self.send(self.message("off", "/tools", contains=DISABLED)),
                      *self.checkpoint("quiet-off")], checkpoints={"quiet-off": self.quiet}, forging=False)
        self.assertEqual(len(self.kind("forge")), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
