"""首个自主学习目标的留出场景验收：换成另一个创作领域（个人主页），复用同一组契约。

用户只在私聊里随口提一次想做个人主页。宿主不加任何领域分支：兴趣观察、学习目标、实践、技能、
邀请、回应识别与后续创作都走与 Mindustry 相同的契约，只把实践运行环境换成操作者提供的无头
Chromium（`--practice-browser`），在全新目录中实际打开 HTML/CSS 并探测元素的计算样式与文字。

模型与 QQ 平台是替身：替身只按宿主交给它的数据作答（例如运行器声明的领域与规则），这里核对的是
宿主契约在新领域上的行为与真实浏览器的验证结果，不评估真实模型能否做好网页。需要设置
EVE_TEST_BROWSER 为 Chromium 或 Chrome 程序路径。
"""
import json
import os
import re
import time
import unittest

import outreach_test
from memory_test import BINARY, ROOT
import research_test

BROWSER = os.environ.get("EVE_TEST_BROWSER")
HOMEPAGE = "我最近想给自己做个个人主页，但完全不会写网页。"
HOMEPAGE_QUOTE = "我最近想给自己做个个人主页"
IDEA = "那再加一个联系我的按钮吧！"
WISH = "加一个联系我的按钮"
CHAT = outreach_test.CHAT
LATER = outreach_test.LATER
# 宿主与模型请求中都不应出现首个场景的领域词。
FIRST_DOMAIN = ("Mindustry", "模组", "hjson")


def report(quote):
    return f"你上次说“{quote}”，我后来自己动手做了一版，已经实际打开验证过了。你想再加点什么？"


def page(title, size, button=None):
    extra = f'<a id="{button[0]}" class="button">{button[1]}</a>' if button else ""
    index = ("<!doctype html><html><head><meta charset=\"utf-8\"><title>我的主页</title>"
             "<link rel=\"stylesheet\" href=\"style.css\"></head>"
             f"<body><h1 id=\"{title}\">欢迎来到我的主页</h1><p id=\"intro\">很高兴认识你</p>{extra}</body></html>")
    css = f"#{title} {{ font-size: {size}px; color: rgb(200, 30, 30); }}\n#intro {{ text-align: center; }}\n"
    return [{"path": "index.html", "content": index}, {"path": "style.css", "content": css}]


def probes(title, size):
    return [{"subject": title, "property": "fontSize", "expected": f"{size}px"},
            {"subject": title, "property": "color", "expected": "rgb(200, 30, 30)"},
            {"subject": title, "property": "text", "expected": "欢迎来到我的主页"},
            {"subject": "intro", "property": "textAlign", "expected": "center"}]


