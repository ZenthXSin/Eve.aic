"""实际 eve-qqbot 的学习目标推进与停止验收；连接本地模型替身、本地文档站点、fake bridge 与 Mindustry 服务端替身。

用户在私聊里三次提到同一个兴趣：每次都让学习目标有了新版本，后台各研究一次；前两次各实践一次。
研究 3 次、实践 2 次都用完后，Eve 判断这件事再也没有可以做的，议程上下文与 /interests 如实说明；
开启 --learning-stop 后，后台把目标标为已取消并记下原因，兴趣本身保留。
确定性替身只证明宿主契约，不代表真实模型的理解能力。
"""
import json
import unittest

import agenda_test
import practice_test
from interest_test import MINDUSTRY

MORE = ["我想先做一个能放在地图上的方块", "做完方块以后我还想试试做单位"]
CHAT = ["你那个模组学得怎么样了？", "最后学成什么样了？", "现在还在学吗？"]
WAITING = {"verdict": "wait", "reason": "new_input"}
SPENT = {"verdict": "stop", "reason": "learning_budget_spent"}
STOPPED = {"verdict": "stopped", "reason": "learning_budget_spent"}


class PursuitAcceptance(agenda_test.AgendaAcceptance):
    # 只运行本文件的用例。
    for _name in dir(agenda_test.AgendaAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.observe = self.observe_more

    def run_eve(self, script, stop=True, **options):
        more = ["--agenda-context"] + (["--learning-stop"] if stop else [])
        options.setdefault("max_executions", 4)
        return practice_test.PracticeAcceptance.run_eve(self, script, more=more, **options)

    def observe_more(self, batch):
        """用户再提到这个兴趣时，记为同一条兴趣的新陈述，兴趣与学习目标随之有新版本。"""
        result = self.default_observe(batch)
        for evidence in batch["evidence"]:
            text = evidence["source"]["user_text"]
            if text in MORE and batch["known_interests"]:
                result["updates"].append({
                    "target": {"existing": {"id": batch["known_interests"][0]["id"]}},
                    "statements": [{"kind": "experience", "quote": text, "evidence_id": evidence["id"]}]})
        return result

    def goal(self):
        [interest] = self.interests()
        return self.learning_goal(interest["id"])

    def next(self, text):
        [item] = self.agenda(text)["items"]
        return item["next"]

    def researched(self, count):
        runs = self.research_runs()
        return len(runs) == count and all(run["status"] != "Running" for run in runs)

    def stop_events(self):
        return [event for event in self.cognition()["state"]["events"]
                if event["source"]["channel"] == "eve.pursuit"
                and json.loads(event["summary"]).get("schema") == "pursuit-stop:v1"]

    def test_spent_learning_budget_is_reported_then_stopped_once_with_the_reason(self):
        # 第一版：研究与实践各一次，之后能做的都做了，等用户提到新的想法。
        self.inspect_run(lambda: self.wait_for(lambda: self.researched(1) and self.practiced(1),
                                               "first version was not learned", 20),
                         prefix=self.send(self.message("casual", MINDUSTRY)), stop=False)
        self.run_eve([*self.send(self.message("ask-1", CHAT[0])),
                      *self.send(self.message("list-1", "/interests",
                                              contains="后台学习中（接下来：能做的都做了，等你提到新的想法或情况）"))],
                     stop=False)
        self.assertEqual(self.next(CHAT[0]), WAITING)

        # 再提两次：第二版研究并实践（实践次数用完），第三版只研究（研究次数也用完）。
        self.inspect_run(lambda: self.wait_for(lambda: self.researched(2) and self.practiced(2),
                                               "second version was not learned", 30),
                         prefix=self.send(self.message("more-1", MORE[0])), stop=False)
        self.inspect_run(lambda: self.wait_for(lambda: self.researched(3), "third version was not researched", 30),
                         prefix=self.send(self.message("more-2", MORE[1])), stop=False)
        self.assertEqual(self.goal()["revision"], 3)
        self.assertEqual(len(self.practice_runs()), 2, "实践次数用完后新版本不再实践")

        # 未开启 --learning-stop：如实说明不再推进，但目标保持等待，不写任何状态。
        before = self.cognition()
        self.run_eve([*self.send(self.message("ask-2", CHAT[1])),
                      *self.send(self.message("list-2", "/interests",
                                              contains="后台学习中（接下来：学习次数已用完，不再推进）"))],
                     stop=False)
        self.assertEqual(self.next(CHAT[1]), SPENT)
        self.assertEqual(self.cognition(), before)
        self.assertEqual(self.goal()["status"], "Waiting")

        # 开启后，后台把目标标为已取消，并在同一次替换中记下原因；兴趣本身保留。
        waiting = self.goal()
        self.inspect_run(lambda: self.wait_for(lambda: self.goal()["status"] == "Cancelled",
                                               "spent learning goal was not stopped", 15))
        stopped = self.goal()
        self.assertEqual(stopped["revision"], waiting["revision"] + 1)
        self.assertIsNone(stopped["wait_reason"])
        self.assertEqual((stopped["description"], stopped["stop_condition"]),
                         (waiting["description"], waiting["stop_condition"]))
        [event] = self.stop_events()
        self.assertEqual(event["goal_id"], stopped["id"])
        self.assertEqual(event["source"], {"kind": "Internal", "channel": "eve.pursuit", "reference": stopped["id"]})
        record = json.loads(event["summary"])
        self.assertEqual(record, {"schema": "pursuit-stop:v1", "reason": "learning_budget_spent",
                                  "goal_revision": waiting["revision"], "research_runs": 3, "practice_runs": 2,
                                  "policy_version": "pursuit-rules:v1", "user_instruction": False})
        self.assertNotIn(MINDUSTRY, event["summary"], "不保存用户原话")
        [interest] = self.interests()
        self.assertEqual(interest["status"], "Active")

        # 重启后如实说明已停止；不再研究、实践，也不重复写停止事件。
        self.run_eve([*self.send(self.message("ask-3", CHAT[2])),
                      *self.send(self.message("list-3", "/interests", contains="已停止：学习次数已用完"))])
        self.assertEqual(self.next(CHAT[2]), STOPPED)
        self.assertEqual(len(self.stop_events()), 1)
        self.assertEqual((len(self.research_runs()), len(self.practice_runs())), (3, 2))
        self.assertEqual(self.goal(), stopped)


if __name__ == "__main__":
    unittest.main(verbosity=2)
