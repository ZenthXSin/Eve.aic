"""实际 eve-qqbot 的实践验证验收；连接本地模型替身、本地文档站点、fake bridge 与 Mindustry 服务端替身。

用户只在普通聊天里提一次兴趣；学习目标在研究之后由草稿器做出只含数据文件的最小模组，
运行器在全新目录中实际启动服务端、加载模组并探测内容属性；警告或探测不符时依据证据修正。
各阶段先持久化再执行，失败与中断不重试、不重放。

设置 EVE_MINDUSTRY_SERVER_JAR 时，`test_real_runtime_verifies_the_drafted_mod` 改用真实 Mindustry
无头服务端（需要 PATH 上的 java）；其余用例使用确定性替身，只证明宿主契约与恢复语义，
不代表真实模型的草稿质量。
"""
import json
import os
import pathlib
import subprocess
import sys
import time
import unittest

import research_test
from interest_test import MINDUSTRY, QUIT, WATERCOLOR
from memory_test import BINARY, ROOT
from research_test import WOODWORK

FAKE_SERVER = pathlib.Path(__file__).with_name("fake_mindustry.py")
REAL_JAR = os.environ.get("EVE_MINDUSTRY_SERVER_JAR")
DISABLED = "实践验证未启用。"
MANIFEST = 'name: "eve-sample"\ndisplayName: "Eve Sample"\nauthor: "Eve"\nversion: "1.0"\nminGameVersion: 146\n'
PROBES = [{"subject": "eve-sample-sample-wall", "property": "exists", "expected": "true"},
          {"subject": "eve-sample-sample-wall", "property": "class", "expected": "Wall"},
          {"subject": "eve-sample-sample-wall", "property": "health", "expected": "520"},
          {"subject": "eve-sample-sample-wall", "property": "category", "expected": "defense"}]


def wall(block_type):
    return f"type: {block_type}\nhealth: 520\nsize: 1\nrequirements: [ copper/6 ]\ncategory: defense\n"


def mod(block_type, notes=()):
    return {"applicable": True,
            "files": [{"path": "mod.hjson", "content": MANIFEST},
                      {"path": "content/blocks/sample-wall.hjson", "content": wall(block_type)}],
            "probes": PROBES, "rationale": "最小方块模组", "notes_used": list(notes)}


def base():
    return research_test.ResearchAcceptance


