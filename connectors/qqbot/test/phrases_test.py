"""实际 QQ 宿主的说法学习：训练中的会话攒够一批原话后学习一次，核对摘录出自原话再保存。

模型与 QQ 都是本地替身。验证学到的词语进入下一轮对话上下文、可查看可删除、
不合格/已完成结果不重放、明确暂态失败跨重启有限补做，关闭学习或停止训练时不发学习请求。
"""
import json
import time
import unittest

import developer_tools_test as developer
import dialogue_tools_test as dialogue

LEARNER_MARK = "从一位用户自己发的聊天原话里学习说法"
WORDS = ["这游戏yyds", "确实确实，我也这么想", "今天又摸鱼了", "晚上开黑吗", "这个boss好难打",
         "确实确实，太对了", "周末去看电影", "那家店的奶茶yyds", "我先去吃饭", "回来再聊",
         "你觉得呢", "就这样吧"]


class PhraseAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.callback = lambda body: dialogue.text_response("好的")
        self.learned = {"phrases": [
            {"kind": "term", "text": "yyds", "meaning": "永远的神，夸东西特别好", "example": "这游戏yyds"},
            {"kind": "expression", "text": "表示赞同时", "meaning": "常连说两遍“确实”", "example": "确实确实"},
        ]}

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and set(decoded) == {"messages"}:
            return "phrase"
        return super().classify(decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "phrase":
            self.assertEqual(decoded["messages"], WORDS)
            return json.dumps(self.learned, ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def provider_response(self, body, kind, fallback):
        if body['messages'][0].get('content', '').startswith('你是对话历史压缩器'):
            data = json.loads(body['messages'][1]['content'])
            covered = data['previous_turns'] + len(data['turns'])
            self.assertFalse(body.get('tools'))
            return dialogue.text_response(json.dumps({'points': [{'text': '原话与学习结果保存在原始档案。', 'turns': [covered]}]}, ensure_ascii=False))
        if kind == "phrase":
            self.assertEqual(body.get("response_format"), {"type":"json_object"})
            self.assertFalse(body.get("tools"))
            self.assertEqual(body["messages"][-2]["role"],"user")
            self.assertIn("待分析的用户原话",body["messages"][-2]["content"])
        return super().provider_response(body,kind,fallback)

    def phrase_state(self):
        return self.documents().get("eve.training", {}).get("phrases.v1")

    def due_retry(self):
        # 宿主已退出，仅推进测试状态的到期时间；不改变真实重试间隔与尝试计数。
        state = json.loads(self.state_path.read_text())
        value = json.loads(bytes(state["entries"]["eve.training"]["phrases.v1"]))
        self.assertEqual(len(value["inflight"]), 1)
        self.assertEqual(value["inflight"][0]["phase"]["state"], "retry_wait")
        value["inflight"][0]["phase"]["ready_at_ms"] = 0
        state["entries"]["eve.training"]["phrases.v1"] = list(json.dumps(value).encode())
        self.state_path.write_text(json.dumps(state))

    def wait_for(self, predicate, message, timeout=15):
        deadline = time.monotonic() + timeout
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            time.sleep(0.05)

    def chat(self, script, env=None, **options):
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     more=["--training"], env_extra=env or {}, timeout=60, **options)

    def words(self, prefix="w"):
        return [step for index, text in enumerate(WORDS)
                for step in self.send(self.message(f"{prefix}{index}", text, expected="好的"))]

    def test_batch_learns_terms_that_reach_the_next_turn_and_can_be_listed_and_forgotten(self):
        def learned():
            self.wait_for(lambda: len((self.phrase_state() or {}).get("learned", [])) == 2, "说法没有保存")
        seen = []
        styles = []
        def respond(body):
            styles.append(body["messages"][-3]["content"])
            seen.append("\n".join(str(m.get("content", "")) for m in body["messages"] if m["role"] == "system"))
            return dialogue.text_response("好的")
        self.callback = respond
        self.chat([*self.words(), *self.checkpoint("learned"),
                   *self.send(self.message("list", "/train words", contains=["yyds", "永远的神", "表示赞同时", "p"])),
                   *self.send(self.message("use", "这首歌yyds", expected="好的"))],
                  checkpoints={"learned": learned})
        self.assertEqual(len(self.kind("phrase")), 1)
        state = self.phrase_state()
        self.assertEqual([p["text"] for p in state["pending"]], ["这首歌yyds"], "取出成批后原话移出缓冲，之后的新原话重新攒批")
        self.assertIn("词语「yyds」：永远的神", seen[-1])
        self.assertIn("表示赞同时", seen[-1])
        self.assertIn("eve.current-expression-style-v1",styles[-1])
        self.assertIn("yyds",styles[-1])
        self.assertIn("表示赞同时",styles[-1])
        term = next(s["phrase"] for s in state["learned"] if s["phrase"]["kind"] == "term")
        self.chat(self.send(self.message("forget", f"/train forget {term['id']}", contains="已删除")))
        self.assertEqual([s["phrase"]["text"] for s in self.phrase_state()["learned"]], ["表示赞同时"])
        self.assertEqual(len(self.kind("phrase")), 1, "重启不重新学习已完成的批次")

    def test_unverifiable_output_saves_nothing_and_is_not_retried_after_restart(self):
        self.learned = {"phrases": [{"kind": "term", "text": "绝绝子", "meaning": "非常好", "example": "绝绝子"}]}
        self.chat([*self.words(), *self.checkpoint("asked")],
                  checkpoints={"asked": lambda: self.wait_for(lambda: self.kind("phrase"), "没有学习请求")})
        self.wait_for(lambda: self.phrase_state() is not None, "状态未写入")
        self.chat(self.send(self.message("list", "/train words", contains="还没有学到说法")))
        self.assertEqual(self.phrase_state()["learned"], [])
        self.assertEqual(self.phrase_state()["pending"], [])
        self.assertEqual(len(self.kind("phrase")), 1)

    def test_transient_failure_preserves_batch_and_retries_after_restart(self):
        self.failing.add("phrase")
        def retry_saved():
            self.wait_for(lambda: (self.phrase_state() or {}).get("inflight", [])
                          and self.phrase_state()["inflight"][0]["phase"]["state"] == "retry_wait",
                          "HTTP 503 后没有保存待补做批次")
        self.chat([*self.words(), *self.checkpoint("failed")], checkpoints={"failed": retry_saved})
        self.assertEqual(len(self.kind("phrase")), 1)
        saved = self.phrase_state()["inflight"][0]["batch"]
        self.assertEqual(saved["messages"], WORDS)
        self.assertEqual(saved["attempt"], 1)
        self.assertEqual(self.phrase_state()["pending"], [])
        self.due_retry()
        self.failing.clear()
        # 到期但开关关闭，原批次和尝试计数必须保留。
        self.chat(self.checkpoint("paused"), env={"EVE_PHRASE_LEARNING": "false"},
                  checkpoints={"paused": lambda: time.sleep(1)})
        self.assertEqual(self.phrase_state()["inflight"][0]["batch"], saved)
        self.assertEqual(len(self.kind("phrase")), 1)
        self.chat(self.checkpoint("learned"), checkpoints={"learned": lambda: self.wait_for(
            lambda: len((self.phrase_state() or {}).get("learned", [])) == 2, "原失败批次没有完成")})
        self.assertEqual(len(self.kind("phrase")), 2)
        request = self.kind("phrase")[-1]
        at_request = request["state"]["eve.training"]["phrases.v1"]["inflight"][0]["batch"]
        self.assertEqual(at_request["id"], saved["id"])
        self.assertEqual(at_request["scope"], saved["scope"])
        self.assertEqual(at_request["messages"], WORDS)
        self.assertEqual(at_request["attempt"], 2)
        self.assertEqual(self.phrase_state()["inflight"], [])
        self.chat(self.send(self.message("done", "/train words", contains="yyds")))
        self.assertEqual(len(self.kind("phrase")), 2)
        self.assertTrue(all(row["phrase"]["uses"] == 1 for row in self.phrase_state()["learned"]))

    def test_http_400_is_terminal_and_not_retried_after_restart(self):
        self.http_status = lambda kind, number: 400 if kind == "phrase" else None
        self.chat([*self.words(), *self.checkpoint("rejected")], checkpoints={"rejected": lambda: self.wait_for(
            lambda: self.kind("phrase") and not (self.phrase_state() or {}).get("inflight", []),
            "HTTP 400 批次未终结")})
        self.assertEqual(self.phrase_state()["learned"], [])
        self.assertEqual(self.phrase_state()["pending"], [])
        self.chat(self.send(self.message("after-rejected", "/train words", contains="还没有学到说法")))
        self.assertEqual(len(self.kind("phrase")), 1)

    def test_reset_rejects_a_late_success_and_restart_never_restores_it(self):
        self.response_gates[("phrase", 1)] = "late-phrase"
        claimed = {}
        def arrived():
            self.wait_for(lambda: self.arrived("phrase", 1).exists(), "未收到学习请求")
            claimed["batch"] = self.phrase_state()["inflight"][0]["batch"]["id"]
        def stale_finished():
            for line in self.panel_stderr.read_text().splitlines():
                if line.startswith("EVE_PHRASES "):
                    event = json.loads(line[len("EVE_PHRASES "):])
                    if event.get("batch") == claimed["batch"] and event.get("outcome") == "stale":
                        return True
            return False
        def release():
            self.assertEqual(self.phrase_state()["inflight"], [])
            self.gate("late-phrase").touch()
            self.wait_for(stale_finished, "迟到结果没有结束")
        self.chat([*self.words(), *self.checkpoint("arrived"),
                   *self.send(self.message("reset-late", "/train reset", contains="已重置")),
                   *self.checkpoint("release")], checkpoints={"arrived": arrived, "release": release}, panel=True)
        self.assertEqual(self.phrase_state()["learned"], [])
        self.chat(self.send(self.message("after-reset", "/train words", contains="还没有学到说法")))
        self.assertEqual(len(self.kind("phrase")), 1)

    def test_exhausted_global_budget_preserves_pending_phrases_across_restart(self):
        options = dict(interest=True, memory=True, research=False, practice=False,
                       more=["--training", "--autonomy-budget", "1", "--autonomy-budget-window-hours", "720"])
        self.run_eve(self.send(self.message("budget-init", "/budget", contains="剩余 1 次")), **options)
        state = json.loads(self.state_path.read_text())
        window = 720 * 3_600_000
        now = int(time.time() * 1000)
        ledger = {"format_version": 1, "policy": {"window_ms": window, "limit": 1},
                  "current": {"start_ms": now - now % window, "used": 1, "by_category": {"interest": 1}},
                  "history": []}
        state["entries"].setdefault("eve.budget", {})["usage.v1"] = list(json.dumps(ledger).encode())
        self.state_path.write_text(json.dumps(state))
        self.run_eve([*self.words(), *self.checkpoint("quiet")],
                     checkpoints={"quiet": lambda: time.sleep(1.5)}, **options)
        self.assertEqual(self.kind("phrase"), [], "耗尽总预算不能发出说法学习请求")
        self.assertEqual(len(self.phrase_state()["pending"]), len(WORDS), "余额不足不能提前取出并丢弃待学原话")
        self.run_eve([*self.send(self.message("budget-restored", "/budget", contains="剩余 0 次")),
                      *self.checkpoint("quiet")], checkpoints={"quiet": lambda: time.sleep(1.5)}, **options)
        self.assertEqual(self.kind("phrase"), [])
        self.assertEqual(len(self.phrase_state()["pending"]), len(WORDS))

    def test_disabled_learning_or_stopped_training_sends_no_learning_request(self):
        self.chat([*self.words("off")], env={"EVE_PHRASE_LEARNING": "false"})
        self.assertEqual(self.kind("phrase"), [])
        self.assertEqual(len(self.phrase_state()["pending"]), len(WORDS), "关闭学习时原话留在有界缓冲")
        self.chat([*self.send(self.message("reset", "/train reset", contains="已重置")),
                   *self.send(self.message("stop", "/train stop", contains="已结束")),
                   *self.words("stopped")], env={"EVE_PHRASE_LEARNING": "false"})
        self.assertEqual(self.phrase_state()["pending"], [], "重置删除待学原话，停止训练后不再缓冲")
        # 重新开启学习也没有可取的批次。
        self.chat(self.send(self.message("words", "/train words", contains="还没有学到说法")))
        self.assertEqual(self.kind("phrase"), [])


if __name__ == '__main__':
    unittest.main()
