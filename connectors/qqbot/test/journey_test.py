"""首个自主学习目标的完整行为链验收：每个阶段都是一个独立的 eve-qqbot 进程。

用户只在私聊里随口提一次兴趣。之后 Eve 自行研究、实践验证、固化技能，在用户下次找它时附带邀请；
用户提出想法后，Eve 选用技能按想法做出新产物并实际验证，再告诉用户；最后用户撤回兴趣。阶段之间
进程退出、重新启动，核对已完成的步骤不重放、记录完整保留。

默认使用模型替身、桥接替身与 Mindustry 服务端替身；设置 EVE_MINDUSTRY_SERVER_JAR 时改用真实
Mindustry 服务端运行实践、技能验证与后续调用。模型与 QQ 平台始终是替身：这里不评估真实模型的
质量，也不是正式 QQ 的体验验收。
"""
import json
import time
import unittest

import outreach_test
from interest_test import MINDUSTRY, QUIT
from outreach_test import CHAT, IDEA, LATER, WISH, invitation_text
from practice_test import REAL_JAR


class JourneyAcceptance(outreach_test.OutreachAcceptance):
    # 复用主动交流验收的进程、替身与状态读取；不重复运行它的用例。
    for _name in dir(outreach_test.OutreachAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    COOLDOWN = 1000

    def stage(self, script, checkpoints=None, seconds=60):
        real = REAL_JAR is not None
        # 检查点的门文件在同一用例的各次运行之间保留，每个阶段用自己的名字。
        settled = f"settled-{self.runs + 1}"
        checkpoints = dict(checkpoints or {}, **{settled: self.quiet})
        self.run_eve([*script, *self.checkpoint(settled)], checkpoints=checkpoints, research=True,
                     skills=True, real=real, cooldown=self.COOLDOWN, max_executions=8,
                     timeout=seconds * (5 if real else 1))

    def counts(self):
        kinds = ("interest", "select", "extract", "draft", "distill", "skill_select", "compose", "judge", "respond")
        return {kind: len(self.kind(kind)) for kind in kinds}

    def runtime_launches(self):
        # 真实服务端没有启动记录；用实践与技能账本里的运行证据计数。
        attempts = sum(1 for run in self.practice_runs() for attempt in run["attempts"] if attempt.get("evidence"))
        holdouts = sum(1 for entry in self.skill_ledger()["distillations"] if entry.get("evidence"))
        return attempts + holdouts

    def skill_ledger(self):
        return self.documents().get("eve.skill", {}).get("skill.v1") or {
            "skills": [], "distillations": [], "selections": []}

    # 阶段之间没有运行中的进程，直接等上一条送达后的冷却期过去。
    def wait_cooled(self, invitation):
        time.sleep(max(invitation["delivered_at_ms"] + self.COOLDOWN + 300 - time.time() * 1000, 0) / 1000)

    def test_one_mention_leads_to_learning_practice_skill_invitation_and_follow_up_across_restarts(self):
        real = REAL_JAR is not None
        report = {"runtime": "real Mindustry" if real else "deterministic double"}

        # 阶段 1：只提一次兴趣；之后没有任何消息，后台研究、实践、固化技能并撰写邀请。
        self.stage([*self.send(self.message("casual", MINDUSTRY)), *self.checkpoint("learned")],
                   {"learned": lambda: self.wait_status("Pending", seconds=200 if real else 40)})
        [interest] = self.interests()
        casual = next(item for item in self.interactions() if item["source"]["message_id"] == "casual")
        self.assertEqual({statement["evidence_id"] for statement in interest["statements"]}, {casual["id"]},
                         "兴趣追溯到那一条真实交互")
        learning = self.learning_goal(interest["id"])
        self.assertEqual(learning["source"]["kind"], "Inference")
        quoted = [entry for entry in self.knowledge()["entries"] if entry["status"] == "SourceQuoted"]
        self.assertTrue(any(entry["version"] == "v146" and entry["source"] for entry in quoted),
                        "有来源、适用版本与验证状态的领域知识")
        [practice] = self.practice_runs()
        self.assertEqual(practice["status"], "Verified")
        verified = practice["attempts"][-1]["evidence"]
        self.assertTrue(all(result["passed"] for result in verified["probes"]))
        skills = self.skill_ledger()
        [skill] = skills["skills"]
        [distillation] = skills["distillations"]
        self.assertEqual((distillation["status"], skill["enabled"]), ("Verified", 1), "验证通过后自动启用")
        [invitation] = self.invitations()
        self.assertEqual(invitation["milestone"], {"practice_run_id": practice["id"], "skill_id": skill["id"]})
        self.assertEqual(self.pushes(), [], "未开启主动私聊时从不主动发送")
        learned = self.counts()
        launches = self.runtime_launches()
        report["learned"] = {"requests": learned, "runtime_runs": launches,
                             "runtime_version": verified["runtime_version"]}

        # 阶段 2：重启后用户找 Eve 闲聊；判断时机合适，邀请随被动回复附带，以平台回执为准。
        self.stage(self.send_invited("chat", CHAT))
        invitation = next(item for item in self.invitations() if item["goal_id"] == learning["id"])
        self.assertEqual(invitation["status"], "Delivered")
        self.assertEqual(self.receipt("chat")["segments"]["parts"][-1]["state"], "Sent")
        invited = self.counts()
        # 新消息照常经兴趣观察；研究、实践、提炼与撰写都不重放。
        replayable = ("select", "extract", "draft", "distill", "skill_select", "compose", "respond")
        self.assertEqual({kind: invited[kind] for kind in replayable},
                         {kind: learned[kind] for kind in replayable}, "重启后不重放研究、实践、提炼与撰写")
        self.assertEqual(invited["judge"], learned["judge"] + 1)
        self.assertEqual(self.runtime_launches(), launches)

        # 阶段 3：重启后用户提出想法；Eve 选用技能按想法实际做成，并撰写新的邀请。
        self.stage([*self.send(self.message("idea", IDEA)), *self.checkpoint("followed")],
                   {"followed": lambda: self.wait_for(
                       lambda: any(item["goal_id"] != learning["id"] and item["status"] == "Pending"
                                   for item in self.invitations()),
                       "follow-up was not invited", 200 if real else 40)})
        [follow_goal] = self.request_goals()
        self.assertEqual(json.loads(follow_goal["wait_reason"])["learning_goal_id"], learning["id"])
        follow_up = next(run for run in self.practice_runs() if run["task"]["goal_id"] == follow_goal["id"])
        [selection] = self.skill_ledger()["selections"]
        self.assertEqual((selection["id"], selection["status"], selection["outcome"]),
                         (follow_up["id"], "Chosen", "Verified"), "后续独立任务实际调用技能成功")
        self.assertTrue(all(result["passed"] for result in follow_up["attempts"][0]["evidence"]["probes"]))
        followed = self.counts()
        self.assertEqual((followed["respond"], followed["distill"], followed["compose"]),
                         (invited["respond"] + 1, invited["distill"], invited["compose"] + 1),
                         "识别一次回应、撰写一条新邀请；技能实例不再提炼")

        # 阶段 4：重启后用户再来闲聊，邀请引用用户的想法告诉他做好了。
        reported = next(item for item in self.invitations() if item["goal_id"] == follow_goal["id"])
        self.wait_cooled(invitation)
        self.stage([{"send": self.message("later", LATER, expected=None, contains=None) | {
                        "expected_segments": [LATER, invitation_text(WISH)]}},
                    {"wait_command": {"id": "later", "type": "segment", "count": 2}}, self.wait_receipt("later"),
                    *self.send(self.message("status", "/outreach", contains=[
                        "你的回应：提出了想法（“加一个会发光的墙”）", "随你的消息回复附带，平台已确认"])),
                    *self.send(self.message("skills", "/skills", contains="调用 1 次，验证通过 1 次"))])
        reported = next(item for item in self.invitations() if item["id"] == reported["id"])
        self.assertEqual(reported["status"], "Delivered")
        self.assertIn({"kind": "UserQuote", "text": WISH}, reported["facts"])

        # 阶段 5：重启后用户撤回兴趣；学习目标与后续目标取消，已有知识、实践与技能记录保留。
        kept = (self.knowledge()["entries"], self.practice_runs(), self.skill_ledger()["skills"])
        self.stage([*self.send(self.message("quit", QUIT)), *self.checkpoint("withdrawn")],
                   {"withdrawn": lambda: self.wait_for(
                       lambda: self.learning_goal(interest["id"])["status"] == "Cancelled",
                       "learning goal was not cancelled", 30)})
        self.assertEqual(self.interests()[0]["status"], "Withdrawn")
        self.assertEqual(self.request_goals()[0]["status"], "Cancelled")
        self.assertEqual((self.knowledge()["entries"], self.practice_runs(), self.skill_ledger()["skills"]), kept)
        report["final"] = {"requests": self.counts(), "runtime_runs": self.runtime_launches(),
                           "invitations": [item["status"] for item in self.invitations()]}
        print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    unittest.main()