@unittest.skipUnless(BROWSER, "需要 EVE_TEST_BROWSER（Chromium 或 Chrome 程序路径）")
class HoldoutAcceptance(outreach_test.OutreachAcceptance):
    # 复用主动交流验收的进程、替身与状态读取；不重复运行它的用例。
    for _name in dir(outreach_test.OutreachAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    COOLDOWN = 1000

    def setUp(self):
        super().setUp()
        self.observe = self.observe_homepage
        self.draft = self.draft_page
        self.compose = lambda request: {"text": report(next(
            fact["text"] for fact in request["facts"] if fact["kind"] == "UserQuote"))}
        self.respond = self.respond_homepage

    def run_eve(self, script, **options):
        extra = ["--practice-browser", BROWSER, "--skill-learning", "--outreach",
                 "--outreach-cooldown-ms", str(self.COOLDOWN)]
        return research_test.interest_test.InterestAcceptance.run_eve(self, script, extra=extra, **options)

    def reply(self, kind, decoded, latest):
        if kind == "distill":
            return json.dumps(self.distill_page(decoded), ensure_ascii=False)
        if kind == "skill_select":
            return json.dumps(self.select_page(decoded), ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def observe_homepage(self, batch):
        updates = []
        for evidence in batch["evidence"]:
            if evidence["source"]["user_text"] == HOMEPAGE:
                updates.append({
                    "target": {"new": {"topic": "个人主页制作"}},
                    "statements": [
                        {"kind": "interest", "quote": HOMEPAGE_QUOTE, "evidence_id": evidence["id"]},
                        {"kind": "difficulty", "quote": "完全不会写网页", "evidence_id": evidence["id"]}]})
        return {"updates": updates}

    # 草稿替身只看运行器声明的领域与上一次的实际证据：第一次字号写错，看到浏览器计算的值后修正。
    def draft_page(self, request):
        if "HTML" not in request["runner"]["domain"]:
            return {"applicable": False, "files": [], "probes": [], "rationale": "运行环境与目标无关。",
                    "notes_used": []}
        previous = request.get("previous")
        failed = previous and previous["evidence"] and any(
            not result["passed"] for result in previous["evidence"]["probes"])
        return {"applicable": True, "files": page("hero-title", 40 if failed else 36),
                "probes": probes("hero-title", 40), "rationale": "一个带标题与简介的个人主页", "notes_used": []}

    # 提炼替身：把标题元素的 id 与字号参数化，其余逐字保留。
    def distill_page(self, request):
        source = request["source"]
        title = source["probes"][0]["subject"]
        size = re.search(r"font-size: (\d+)px", source["files"][1]["content"]).group(1)
        files = [{"path": item["path"],
                  "content": item["content"].replace(title, "{{title}}").replace(f"{size}px", "{{size}}px")}
                 for item in source["files"]]
        template_probes = [{"subject": probe["subject"].replace(title, "{{title}}"), "property": probe["property"],
                            "expected": probe["expected"].replace(f"{size}px", "{{size}}px")}
                           for probe in source["probes"]]
        return {"skill": {
            "extends": None, "name": "homepage-title",
            "template": {"title": "指定标题字号的主页", "summary": "生成带标题与简介的静态主页",
                         "parameters": [
                             {"name": "title", "description": "标题元素的 id", "kind": "identifier"},
                             {"name": "size", "description": "标题字号（像素）",
                              "kind": {"integer": {"min": 16, "max": 72}}}],
                         "files": files, "probes": template_probes},
            "arguments": {"title": title, "size": size}}}

    def select_page(self, request):
        if not request["candidates"]:
            return {"skill": None}
        candidate = request["candidates"][0]
        return {"skill": {"skill_id": candidate["skill_id"], "version": candidate["version"],
                          "arguments": {"title": "contact-title", "size": "48"},
                          "reason": "沿用已验证的主页结构"}}

    def respond_homepage(self, request):
        for turn in reversed(request["turns"]):
            if turn["user_message"] == IDEA:
                return {"kind": "request", "quote": WISH}
        return {"kind": "unrelated"}

    def skill_ledger(self):
        return self.documents().get("eve.skill", {}).get("skill.v1") or {
            "skills": [], "distillations": [], "selections": []}

    def stage(self, script, checkpoints=None, seconds=60):
        settled = f"settled-{self.runs + 1}"
        checkpoints = dict(checkpoints or {}, **{settled: self.quiet})
        self.run_eve([*script, *self.checkpoint(settled)], checkpoints=checkpoints, max_executions=8,
                     timeout=seconds * 3)

    def test_a_different_creative_domain_reuses_the_same_contracts_with_a_real_browser(self):
        # 阶段 1：只提一次想做个人主页；后台实践、固化技能并撰写邀请。
        self.stage([*self.send(self.message("casual", HOMEPAGE)), *self.checkpoint("learned")],
                   {"learned": lambda: self.wait_status("Pending", seconds=120)})
        [interest] = self.interests()
        learning = self.learning_goal(interest["id"])
        [practice] = self.practice_runs()
        self.assertEqual(practice["task"]["goal_id"], learning["id"])
        self.assertEqual(practice["runner"]["runner_id"], "static-web-page:v1")
        failed, verified = practice["attempts"]
        self.assertEqual((failed["outcome"], verified["outcome"]), ("failed", "verified"),
                         "依据浏览器给出的实际值修正一次后验证通过")
        self.assertEqual(failed["evidence"]["probes"][0]["actual"], "36px")
        self.assertRegex(verified["evidence"]["runtime_version"], r"^Chromium \d+")
        self.assertTrue(verified["evidence"]["loaded"])
        skills = self.skill_ledger()
        [skill] = skills["skills"]
        [distillation] = skills["distillations"]
        self.assertEqual((distillation["status"], skill["enabled"]), ("Verified", 1))
        self.assertEqual(distillation["holdout"]["title"], "hero-title-b", "宿主自选的留出参数")
        self.assertTrue(all(result["passed"] for result in distillation["evidence"]["probes"]))

        # 阶段 2：重启后闲聊，邀请附带送达；用户提出想法后，选用技能做出新页面并撰写新邀请。
        [invitation] = self.invitations()
        self.stage([{"send": self.message("chat", CHAT, expected=None, contains=None) | {
                        "expected_segments": [CHAT, report(HOMEPAGE_QUOTE)]}},
                    {"wait_command": {"id": "chat", "type": "segment", "count": 2}}, self.wait_receipt("chat"),
                    *self.send(self.message("idea", IDEA)), *self.checkpoint("followed")],
                   {"followed": lambda: self.wait_for(
                       lambda: any(item["goal_id"] != learning["id"] and item["status"] == "Pending"
                                   for item in self.invitations()), "follow-up was not invited", 120)})
        [follow_goal] = self.request_goals()
        follow_up = next(run for run in self.practice_runs() if run["task"]["goal_id"] == follow_goal["id"])
        [selection] = self.skill_ledger()["selections"]
        self.assertEqual((selection["id"], selection["status"], selection["outcome"]),
                         (follow_up["id"], "Chosen", "Verified"))
        evidence = follow_up["attempts"][0]["evidence"]
        self.assertIn({"subject": "contact-title", "property": "fontSize", "expected": "48px"},
                      [result["probe"] for result in evidence["probes"]])
        self.assertTrue(all(result["passed"] for result in evidence["probes"]))

        # 阶段 3：重启后再闲聊，引用用户想法的新邀请送达。
        delivered = next(item for item in self.invitations() if item["id"] == invitation["id"])
        time.sleep(max(delivered["delivered_at_ms"] + self.COOLDOWN + 300 - time.time() * 1000, 0) / 1000)
        self.stage([{"send": self.message("later", LATER, expected=None, contains=None) | {
                        "expected_segments": [LATER, report(WISH)]}},
                    {"wait_command": {"id": "later", "type": "segment", "count": 2}}, self.wait_receipt("later")])
        self.assertEqual([item["status"] for item in self.invitations()], ["Delivered", "Delivered"])

        # 宿主交给模型的规则与数据里没有首个场景的领域词：同一组契约，没有新增关键词分支。
        for kind in ("interest", "draft", "distill", "skill_select", "compose", "judge", "respond"):
            for request in self.kind(kind):
                body = json.dumps(request["body"], ensure_ascii=False)
                for word in FIRST_DOMAIN:
                    self.assertNotIn(word, body, f"{kind} 请求含有 {word}")
        print(json.dumps({"runtime": verified["evidence"]["runtime_version"],
                          "requests": {kind: len(self.kind(kind)) for kind in (
                              "interest", "draft", "distill", "skill_select", "compose", "judge", "respond")}},
                         ensure_ascii=False))

    def test_the_browser_runtime_is_an_alternative_to_the_game_runtime(self):
        import subprocess
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret")
        for extra, expected in ((["--interest-learning", "--practice-browser", BROWSER,
                                  "--practice-mindustry-server", str(self.jar)], "只能选一个"),
                                (["--practice-browser", BROWSER], "--interest-learning"),
                                (["--interest-learning", "--practice-browser", str(self.work / "missing")],
                                 "实践运行环境无效")):
            result = subprocess.run([str(BINARY), "--state-dir", str(self.work / "refused"),
                                     "--agent", str(ROOT / "AGENT.md"), *extra],
                                    env=env, capture_output=True, text=True, timeout=60)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)


if __name__ == "__main__":
    unittest.main()
