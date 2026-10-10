"""实际 eve-qqbot 的有来源记忆召回验收；仅使用本地模型和 Node 桥接替身。

运行过程只操作临时目录与本测试持有的 Popen，不读取 PID 文件或停止其他服务。
桥接握手等待已送达交互提交，避免用固定延时推测记忆是否可读。
"""
import json
import os
import subprocess
import threading
import unittest

import memory_test as memory_harness

NO_MATCH = "未找到匹配记忆。"
RECALL_KIND = "eve-memory-recall-v1"
QUERY = "LOCAL_RECALL_EVIDENCE_MARKER"
USER_TEXT = QUERY + "：这是一条已经真实发送的用户历史消息。"
ASSISTANT_TEXT = QUERY + "：这是实际完成并送达的助手回复，不是用户偏好。"


class RecallAcceptance(unittest.TestCase):
    # 只复用进程握手和持久快照工具；旧记忆场景仍由 memory_test.py 独立运行。
    tearDown = memory_harness.MemoryAcceptance.tearDown
    documents = memory_harness.MemoryAcceptance.documents
    memory = memory_harness.MemoryAcceptance.memory
    snapshots = memory_harness.MemoryAcceptance.snapshots
    only_snapshot = memory_harness.MemoryAcceptance.only_snapshot
    evidence = memory_harness.MemoryAcceptance.evidence
    interactions = memory_harness.MemoryAcceptance.interactions
    receipt = memory_harness.MemoryAcceptance.receipt
    turn = memory_harness.MemoryAcceptance.turn
    gate = memory_harness.MemoryAcceptance.gate
    arrived = memory_harness.MemoryAcceptance.arrived
    checkpoint = memory_harness.MemoryAcceptance.checkpoint
    wait_until = memory_harness.MemoryAcceptance.wait_until
    wait_reply = memory_harness.MemoryAcceptance.wait_reply
    wait_receipt = memory_harness.MemoryAcceptance.wait_receipt
    send = memory_harness.MemoryAcceptance.send
    duplicate = memory_harness.MemoryAcceptance.duplicate
    remember = memory_harness.MemoryAcceptance.remember

    def setUp(self):
        memory_harness.MemoryAcceptance.setUp(self)
        outer = self

        class DistinctReplyHandler(self.server.RequestHandlerClass):
            def respond(self):
                assert self.path == "/v1/chat/completions"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                latest = body["messages"][-1]["content"]
                kind = "reflection" if "unverified_waiting_input" in latest else "chat"
                with outer.lock:
                    outer.requests.append({"kind": kind, "body": body, "run": outer.runs,
                                           "state": outer.documents()})
                    number = sum(request["kind"] == kind for request in outer.requests)
                outer.arrived(kind, number).touch()
                if kind == "reflection":
                    reply = json.dumps(memory_harness.ARTIFACT, ensure_ascii=False)
                else:
                    reply = ASSISTANT_TEXT if latest == USER_TEXT else latest
                encoded = json.dumps({"choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": reply}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server.RequestHandlerClass = DistinctReplyHandler

    def message(self, id, text, expected=None, contains=None, **route):
        if expected == NO_MATCH:
            expected, contains = None, [NO_MATCH, "memory revision="]
        if text == USER_TEXT and expected is None and contains is None:
            expected = ASSISTANT_TEXT
        return memory_harness.MemoryAcceptance.message(
            self, id, text, expected=expected, contains=contains, **route)

    def run_eve(self, script, memory=True, cognition=False, recall=False,
                app="1904159860", checkpoints=None, send_fail=False, success=True, extra=(), env_extra=None):
        """使用既有替身协议，并为独立召回 opt-in 添加真实命令行参数。"""
        self.runs += 1
        self.run_release = threading.Event()
        events_path = self.work / f"events-{self.runs}.jsonl"
        error_path = self.work / f"error-{self.runs}.txt"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events_path),
            "error_file": str(error_path), "send_fail": send_fail}), encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID=app,
            EVE_OPENAI_API_KEY="test-model-secret",
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
            EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        env.update(env_extra or {})
        command = [str(memory_harness.BINARY), "--state-dir", str(self.work / "state"),
            "--agent", str(memory_harness.ROOT / "AGENT.md"), "--bridge-script",
            str(memory_harness.FAKE), "--bridge-arg", str(scenario)]
        if memory:
            command.append("--memory")
        if cognition:
            command.extend(["--cognition", "--cognition-max-executions", "1"])
        if recall:
            command.append("--memory-recall")
        command.extend(extra)

        def inspect():
            for name, callback in (checkpoints or {}).items():
                try:
                    self.wait_until(lambda: self.gate(name + "-inspect").exists(), name + " not reached")
                    callback()
                except Exception:
                    self.errors.append(memory_harness.traceback.format_exc())
                finally:
                    self.gate(name + "-continue").touch()

        inspector = threading.Thread(target=inspect, daemon=True)
        child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        inspector.start()
        try:
            stdout, stderr = child.communicate(timeout=30)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
            self.run_release.set()
            inspector.join(timeout=2)
        self.events = [json.loads(line) for line in events_path.read_text().splitlines()] if events_path.exists() else []
        self.assertFalse(inspector.is_alive(), "snapshot inspector did not stop")
        self.assertFalse(error_path.exists(), error_path.read_text() if error_path.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ["test-model-secret", "test-app-secret"]:
            self.assertNotIn(secret, stdout + stderr)
        if not success:
            self.assertNotEqual(child.returncode, 0)
            self.assertIn("QQBot 通道异常结束", stderr)
            return None
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])
        return summary

    def reply(self, message_id):
        replies = [event["text"] for event in self.events
                   if event.get("direction") == "out" and event.get("type") == "reply"
                   and event["id"] == message_id]
        self.assertEqual(len(replies), 1)
        return replies[0]

    def wait_import(self, count):
        self.wait_until(lambda: len(self.interactions()) == count, "delivered interaction was not imported")

    def seed_interaction(self, message_id="source", text=USER_TEXT, **route):
        count = len(self.interactions()) + 1
        gate = f"import-{self.runs + 1}"
        self.run_eve([*self.send(self.message(message_id, text, **route)), *self.checkpoint(gate)],
                     checkpoints={gate: lambda: self.wait_import(count)})
        return next(item for item in self.interactions() if item["source"]["message_id"] == message_id)

    def recall_data(self, request):
        payloads = []
        for message in request["body"]["messages"]:
            content = message.get("content", "") or ""
            if RECALL_KIND in content:
                self.assertEqual(message["role"], "system")
                payloads.append(json.loads(content[content.index("{"):]))
        return payloads

    def test_disabled_empty_oversized_and_control_queries_never_reach_model(self):
        self.run_eve(self.send(self.message("off", "/recall " + QUERY, memory_harness.DISABLED)), memory=False)
        self.assertNotIn("eve.memory", self.documents())
        commands = ["/recall", "/recall   ", "/recall " + "x" * 1025,
                    "/recall " + "忆" * 342, "/recall bad\x01query", "/recall\tquery",
                    "/recall\x00query", "/recall query\n"]
        script = []
        for index, command in enumerate(commands):
            script.extend(self.send(self.message(f"invalid-{index}", command, contains="/recall 关键词")))
        script.extend(self.send(self.message("empty", "/recall " + QUERY, NO_MATCH)))
        self.run_eve(script, recall=True)
        self.assertFalse(self.requests)
        self.assertFalse(self.snapshots())

    def test_context_opt_in_without_memory_fails_before_starting_bridge(self):
        child = subprocess.Popen([str(memory_harness.BINARY), "--memory-recall",
                                  "--state-dir", str(self.work / "state")],
                                 env={"PATH": os.environ.get("PATH", "")},
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            stdout, stderr = child.communicate(timeout=10)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
        self.assertNotEqual(child.returncode, 0)
        self.assertIn("--memory-recall 需要同时开启 --memory", stderr)
        self.assertEqual(stdout, "")
        self.assertFalse(self.state_path.exists())
        self.assertFalse(self.requests)

    def test_delivered_roles_have_verifiable_sources_read_only_and_stable_restart(self):
        evidence = self.seed_interaction()
        before = self.memory()
        source = evidence["source"]
        expected = ["[用户原话]", "[历史助手回复]", USER_TEXT, ASSISTANT_TEXT,
                    f"evidence={evidence['id']}", f"evidence_revision={evidence['revision']}",
                    "message=source", f"session_revision={source['session_revision']}",
                    f"turn={source['turn_id']}"]
        self.run_eve(self.send(self.message("recall-first", "/recall " + QUERY, contains=expected)))
        first = self.reply("recall-first")
        self.assertNotIn("[已确认偏好]", first)
        entries = first.split("\n片段：")
        self.assertEqual(len(entries), 3)
        for index, label in enumerate(entries[:-1]):
            expected_text = USER_TEXT if "[用户原话]" in label.split("\n")[-1] else ASSISTANT_TEXT
            self.assertTrue(entries[index + 1].startswith(expected_text))
        self.assertEqual(self.memory(), before)
        self.run_eve(self.send(self.message("recall-restored", "/recall " + QUERY, contains=expected)))
        self.assertEqual(self.reply("recall-restored"), first)
        self.assertEqual(self.memory(), before)
        self.assertEqual(len(self.requests), 1)

    def test_trusted_user_group_private_and_application_scopes_do_not_leak(self):
        self.seed_interaction(scope="group")
        before = self.memory()
        routes = [{"scope": "group", "user": "user-2"},
                  {"scope": "group", "target": "group-2"}, {"scope": "c2c"}]
        script = []
        for index, route in enumerate(routes):
            script.extend(self.send(self.message(f"foreign-{index}", "/recall " + QUERY,
                                                 NO_MATCH, **route)))
        self.run_eve(script)
        self.assertEqual(self.memory(), before)
        self.run_eve(self.send(self.message("other-app", "/recall " + QUERY,
                                            NO_MATCH, scope="group")), app="other-app")
        self.assertEqual(self.memory(), before)
        self.run_eve(self.send(self.message("owner", "/recall " + QUERY,
                                            contains=["[用户原话]", USER_TEXT], scope="group")))
        self.assertEqual(self.memory(), before)
        self.assertEqual(len(self.requests), 1)

    def test_corrected_and_revoked_preferences_never_reappear_from_raw_history(self):
        preference_id = self.remember()
        before = self.memory()
        self.run_eve(self.send(self.message("confirmed", "/recall PRIVATE",
            contains=["[已确认偏好]", "preference=" + preference_id, "version=1", memory_harness.PREFERENCE])))
        self.assertEqual(self.memory(), before)
        self.run_eve(self.send(self.message("correction", f"/correct-memory {preference_id} {memory_harness.CORRECTION}",
                                            contains="偏好已修正：")))
        corrected = self.memory()
        self.run_eve([*self.send(self.message("old", "/recall PRIVATE", NO_MATCH)),
                      *self.send(self.message("new", "/recall CORRECTED",
                          contains=["[已确认偏好]", "preference=" + preference_id,
                                    "version=2", memory_harness.CORRECTION]))])
        self.assertNotIn(memory_harness.PREFERENCE, self.reply("new"))
        self.assertEqual(self.memory(), corrected)
        self.run_eve(self.send(self.message("revoke", "/forget " + preference_id,
                                            contains="偏好已撤销：")))
        revoked = self.memory()
        self.run_eve([*self.duplicate("remember", "/remember " + memory_harness.PREFERENCE),
                      *self.duplicate("correction", f"/correct-memory {preference_id} {memory_harness.CORRECTION}"),
                      *self.send(self.message("revoked-old", "/recall PRIVATE", NO_MATCH)),
                      *self.send(self.message("revoked-new", "/recall CORRECTED", NO_MATCH))])
        self.assertEqual(self.memory(), revoked)
        self.assertEqual(self.only_snapshot()["preferences"][0]["status"], "Revoked")
        self.assertEqual(len(self.evidence()), 3)
        self.assertFalse(self.requests)

    def test_reply_pending_is_not_recalled_until_delivery_observer_commits(self):
        def assert_pending():
            self.assertEqual(self.receipt("held")["state"], "ReplyPending")
            self.assertEqual(self.interactions(), [])

        held = {**self.message("held", USER_TEXT), "hold_delivery": True}
        self.run_eve([{"send": held}, self.wait_reply("held"), *self.checkpoint("pending"),
                      {"send": self.message("not-delivered", "/recall " + QUERY, NO_MATCH)},
                      self.wait_receipt("not-delivered", "Processing"),
                      {"delivery": "held"}, self.wait_receipt("held"),
                      self.wait_reply("not-delivered"), self.wait_receipt("not-delivered"),
                      *self.checkpoint("delivered"),
                      *self.send(self.message("delivered-recall", "/recall " + QUERY,
                                              contains=["[用户原话]", "[历史助手回复]", USER_TEXT]))],
                     checkpoints={"pending": assert_pending, "delivered": lambda: self.wait_import(1)})
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(len(self.evidence()), 1)
        self.assertEqual(self.interactions()[0]["source"]["message_id"], "held")

    def test_failed_or_unacknowledged_delivery_stays_absent_after_restart(self):
        self.run_eve([{"send": self.message("failed", QUERY + " failed")}, self.wait_reply("failed"),
                      self.wait_receipt("failed", "Failed")], send_fail=True)
        pending = {**self.message("unacknowledged", QUERY + " pending"), "hold_delivery": True}
        self.run_eve([{"send": pending}, self.wait_reply("unacknowledged"),
                      self.wait_receipt("unacknowledged", "ReplyPending")], success=False)
        self.run_eve(self.send(self.message("after-recovery", "/recall " + QUERY, NO_MATCH)))
        self.assertEqual(self.interactions(), [])
        self.assertEqual(len(self.requests), 2)

    def test_context_requires_opt_in_and_preserves_scope_and_provenance(self):
        self.seed_interaction()
        self.run_eve(self.send(self.message("default-context", QUERY)))
        self.assertEqual(self.recall_data(self.requests[-1]), [])
        self.run_eve(self.send(self.message("enabled-context", QUERY)), recall=True)
        request = self.requests[-1]
        payloads = self.recall_data(request)
        self.assertEqual(len(payloads), 1)
        data = payloads[0]
        self.assertEqual(data["kind"], RECALL_KIND)
        self.assertGreater(len(data["hits"]), 0)
        self.assertLessEqual(len(data["hits"]), 3)
        evidence = {item["id"]: item for scope in request["state"]["eve.memory"]["memory.v1"]["scopes"]
                    for item in scope["snapshot"]["evidence"]}
        fields = set()
        for hit in data["hits"]:
            self.assertFalse(hit["independently_verified"])
            source = hit["source"]
            self.assertEqual(source["kind"], "CompletedInteraction")
            saved = evidence[source["evidence_id"]]
            original = saved["source"]
            self.assertEqual(source["evidence_revision"], saved["revision"])
            for field in ("message_id", "session_revision", "turn_id"):
                self.assertEqual(source[field], original[field])
            self.assertNotEqual(source["message_id"], "enabled-context")
            fields.add(source["field"])
            if source["field"] == "User":
                self.assertEqual(hit["use_as"], "historical_user_message")
                self.assertIn(hit["excerpt"], original["user_text"])
            else:
                self.assertEqual(source["field"], "Assistant")
                self.assertEqual(hit["use_as"], "historical_assistant_response")
                self.assertIn(hit["excerpt"], original["assistant_text"])
        self.assertEqual(fields, {"User", "Assistant"})
        self.run_eve(self.send(self.message("foreign-context", QUERY, user="other-user")), recall=True)
        self.assertEqual(self.recall_data(self.requests[-1]), [])

    def test_opt_in_does_not_expose_private_recall_to_internal_reflection(self):
        self.seed_interaction()
        before = self.memory()
        description = QUERY + "：请只形成仍需用户确认的草稿。"
        self.run_eve([*self.send(self.message("goal", "/goal " + description, contains="待办已保存：")),
                      {"wait_cognition": {"path": str(self.state_path), "parent_description": description,
                                          "child_state": "Completed"}}], cognition=True, recall=True)
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.requests[-1]["kind"], "reflection")
        self.assertEqual(self.recall_data(self.requests[-1]), [])
        self.assertNotIn(USER_TEXT, json.dumps(self.requests[-1]["body"], ensure_ascii=False))
        self.assertEqual(self.memory(), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
