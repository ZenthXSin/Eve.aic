"""实际 eve-qqbot 的技能固化与复用验收；连接本地模型替身、fake bridge 与 Mindustry 服务端替身。

用户只在普通聊天里提一次兴趣：学习目标实践验证通过后，提炼器把产物改写为参数化模板，
宿主核对模板能逐字还原原产物，再用自己选取的、与原值不同的参数在同一运行环境中实际运行，
通过才启用技能。之后同一用户的另一项独立任务由选择器选用这个技能，实例实际运行通过即调用成功。
各阶段先持久化再执行，失败与中断不重试、不重放。

设置 EVE_MINDUSTRY_SERVER_JAR 时，`test_real_runtime_verifies_skill_holdout_and_invocation` 改用真实
Mindustry 无头服务端；其余用例使用确定性替身，只证明宿主契约与恢复语义，不代表真实模型的提炼质量。
"""
import json
import os
import pathlib
import re
import subprocess
import unittest

import practice_test
from interest_test import MINDUSTRY
from memory_test import BINARY, ROOT

REAL_JAR = practice_test.REAL_JAR
SECOND = "我还想给 Mindustry 做一面更结实的墙，可还是不会写方块。"
DISABLED = "技能固化未启用。"
EMPTY = "还没有固化的技能"
NOT_FOUND = "没有这个技能"


def base():
    return practice_test.PracticeAcceptance