class PracticeAcceptance(unittest.TestCase):
    # 复用研究验收的进程、模型替身、文档站点与持久状态读取；只覆盖分类、应答与启动参数。
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "inspect_run", "interest_state", "interests", "cognition", "goals", "learning_goal",
                  "kind", "quiet", "cognition_user", "default_observe", "observe_three_domains",
                  "default_select", "default_extract", "knowledge", "research_runs"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)
        self.draft = self.default_draft
        self.record = self.work / "launches.jsonl"
        self.hold = None
        self.jar = self.work / "server-release.jar"
        self.jar.write_bytes(b"PK\x03\x04fake mindustry server")

    def tearDown(self):
        base().tearDown(self)

    def run_eve(self, script, research=True, practice=True, real=False, **options):
        extra = ["--research-source", self.seed] if research else []
        if practice and real:
            extra += ["--practice-mindustry-server", REAL_JAR]
        elif practice:
            arguments = [str(FAKE_SERVER), "--record", str(self.record)]
            if self.hold:
                arguments += ["--hold", str(self.hold)]
            extra += ["--practice-mindustry-server", str(self.jar), "--practice-java", sys.executable]
            for argument in arguments:
                extra += ["--practice-java-arg", argument]
        return research_test.interest_test.InterestAcceptance.run_eve(self, script, extra=extra, **options)

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "drafter_version" in decoded and "runner" in decoded:
            return "draft"
        return base().classify(self, decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "draft":
            return json.dumps(self.draft(decoded), ensure_ascii=False)
        return base().reply(self, kind, decoded, latest)

    # 确定性草稿替身：第一次把类型拼错，看到运行警告后修正；与目标无关的领域如实声明不适用。
    def default_draft(self, request):
        if "Mindustry" not in request["task"]["brief"]:
            return {"applicable": False, "files": [], "probes": [],
                    "rationale": "运行环境只能验证该游戏的模组，与这个学习目标无关。", "notes_used": []}
        notes = [note["id"] for note in request["task"]["notes"] if note["source_quoted"]][:1]
        previous = request.get("previous")
        warned = previous and previous["evidence"] and previous["evidence"]["warnings"]
        return mod("Wall" if warned else "Walll", notes)

    def practice(self):
        return self.documents().get("eve.practice", {}).get("practice.v1") or {"runs": []}

    def practice_runs(self):
        return self.practice()["runs"]

    def launches(self):
        if not self.record.exists():
            return []
        return [json.loads(line) for line in self.record.read_text(encoding="utf8").splitlines()]

    def practiced(self, count=1):
        runs = self.practice_runs()
        return len(runs) == count and all(run["status"] not in ("Running",) for run in runs)

    def wait_for(self, predicate, message, seconds):
        deadline = time.monotonic() + seconds
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            self.assertFalse(self.run_release.wait(0.05), message)

    def test_learning_goal_practice_repairs_from_runtime_evidence_and_is_verified_without_replay(self):
        self.inspect_run(lambda: self.wait_for(self.practiced, "learning goal was not practiced", 20),
                         prefix=self.send(self.message("casual", MINDUSTRY)), max_executions=4)
        drafts = self.kind("draft")
        self.assertEqual(len(drafts), 2, "一次修正后验证通过")
        [run] = self.practice_runs()
        [interest] = self.interests()
        goal = self.learning_goal(interest["id"])
        self.assertEqual(run["task"]["goal_id"], goal["id"])
        self.assertEqual(run["status"], "Verified")
        self.assertEqual([attempt["outcome"] for attempt in run["attempts"]], ["failed", "verified"])

        # 研究结束后才实践，资料来自有来源的知识；每次请求前已保存对应尝试。
        research = self.research_runs()
        self.assertEqual(research[0]["status"], "Completed")
        first = json.loads(drafts[0]["body"]["messages"][-1]["content"])
        knowledge_ids = {entry["id"] for entry in self.knowledge()["entries"]}
        self.assertTrue(first["task"]["notes"])
        self.assertTrue({note["id"] for note in first["task"]["notes"]} <= knowledge_ids)
        self.assertIsNone(first["previous"])
        for number, request in enumerate(drafts, start=1):
            saved = request["state"]["eve.practice"]["practice.v1"]["runs"][0]["attempts"][-1]
            self.assertEqual((saved["number"], saved["stage"]), (number, "Drafting"))
            self.assertFalse(request["body"].get("tools"), "草稿请求不安装工具")
            rules = request["body"]["messages"][0]["content"]
            for domain in ("Mindustry", "模组", "游戏"):
                self.assertNotIn(domain, rules, "草稿规则不能按领域关键词分支")
        second = json.loads(drafts[1]["body"]["messages"][-1]["content"])["previous"]
        self.assertIn("No type 'Walll' found", second["evidence"]["warnings"][0])
        self.assertEqual(second["evidence"]["probes"][1]["actual"], "Block", "依据实际探测值修正")

        failed, verified = run["attempts"]
        self.assertTrue(failed["evidence"]["loaded"], "类型写错时游戏仍报告已加载")
        self.assertFalse(failed["evidence"]["probes"][1]["passed"])
        evidence = verified["evidence"]
        self.assertEqual(evidence["exit"], "Completed")
        self.assertIn("build 160.7", evidence["runtime_version"])
        self.assertTrue(all(result["passed"] for result in evidence["probes"]))
        self.assertEqual(evidence["warnings"], [])
        self.assertEqual(len(evidence["log_sha256"]), 64)
        self.assertIn("sha256:", run["runner"]["runtime"])
        self.assertEqual(verified["draft"]["files"][1]["content"], wall("Wall"), "可复验的产物随记录保存")

        # 运行器在状态目录下的全新目录中启动，家目录也限定在其中；结束后清理。
        launches = self.launches()
        self.assertEqual(len(launches), 2)
        workspace_root = (self.work / "state" / "practice-work").resolve()
        for launch in launches:
            cwd = pathlib.Path(launch["cwd"]).resolve()
            self.assertTrue(cwd.is_relative_to(workspace_root))
            self.assertEqual(pathlib.Path(launch["home"]).resolve(), cwd)
            self.assertFalse(cwd.exists(), "运行后清理工作目录")
        before = self.practice()

        self.run_eve([
            *self.send(self.message("own", f"/practice {interest['id']}", contains=[
                "Mindustry 模组创作", "已验证", "尝试 1：未通过", "警告：", "No type 'Walll' found",
                "探测 eve-sample-sample-wall.class：期望 Wall，实际 Block ✗",
                "尝试 2：已验证", "探测 eve-sample-sample-wall.class：期望 Wall，实际 Wall ✓",
                "产物：mod.hjson、content/blocks/sample-wall.hjson", "依据资料 1 条", "sha256:"])),
            *self.send(self.message("other", f"/practice {interest['id']}", user="user-2",
                                    contains="当前会话没有这条兴趣")),
            *self.send(self.message("help", "/practice", contains="用法：/practice")),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        self.assertEqual(len(self.kind("draft")), 2, "restart must not replay drafting")
        self.assertEqual(len(self.launches()), 2, "restart must not rerun the runtime")
        self.assertEqual(self.practice(), before)

    def test_held_out_domain_is_not_applicable_and_withdrawn_interest_is_not_practiced(self):
        self.inspect_run(lambda: self.wait_for(self.practiced, "held-out goal was not decided", 15),
                         prefix=self.send(self.message("water", WATERCOLOR)), max_executions=4)
        [run] = self.practice_runs()
        self.assertEqual(run["status"], "NotApplicable")
        self.assertEqual(run["attempts"][0]["outcome"], "not_applicable")
        self.assertEqual(self.launches(), [], "不适用时从不启动运行环境")
        systems = {request["body"]["messages"][0]["content"] for request in self.kind("draft")}
        self.assertEqual(len(systems), 1)
        [interest] = self.interests()
        self.run_eve(self.send(self.message("list", f"/practice {interest['id']}",
                                            contains="运行环境与这条兴趣无关")))

        # 先撤回再派生：取消的目标不实践。
        self.inspect_run(lambda: (self.wait_for(lambda: any(item["status"] == "Withdrawn"
                                                            for item in self.interests()), "not withdrawn", 15),
                                  self.quiet()),
                         prefix=[*self.send(self.message("casual", MINDUSTRY)), *self.send(self.message("quit", QUIT))],
                         research=False, max_executions=4)
        mindustry = next(item for item in self.interests() if item["status"] == "Withdrawn")
        goal = self.learning_goal(mindustry["id"])
        practiced_goals = {run["task"]["goal_id"] for run in self.practice_runs()}
        if goal["id"] in practiced_goals:
            # 撤回之前可能已经开始；开始后的实践照常记录，但撤回后不会再有新的实践。
            self.assertEqual(sum(run["task"]["goal_id"] == goal["id"] for run in self.practice_runs()), 1)
        count = len(self.practice_runs())
        self.run_eve(self.checkpoint("quiet"), checkpoints={"quiet": self.quiet}, research=False)
        self.assertEqual(len(self.practice_runs()), count)

    def test_rejected_scripts_invalid_drafts_and_exhausted_attempts_never_retry(self):
        def scripted(request):
            if request["attempt"] == 1:
                draft = mod("Wall")
                draft["files"].append({"path": "scripts/main.js", "content": "print('x')"})
                return draft
            return "not a draft"

        self.draft = scripted
        self.inspect_run(lambda: self.wait_for(self.practiced, "invalid draft was not recorded", 15),
                         prefix=self.send(self.message("casual", MINDUSTRY)), research=False, max_executions=4)
        [run] = self.practice_runs()
        self.assertEqual(run["status"], {"Failed": "InvalidOutput"})
        rejected, failed = run["attempts"]
        self.assertEqual(rejected["outcome"], "rejected")
        self.assertIn("不允许的文件：scripts/main.js", rejected["issues"][0])
        self.assertIsNone(rejected["evidence"])
        self.assertEqual(failed["outcome"], {"draft_failed": {"failure": "InvalidOutput"}})
        self.assertEqual(self.launches(), [], "结构检查不通过的产物从不运行")

        self.draft = lambda request: mod("Walll")
        self.inspect_run(lambda: self.wait_for(lambda: self.practiced(2), "attempts were not exhausted", 20),
                         prefix=self.send(self.message("wood", WOODWORK)), research=False, max_executions=4)
        exhausted = self.practice_runs()[1]
        self.assertEqual(exhausted["status"], "Unverified")
        self.assertEqual([attempt["outcome"] for attempt in exhausted["attempts"]], ["failed"] * 3)
        self.assertEqual(len(self.launches()), 3)
        self.run_eve(self.checkpoint("quiet"), checkpoints={"quiet": self.quiet}, research=False)
        self.assertEqual(len(self.kind("draft")), 5, "失败的实践不重试")
        self.assertEqual(len(self.launches()), 3)

    def test_interrupted_runtime_is_interrupted_after_restart_without_replay(self):
        self.hold = self.gate("never-released")
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)),
                      {"wait_file": str(self.gate("never-released"))}], research=False, stop_at=self.record)
        self.hold = None
        [run] = self.practice_runs()
        self.assertEqual((run["status"], run["attempts"][0]["stage"]), ("Running", "Running"))
        leftover = self.work / "state" / "practice-work" / run["id"]
        self.assertTrue(leftover.exists(), "进程被强制结束时留下工作目录")
        self.run_eve(self.checkpoint("quiet"), checkpoints={"quiet": self.quiet}, research=False)
        [run] = self.practice_runs()
        self.assertEqual(run["status"], "Interrupted")
        self.assertIsNone(run["attempts"][0]["outcome"])
        self.assertIsNone(run["finished_at_ms"])
        self.assertEqual(len(self.launches()), 1, "interrupted runtime is not replayed")
        self.assertEqual(len(self.kind("draft")), 1)
        self.assertFalse(leftover.exists(), "重启时清理上次中断留下的工作目录")

    def test_practice_is_disabled_by_default_and_invalid_runtime_is_refused(self):
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)),
                      *self.send(self.message("practice", "/practice interest-x", expected=DISABLED)),
                      *self.checkpoint("quiet")], research=False, practice=False,
                     checkpoints={"quiet": self.quiet})
        self.assertEqual(self.kind("draft"), [])
        self.assertNotIn("eve.practice", self.documents())

        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret")
        not_jar = self.work / "not-a-jar.jar"
        not_jar.write_text("#!/bin/sh\n", encoding="utf8")
        for extra, expected in ((["--practice-mindustry-server", str(self.jar)], "--interest-learning"),
                                (["--interest-learning", "--practice-java", "java"], "--practice-mindustry-server"),
                                (["--interest-learning", "--practice-mindustry-server", str(not_jar)], "实践运行环境无效")):
            result = subprocess.run([str(BINARY), "--state-dir", str(self.work / "refused"),
                                     "--agent", str(ROOT / "AGENT.md"), *extra],
                                    env=env, capture_output=True, text=True, timeout=60)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)
        self.assertEqual(self.launches(), [])

    @unittest.skipUnless(REAL_JAR, "需要 EVE_MINDUSTRY_SERVER_JAR 与 PATH 上的 java")
    def test_real_runtime_verifies_the_drafted_mod(self):
        self.inspect_run(lambda: self.wait_for(self.practiced, "real runtime practice did not finish", 150),
                         prefix=self.send(self.message("casual", MINDUSTRY)), research=False, real=True,
                         max_executions=4, timeout=240)
        [run] = self.practice_runs()
        self.assertEqual(run["status"], "Verified", json.dumps(run, ensure_ascii=False)[:4000])
        failed, verified = run["attempts"]
        self.assertIn("No type 'Walll' found", failed["evidence"]["warnings"][0])
        self.assertEqual(failed["evidence"]["probes"][1]["actual"], "Block")
        self.assertRegex(verified["evidence"]["runtime_version"], r"Mindustry .* build \d+")
        self.assertTrue(all(result["passed"] for result in verified["evidence"]["probes"]))
        print(verified["evidence"]["log_excerpt"])


if __name__ == "__main__":
    unittest.main()
