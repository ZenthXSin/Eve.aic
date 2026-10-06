"""真实 QQ 进程的学习决策与偏好更新验收，仅使用本地模型和桥接替身。

正向步骤等待实际持久状态，短暂观察窗口只用于断言没有后台重复写入。
所有进程管理复用 LearningAcceptance 持有的 Popen，不读取 PID 文件。
"""
import json
import unittest

import learning_test


class LearningDecisionAcceptance(unittest.TestCase):
    # 只复用助手，不继承原测试类，避免隐式重复运行整组历史用例。
    setUp = learning_test.LearningAcceptance.setUp
    tearDown = learning_test.LearningAcceptance.tearDown
    documents = learning_test.LearningAcceptance.documents
    memory = learning_test.LearningAcceptance.memory
    snapshots = learning_test.LearningAcceptance.snapshots
    only_snapshot = learning_test.LearningAcceptance.only_snapshot
    evidence = learning_test.LearningAcceptance.evidence
    interactions = learning_test.LearningAcceptance.interactions
    receipt = learning_test.LearningAcceptance.receipt
    gate = learning_test.LearningAcceptance.gate
    arrived = learning_test.LearningAcceptance.arrived
    checkpoint = learning_test.LearningAcceptance.checkpoint
    wait_until = learning_test.LearningAcceptance.wait_until
    message = learning_test.LearningAcceptance.message
    wait_reply = learning_test.LearningAcceptance.wait_reply
    wait_receipt = learning_test.LearningAcceptance.wait_receipt
    send = learning_test.LearningAcceptance.send
    preference_data = learning_test.LearningAcceptance.preference_data
    learning = learning_test.LearningAcceptance.learning
    learning_bytes = learning_test.LearningAcceptance.learning_bytes
    jobs = learning_test.LearningAcceptance.jobs
    preferences = learning_test.LearningAcceptance.preferences
    learning_requests = learning_test.LearningAcceptance.learning_requests
    chat_requests = learning_test.LearningAcceptance.chat_requests
    wait_jobs = learning_test.LearningAcceptance.wait_jobs
    quiet = learning_test.LearningAcceptance.quiet
    run_eve = learning_test.LearningAcceptance.run_eve
    inspect_run = learning_test.LearningAcceptance.inspect_run
    assert_reserved_before_request = learning_test.LearningAcceptance.assert_reserved_before_request

    def decision_document(self):
        return self.documents().get("eve.learning", {}).get("learning.decisions.v1")

    def decision_bytes(self):
        if not self.state_path.exists():
            return None
        return json.loads(self.state_path.read_text())["entries"].get("eve.learning", {}).get(
            "learning.decisions.v1")

    def records(self, candidate_id=None):
        records = [entry["record"] for entry in (self.decision_document() or {}).get("records", [])]
        if candidate_id is not None:
            records = [record for record in records if record["decision"]["candidate_id"] == candidate_id]
        return records

    def wait_decision(self, candidate_id, reason):
        self.wait_until(lambda: any(record["decision"]["reason"] == reason
                                    for record in self.records(candidate_id)),
                        f"decision {reason} was not saved for {candidate_id}")
        return next(record["decision"] for record in reversed(self.records(candidate_id))
                    if record["decision"]["reason"] == reason)

    def complete_batch(self, batch_count, prefix, candidate_text, reason, before=None):
        self.candidate_text = candidate_text
        previous_interactions = len(self.interactions())
        script = list(before or [])
        for index in range(3):
            script.extend(self.send(self.message(f"{prefix}-{index}", candidate_text.strip())))

        def completed():
            self.wait_until(lambda: len(self.interactions()) == previous_interactions + 3,
                            "three delivered interactions were not imported")
            self.wait_jobs(batch_count)
            candidate = self.jobs()[-1]["candidates"][0]
            self.wait_decision(candidate["id"], reason)
            # 决策在 Memory CAS 之前保存；成功判断必须继续等待实际记忆历史。
            if reason in ("Eligible", "ExplicitRevisionUpdate"):
                self.wait_until(lambda: any(
                    version["text"] == candidate_text
                    and version["evidence_id"] in candidate["draft"]["evidence_ids"]
                    for preference in self.preferences() for version in preference["history"]),
                    "decision was saved but the corresponding preference history was not committed")

        self.inspect_run(completed, prefix=script, self_learning=True, cooldown_ms=0)
        self.assertEqual(len(self.learning_requests()), batch_count)
        batch = self.assert_reserved_before_request(self.learning_requests()[-1])
        self.assertEqual(len(batch["evidence"]), 3)
        candidate = self.jobs()[-1]["candidates"][0]
        decision = self.wait_decision(candidate["id"], reason)
        self.assertEqual(decision["batch_id"], batch["id"])
        self.assertEqual(decision["evidence_ids"], candidate["draft"]["evidence_ids"])
        self.assertEqual(decision["policy_version"], "evidence-conflict-v2")
        return candidate, decision

    def test_confirm_duplicate_update_manual_barrier_and_restart_preserve_evidence(self):
        first, confirmation = self.complete_batch(1, "first", "回复最多2段", "Eligible")
        target_id = "learned-" + first["id"]
        self.assertEqual(confirmation["action"], "Confirm")
        self.assertEqual(len(self.preferences()), 1)
        original = self.preferences()[0]
        self.assertEqual((original["id"], original["revision"], original["text"]),
                         (target_id, 1, "回复最多2段"))

        duplicate, repeated = self.complete_batch(
            2, "duplicate", "  回复最多2段。  ", "Duplicate")
        self.assertNotEqual(duplicate["id"], first["id"])
        self.assertEqual(repeated["action"], "Reject")
        self.assertEqual(self.preferences(), [original], "duplicate candidate added a preference/history")

        changed, update = self.complete_batch(3, "changed", "回复最多3段", "ExplicitRevisionUpdate")
        self.assertEqual(update["action"], {"Update": {
            "preference_id": target_id, "expected_revision": 1}})
        self.assertEqual(len(self.preferences()), 1, "update must preserve the existing preference ID")
        updated = self.preferences()[0]
        self.assertEqual((updated["id"], updated["revision"], updated["text"]),
                         (target_id, 2, "回复最多3段"))
        self.assertEqual(updated["history"][0], original["history"][0])
        self.assertEqual([version["revision"] for version in updated["history"]], [1, 2])
        self.assertIn(updated["history"][1]["evidence_id"], changed["draft"]["evidence_ids"])
        self.assertTrue(set(first["draft"]["evidence_ids"]).isdisjoint(changed["draft"]["evidence_ids"]))

        decision_sources = changed["draft"]["evidence_ids"]
        self.run_eve([
            *self.send(self.message("decision-details", "/memory-decision " + changed["id"],
                                    contains=[changed["id"], target_id, "evidence-conflict-v2",
                                              *decision_sources])),
            *self.send(self.message("foreign-decision", "/memory-decision " + changed["id"],
                                    contains="当前会话没有这条候选", user="other-user")),
        ], self_learning=True, cooldown_ms=0)
        foreign = next(event["text"] for event in self.events
                       if event["direction"] == "out" and event.get("id") == "foreign-decision"
                       and event["type"] == "reply")
        for private in [changed["id"], target_id, *decision_sources]:
            self.assertNotIn(private, foreign)
        self.assertEqual(len(self.learning_requests()), 3, "decision lookup called the extractor")

        command = "/correct-memory " + target_id + " 回复最多4段"
        blocked, refusal = self.complete_batch(4, "manual-barrier", "回复最多5段", "ManualConflict",
            before=self.send(self.message("manual-correction", command, contains="偏好已修正")))
        self.assertEqual(refusal["action"], "Defer")
        self.assertEqual(len(self.preferences()), 1)
        corrected = self.preferences()[0]
        self.assertEqual((corrected["id"], corrected["revision"], corrected["text"]),
                         (target_id, 3, "回复最多4段"))
        self.assertEqual(corrected["history"][:2], updated["history"])
        manual_source = next(item for item in self.evidence()
                             if item["id"] == corrected["history"][-1]["evidence_id"])
        self.assertEqual(manual_source["source"], {
            "kind": "UserStatement", "message_id": "manual-correction", "text": command})
        self.assertNotIn(manual_source["id"], refusal["evidence_ids"])
        self.assertEqual(len(self.interactions()), 12)
        self.assertEqual(len(self.evidence()), 13, "automatic decisions must not forge user statements")

        # 旧未保存候选允许重新判断不同原因；先让当前状态稳定，再比较重启字节。
        self.inspect_run(self.quiet, self_learning=True, cooldown_ms=0)
        before_learning = self.learning_bytes()
        before_decisions = self.decision_bytes()
        before_memory = self.memory()
        before_records = self.records()
        self.assertTrue(before_records)
        self.assertEqual([record["sequence"] for record in before_records],
                         list(range(1, len(before_records) + 1)))
        scope = self.only_snapshot()["scope"]
        self.assertTrue(all(entry["scope"] == scope for entry in self.decision_document()["records"]))

        self.inspect_run(self.quiet, prefix=[
            *self.send(self.message("restored-decision", "/memory-decision " + changed["id"],
                                    contains=[target_id, "evidence-conflict-v2", *decision_sources])),
            *self.send(self.message("restored-blocked", "/memory-decision " + blocked["id"],
                                    contains=[blocked["id"], "保留手动选择", "defer"])),
        ], self_learning=True, cooldown_ms=0)
        self.assertEqual(self.learning_bytes(), before_learning, "restart rewrote consumed learning batches")
        self.assertEqual(self.decision_bytes(), before_decisions, "restart appended duplicate decisions")
        self.assertEqual(self.memory(), before_memory, "restart changed the manually corrected history")
        self.assertEqual(len(self.learning_requests()), 4, "restart replayed an extraction request")
        self.assertEqual(len(self.chat_requests()), 12)
        print(json.dumps({"scenario": "learning-decisions", "process_runs": self.runs,
                          "chat_requests": len(self.chat_requests()),
                          "learning_requests": len(self.learning_requests()),
                          "completed_interactions": len(self.interactions()),
                          "decision_records": len(before_records),
                          "preference_count": len(self.preferences()),
                          "preference_revision": self.preferences()[0]["revision"]}))

    def test_decision_commands_disabled_invalid_and_missing_are_model_free(self):
        self.run_eve(self.send(self.message("disabled-decision", "/memory-decision absent",
                                           expected="偏好提炼未启用。")), learning=False, memory=False)
        self.assertIsNone(self.decision_document())
        self.run_eve([
            *self.send(self.message("invalid-decision", "/memory-decision", contains="用法：")),
            *self.send(self.message("extra-decision", "/memory-decision a b", contains="用法：")),
            *self.send(self.message("missing-decision", "/memory-decision absent",
                                    contains="当前会话没有这条候选")),
        ], self_learning=True, cooldown_ms=0)
        self.assertEqual(self.requests, [])
        self.assertFalse(self.jobs())
        self.assertIsNone(self.decision_document())
        self.assertFalse(self.snapshots())


if __name__ == "__main__":
    unittest.main(verbosity=2)