class SkillAcceptance(unittest.TestCase):
    # 复用实践验收的进程、模型替身、服务端替身与持久状态读取；只覆盖分类、应答与启动参数。
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "inspect_run", "interest_state", "interests", "cognition", "goals", "learning_goal",
                  "kind", "quiet", "cognition_user", "default_observe", "observe_three_domains", "default_select",
                  "default_extract", "knowledge",
                  "research_runs", "default_draft", "practice", "practice_runs", "launches", "practiced",
                  "wait_for"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)
        self.observe = self.observe_second
        self.distill = self.default_distill
        self.skill_select = self.default_skill_select

    def tearDown(self):
        base().tearDown(self)

    def run_eve(self, script, skills=True, **options):
        more = ["--skill-learning"] if skills else []
        options.setdefault("research", False)
        return base().run_eve(self, script, more=more, **options)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "distillation_id" in decoded:
            return "distill"
        if isinstance(decoded, dict) and "selection_id" in decoded:
            return "skill_select"
        return base().classify(self, decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "distill":
            return json.dumps(self.distill(decoded), ensure_ascii=False)
        if kind == "skill_select":
            return json.dumps(self.skill_select(decoded), ensure_ascii=False)
        return base().reply(self, kind, decoded, latest)

    def observe_second(self, batch):
        result = base().default_observe(self, batch)
        for evidence in batch["evidence"]:
            if evidence["source"]["user_text"] == SECOND:
                result["updates"].append({
                    "target": {"new": {"topic": "Mindustry 结实的墙"}},
                    "statements": [{"kind": "interest", "quote": "我还想给 Mindustry 做一面更结实的墙",
                                    "evidence_id": evidence["id"]}]})
        return result

    # 确定性提炼替身：把方块名与生命值参数化，其余逐字保留；已有同名技能时作为它的新版本。
    def default_distill(self, request, original=None):
        source = request["source"]
        block = next(item for item in source["files"] if item["path"].startswith("content/blocks/"))
        name = block["path"].rsplit("/", 1)[1].removesuffix(".hjson")
        health = re.search(r"health: (\d+)", block["content"]).group(1)
        files = [item if item is not block else
                 {"path": "content/blocks/{{block}}.hjson",
                  "content": block["content"].replace(f"health: {health}", "health: {{health}}")}
                 for item in source["files"]]
        probes = [{"subject": probe["subject"].replace(name, "{{block}}"), "property": probe["property"],
                   "expected": "{{health}}" if probe["property"] == "health" else probe["expected"]}
                  for probe in source["probes"]]
        existing = request["existing"]
        return {"skill": {
            "extends": existing[0]["skill_id"] if existing else None,
            "name": "sturdy-wall",
            "template": {"title": "指定生命值的墙", "summary": "生成一个指定名称与生命值的墙方块",
                         "parameters": [
                             {"name": "block", "description": "方块名称", "kind": "identifier"},
                             {"name": "health", "description": "生命值", "kind": {"integer": {"min": 100, "max": 2000}}}],
                         "files": files, "probes": probes},
            "arguments": {"block": name, "health": original or health}}}

    # 确定性选择替身：与 Mindustry 相关的任务选用第一个候选技能，其余不选。
    def default_skill_select(self, request):
        if "Mindustry" not in request["task"]["brief"]:
            return {"skill": None}
        candidate = request["candidates"][0]
        return {"skill": {"skill_id": candidate["skill_id"], "version": candidate["version"],
                          "arguments": {"block": "sturdy-wall", "health": "1200"},
                          "reason": "任务需要一面指定生命值的墙"}}

    def ledger(self):
        return (self.documents().get("eve.skill", {}).get("skill.v1")
                or {"skills": [], "distillations": [], "selections": []})

    def skills(self):
        return self.ledger()["skills"]

    def distillations(self):
        return self.ledger()["distillations"]

    def selections(self):
        return self.ledger()["selections"]

    def distilled(self, count=1):
        entries = self.distillations()
        return len(entries) == count and all(entry["status"] != "Running" for entry in entries)

    def invoked(self):
        return any(selection.get("outcome") for selection in self.selections())

    def learn_skill(self, **options):
        self.inspect_run(lambda: self.wait_for(self.distilled, "practice was not distilled", 25),
                         prefix=self.send(self.message("casual", MINDUSTRY)), max_executions=4, **options)

    def test_verified_practice_becomes_a_skill_that_a_later_independent_task_invokes(self):
        self.learn_skill()
        [practice] = self.practice_runs()
        self.assertEqual(practice["status"], "Verified")
        [distillation] = self.distillations()
        self.assertEqual(distillation["status"], "Verified")
        [skill] = self.skills()
        self.assertEqual((skill["enabled"], skill["owner"]), (1, practice["task"]["owner"]))
        self.assertEqual(skill["changes"][0]["actor"], "Automatic")

        # 提炼请求只含已验证产物与证据，不含用户原话；发出前已保存 Running/Proposing。
        [request] = self.kind("distill")
        self.assertFalse(request["body"].get("tools"))
        content = request["body"]["messages"][-1]["content"]
        self.assertNotIn("不知道怎么创作", content)
        self.assertNotIn("brief", json.loads(content))
        rules = request["body"]["messages"][0]["content"]
        for domain in ("Mindustry", "模组", "游戏"):
            self.assertNotIn(domain, rules, "提炼规则不能按领域关键词分支")
        saved = request["state"]["eve.skill"]["skill.v1"]["distillations"][0]
        self.assertEqual((saved["status"], saved["stage"]), ("Running", "Proposing"))

        # 宿主选取的验证参数：标识加后缀、整数取边界；验证运行在全新目录中，用后清理。
        self.assertEqual(distillation["holdout"], {"block": "sample-wall-b", "health": "2000"})
        evidence = distillation["evidence"]
        self.assertTrue(all(result["passed"] for result in evidence["probes"]))
        self.assertIn({"subject": "eve-sample-sample-wall-b", "property": "health", "expected": "2000"},
                      [result["probe"] for result in evidence["probes"]])
        launches = self.launches()
        self.assertEqual(len(launches), 3, "两次实践尝试加一次验证运行")
        holdout = pathlib.Path(launches[-1]["cwd"]).resolve()
        self.assertTrue(holdout.is_relative_to((self.work / "state" / "practice-work" / distillation["id"]).resolve()))
        self.assertFalse(holdout.exists())

        # 重启后，同一用户的另一项独立任务选用技能：第一次尝试直接用实例，不请求草稿器。
        drafts = len(self.kind("draft"))
        self.inspect_run(lambda: self.wait_for(self.invoked, "skill was not invoked", 25),
                         prefix=self.send(self.message("second", SECOND)), max_executions=4)
        self.assertEqual(len(self.kind("draft")), drafts)
        [select] = self.kind("skill_select")
        self.assertEqual(select["state"]["eve.skill"]["skill.v1"]["selections"][0]["status"], "Running")
        self.assertEqual([candidate["skill_id"] for candidate in json.loads(
            select["body"]["messages"][-1]["content"])["candidates"]], [skill["id"]])
        [selection] = self.selections()
        self.assertEqual((selection["status"], selection["outcome"]), ("Chosen", "Verified"))
        second = next(run for run in self.practice_runs() if run["id"] == selection["id"])
        self.assertEqual(second["status"], "Verified")
        self.assertEqual(second["attempts"][0]["draft"]["files"][1]["path"], "content/blocks/sturdy-wall.hjson")
        self.assertIn("health: 1200", second["attempts"][0]["draft"]["files"][1]["content"])
        self.assertEqual(len(self.launches()), 4)

        interest = next(item for item in self.interests() if item["topic"] == "Mindustry 结实的墙")
        before = self.ledger()
        self.run_eve([
            *self.send(self.message("skills", "/skills", contains=[
                "sturdy-wall「指定生命值的墙」", "启用第 1 版", "1 个版本", "调用 1 次，验证通过 1 次", skill["id"]])),
            *self.send(self.message("show", f"/skill {skill['id']}", contains=[
                "第 1 版「指定生命值的墙」", "参数：block（标识）、health（整数 100～2000）",
                "验证参数：block=sample-wall-b，health=2000", "已验证",
                "探测 eve-sample-sample-wall-b.health：期望 2000，实际 2000 ✓",
                "验证通过后自动启用第 1 版", "参数 block=sturdy-wall，health=1200｜实际运行验证通过"])),
            *self.send(self.message("practice", f"/practice {interest['id']}", contains=[
                "尝试 1：已验证", "由已验证技能的模板实例化：「指定生命值的墙」第 1 版"])),
            *self.send(self.message("other", f"/skill {skill['id']}", user="user-2", expected=NOT_FOUND + "。发送 /skills 查看技能 ID。")),
            *self.send(self.message("empty", "/skills", user="user-2", contains=EMPTY)),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        # 技能实例验证通过的实践不再提炼；重启不重放任何请求或运行。
        self.assertEqual(len(self.kind("distill")), 1)
        self.assertEqual(len(self.kind("skill_select")), 1)
        self.assertEqual(len(self.launches()), 4)
        self.assertEqual(self.ledger(), before)

    def test_owner_disable_holds_new_versions_and_enable_or_rollback_restore_them(self):
        self.learn_skill()
        [skill] = self.skills()
        # 停用后，独立任务不再选择技能而是照常草稿；新验证的版本也保持停用。
        self.inspect_run(lambda: self.wait_for(lambda: self.distilled(2), "second practice was not distilled", 25),
                         prefix=[*self.send(self.message("disable", f"/skill disable {skill['id']}", contains="已停用「sturdy-wall」")),
                                 *self.send(self.message("second", SECOND))], max_executions=4)
        self.assertEqual(self.kind("skill_select"), [])
        self.assertEqual(len(self.kind("draft")), 4)
        [skill] = self.skills()
        self.assertEqual((skill["enabled"], len(skill["versions"])), (None, 2))
        self.assertEqual(self.distillations()[1]["skill"], {"skill_id": skill["id"], "version": 2})

        self.run_eve([
            *self.send(self.message("enable", f"/skill enable {skill['id']} 2", expected="已启用「sturdy-wall」第 2 版。")),
            *self.send(self.message("rollback", f"/skill rollback {skill['id']}", expected="已启用「sturdy-wall」第 1 版。")),
            *self.send(self.message("again", f"/skill rollback {skill['id']}", expected="没有可以回退的更早版本。")),
            *self.send(self.message("missing", f"/skill enable {skill['id']} 7", expected="没有这个已验证版本。")),
            *self.send(self.message("other", f"/skill disable {skill['id']}", user="user-2",
                                    expected=NOT_FOUND + "。发送 /skills 查看技能 ID。")),
            *self.send(self.message("help", "/skill", contains="用法：/skills")),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        [skill] = self.skills()
        self.assertEqual(skill["enabled"], 1)
        self.assertEqual([(change["actor"], change["enabled"]) for change in skill["changes"]],
                         [("Automatic", 1), ("Owner", None), ("Owner", 2), ("Owner", 1)])

    def test_unfaithful_templates_are_rejected_without_running(self):
        self.distill = lambda request: self.default_distill(request, original="999")
        self.learn_skill()
        [distillation] = self.distillations()
        self.assertEqual(distillation["status"], "Rejected")
        self.assertIn("逐字还原", distillation["issues"][0])
        self.assertIsNone(distillation["holdout"])
        self.assertEqual(self.skills(), [])
        self.assertEqual(len(self.launches()), 2, "被拒绝的模板从不运行")
        self.run_eve([*self.send(self.message("skills", "/skills", contains=EMPTY)), *self.checkpoint("quiet")],
                     checkpoints={"quiet": self.quiet})
        self.assertEqual(len(self.kind("distill")), 1, "被拒绝的提炼不重试")

    def test_interrupted_distillation_is_interrupted_after_restart_without_replay(self):
        self.response_gates = {("distill", 1): "never-released"}
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)),
                      {"wait_file": str(self.gate("never-released"))}], stop_at=self.arrived("distill", 1))
        [distillation] = self.distillations()
        self.assertEqual((distillation["status"], distillation["stage"]), ("Running", "Proposing"))
        self.response_gates = {}
        self.run_eve(self.checkpoint("quiet"), checkpoints={"quiet": self.quiet})
        [distillation] = self.distillations()
        self.assertEqual(distillation["status"], "Interrupted")
        self.assertIsNone(distillation["finished_at_ms"])
        self.assertEqual(len(self.kind("distill")), 1, "interrupted distillation is not replayed")
        self.assertEqual(self.skills(), [])
        self.assertEqual(len(self.launches()), 2)

    def test_skill_learning_is_disabled_by_default_and_requires_a_practice_runtime(self):
        self.inspect_run(lambda: self.wait_for(self.practiced, "practice did not finish", 20),
                         prefix=[*self.send(self.message("casual", MINDUSTRY)),
                                 *self.send(self.message("skills", "/skills", expected=DISABLED))],
                         skills=False, max_executions=4)
        self.run_eve(self.checkpoint("quiet"), skills=False, checkpoints={"quiet": self.quiet})
        self.assertEqual(self.kind("distill"), [])
        self.assertNotIn("eve.skill", self.documents())

        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret")
        result = subprocess.run([str(BINARY), "--state-dir", str(self.work / "refused"),
                                 "--agent", str(ROOT / "AGENT.md"), "--interest-learning", "--skill-learning"],
                                env=env, capture_output=True, text=True, timeout=60)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--practice-mindustry-server", result.stderr)

    @unittest.skipUnless(REAL_JAR, "需要 EVE_MINDUSTRY_SERVER_JAR 与 PATH 上的 java")
    def test_real_runtime_verifies_skill_holdout_and_invocation(self):
        self.inspect_run(lambda: self.wait_for(self.distilled, "real skill was not distilled", 200),
                         prefix=self.send(self.message("casual", MINDUSTRY)), real=True,
                         max_executions=4, timeout=300)
        [distillation] = self.distillations()
        self.assertEqual(distillation["status"], "Verified", json.dumps(distillation, ensure_ascii=False)[:4000])
        self.assertRegex(distillation["evidence"]["runtime_version"], r"Mindustry .* build \d+")
        self.inspect_run(lambda: self.wait_for(self.invoked, "real skill was not invoked", 120),
                         prefix=self.send(self.message("second", SECOND)), real=True,
                         max_executions=4, timeout=200)
        [selection] = self.selections()
        self.assertEqual(selection["outcome"], "Verified")
        second = next(run for run in self.practice_runs() if run["id"] == selection["id"])
        evidence = second["attempts"][0]["evidence"]
        self.assertIn({"subject": "eve-sample-sturdy-wall", "property": "health", "expected": "1200"},
                      [result["probe"] for result in evidence["probes"]])
        self.assertTrue(all(result["passed"] for result in evidence["probes"]))
        print(evidence["log_excerpt"])


if __name__ == "__main__":
    unittest.main()
