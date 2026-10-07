"""实际 QQ 进程的偏好提炼闭环；只连接本地模型替身和 fake bridge。

正向时序通过文件门控和真实持久状态确认；短暂观察窗口只验证没有后台重试。
停止仅操作本测试持有的 Popen，不读取 PID 文件，也不向任意 PID 发信号。
"""
import json
import os
import pathlib
import signal
import subprocess
import tempfile
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import memory_test
from memory_test import ARTIFACT, BINARY, FAKE, ROOT

DISABLED = "偏好提炼未启用。"
HELP = "用法：/memory-candidates [页码]、/memory-decision 候选ID、/accept-memory 候选ID。"
EMPTY = "当前会话没有偏好候选。"
NOT_OWNED = "当前会话没有这条候选。发送 /memory-candidates 查看候选 ID。"
CANDIDATE = "LEARNED_CANDIDATE_MARKER：回答先给简短结论，再给必要依据。"
CORRECTION = "CORRECTED_CANDIDATE_MARKER：先给依据，然后给结论。"


class LearningAcceptance(unittest.TestCase):
    # 复用已验收的只读状态和桥接脚本助手，不继承另一组测试。
    documents = memory_test.MemoryAcceptance.documents
    memory = memory_test.MemoryAcceptance.memory
    snapshots = memory_test.MemoryAcceptance.snapshots
    only_snapshot = memory_test.MemoryAcceptance.only_snapshot
    evidence = memory_test.MemoryAcceptance.evidence
    interactions = memory_test.MemoryAcceptance.interactions
    receipt = memory_test.MemoryAcceptance.receipt
    gate = memory_test.MemoryAcceptance.gate
    arrived = memory_test.MemoryAcceptance.arrived
    checkpoint = memory_test.MemoryAcceptance.checkpoint
    wait_until = memory_test.MemoryAcceptance.wait_until
    message = memory_test.MemoryAcceptance.message
    wait_reply = memory_test.MemoryAcceptance.wait_reply
    wait_receipt = memory_test.MemoryAcceptance.wait_receipt
    send = memory_test.MemoryAcceptance.send
    duplicate = memory_test.MemoryAcceptance.duplicate
    preference_data = memory_test.MemoryAcceptance.preference_data

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_path = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.runs = 0
        self.response_gates = {}
        self.learning_response = "valid"
        self.candidate_text = CANDIDATE
        self.candidate_confidence = 83
        self.candidate_sources = None
        self.chat_response = None
        self.run_release = threading.Event()
        self.events = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    self.respond()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

            def respond(self):
                assert self.path == "/v1/chat/completions"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                latest = body["messages"][-1]["content"]
                try:
                    decoded = json.loads(latest)
                except (ValueError, TypeError):
                    decoded = None
                if isinstance(decoded, dict) and "extractor_version" in decoded and "evidence" in decoded:
                    kind = "learning"
                elif "unverified_waiting_input" in latest:
                    kind = "reflection"
                else:
                    kind = "chat"
                release = outer.run_release
                with outer.lock:
                    outer.requests.append({"kind": kind, "body": body, "run": outer.runs,
                                           "state": outer.documents()})
                    number = sum(request["kind"] == kind for request in outer.requests)
                outer.arrived(kind, number).touch()
                gate = outer.response_gates.get((kind, number))
                if gate is not None:
                    deadline = time.monotonic() + 15
                    while not outer.gate(gate).exists():
                        if release.wait(0.01):
                            return
                        assert time.monotonic() < deadline, "response gate timed out"
                status = 200
                finish = "stop"
                text = json.dumps(ARTIFACT, ensure_ascii=False) if kind == "reflection" else latest
                if kind == "chat" and outer.chat_response is not None:
                    text = outer.chat_response(body, latest)
                message = {"role": "assistant", "content": text}
                if kind == "learning":
                    mode = outer.learning_response
                    draft = {"text": outer.candidate_text, "confidence": outer.candidate_confidence,
                             "evidence_ids": [item["id"] for item in decoded["evidence"]][:outer.candidate_sources]}
                    result = {"candidates": [] if mode == "empty" else [draft]}
                    if mode == "invalid":
                        draft["evidence_ids"] = ["foreign-evidence-must-be-rejected"]
                    if mode == "provider":
                        status = 503
                    message["content"] = json.dumps(result, ensure_ascii=False)
                    if mode == "tool":
                        finish = "tool_calls"
                        message = {"role": "assistant", "content": None, "tool_calls": [{
                            "id": "forbidden-call", "type": "function", "function": {
                                "name": "must_not_execute", "arguments": "{}"}}]}
                response = ({"error": {"message": "local provider unavailable", "type": "test_error"}}
                            if status != 200 else {"choices": [{"index": 0, "finish_reason": finish,
                                                                 "message": message}]})
                encoded = json.dumps(response).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.run_release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.directory.cleanup()

    def learning(self):
        return self.documents().get("eve.learning", {}).get("learning.v1")

    def learning_bytes(self):
        if not self.state_path.exists():
            return None
        return json.loads(self.state_path.read_text())["entries"].get("eve.learning", {}).get("learning.v1")

    def jobs(self):
        return [record["job"] for record in (self.learning() or {}).get("jobs", [])]

    def learning_requests(self):
        return [request for request in self.requests if request["kind"] == "learning"]

    def chat_requests(self):
        return [request for request in self.requests if request["kind"] == "chat"]

    def wait_jobs(self, count, status="Completed"):
        self.wait_until(lambda: len(self.jobs()) == count and all(job["status"] == status for job in self.jobs()),
                        f"expected {count} jobs with status {status}; got {self.jobs()}")

    def quiet(self):
        # 只作负向断言：覆盖至少两轮 250ms 扫描，不能替代正向状态门控。
        self.assertFalse(self.run_release.wait(0.65), "process stopped during observation")

    def run_eve(self, script, learning=True, memory=True, cognition=False, training=False,
                app="1904159860", checkpoints=None, stop_at=None, abrupt=False, segmented=False, self_learning=False, cooldown_ms=None):
        self.runs += 1
        self.run_release = threading.Event()
        events_path = self.work / f"events-{self.runs}.jsonl"
        error_path = self.work / f"error-{self.runs}.txt"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events_path),
                                       "error_file": str(error_path)}), encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID=app,
                   EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        command = [str(BINARY), "--state-dir", str(self.work / "state"),
                   "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE),
                   "--bridge-arg", str(scenario)]
        if learning:
            command.append("--memory-learning")
        if memory:
            command.append("--memory")
        if cooldown_ms is not None:
            command.extend(["--learning-cooldown-ms", str(cooldown_ms)])
        if self_learning:
            command.append("--self-learning")
        if segmented:
            command.append("--segmented")
        if cognition:
            command.extend(["--cognition", "--cognition-max-executions", "1"])
        if training:
            command.append("--training")

        def inspect():
            for name, callback in (checkpoints or {}).items():
                try:
                    self.wait_until(lambda: self.gate(name + "-inspect").exists(), name + " not reached")
                    callback()
                except Exception:
                    self.errors.append(traceback.format_exc())
                finally:
                    self.gate(name + "-continue").touch()

        inspector = threading.Thread(target=inspect, daemon=True)
        child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        inspector.start()
        try:
            if stop_at is not None:
                self.wait_until(lambda: stop_at.exists() or child.poll() is not None, "stop gate was not reached")
                self.assertTrue(stop_at.exists(), "process exited before stop synchronization")
                if abrupt:
                    child.kill()
                else:
                    child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=30)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
            self.run_release.set()
            inspector.join(timeout=2)
        self.events = ([json.loads(line) for line in events_path.read_text().splitlines()]
                       if events_path.exists() else [])
        self.assertFalse(inspector.is_alive(), "state inspector did not stop")
        self.assertFalse(error_path.exists(), error_path.read_text() if error_path.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ("test-model-secret", "test-app-secret"):
            self.assertNotIn(secret, stdout + stderr)
        if abrupt:
            self.assertNotEqual(child.returncode, 0)
            return
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])

    def inspect_run(self, callback, prefix=None, **options):
        name = f"inspect-{self.runs + 1}"
        self.run_eve([*(prefix or []), *self.checkpoint(name)], checkpoints={name: callback}, **options)

    def seed(self, count=3, prefix="seed", feedback=None, **route):
        previous = len(self.interactions())
        script = []
        for index in range(count):
            text = feedback or f"{prefix}-USER_EVIDENCE-{index}：每次回答请先给简短结论，再给必要依据。"
            script.extend(self.send(self.message(f"{prefix}-{index}", text, **route)))
        self.inspect_run(lambda: self.wait_until(lambda: len(self.interactions()) == previous + count,
                                               "completed delivery was not imported"),
                         prefix=script, learning=False)

    def generate(self):
        self.inspect_run(lambda: self.wait_jobs(1))
        self.assertEqual(len(self.learning_requests()), 1)
        return self.jobs()[0]["candidates"][0]["id"]

    def preferences(self):
        return [p for snapshot in self.snapshots() for p in snapshot["preferences"]]

    def segment_send(self, message, parts):
        return [{"send": {**message, "expected_segments": parts}}, self.wait_receipt(message["id"])]

    def test_content_rules_generate_new_topic_reply_and_follow_correction_scope_and_revocation(self):
        self.candidate_text = "解释问题时先给结论，再给依据；信息足够时不追加追问。"
        self.seed(feedback="以后解释问题先给结论再给依据，信息够了不要追问；刚才的天气例子只是改写示范。")
        self.inspect_run(lambda: self.wait_until(lambda: len(self.preferences()) == 1,
                                               "content rule was not automatically confirmed"), self_learning=True)
        candidate = self.jobs()[0]["candidates"][0]
        source = "learned-" + candidate["id"]
        self.assertEqual(self.jobs()[0]["batch"]["extractor_version"], "preference-extractor:v3")
        self.assertEqual(self.preferences()[0]["text"], self.candidate_text)
        corrected = "解释问题时先给依据，再给结论；信息足够时不追加追问。"

        # 确定性模型替身验证真实进程把规则交给新主题的生成请求；不声称真实模型质量。
        def generate(body, latest):
            preferences = self.preference_data({"body": body})
            texts = [item["text"] for data in preferences for item in data["preferences"]]
            if latest == "解释两数相加":
                if self.candidate_text in texts:
                    return "结果是四。因为二加二等于四。"
                if corrected in texts:
                    return "二加二等于四，所以结果是四。"
                return "你希望怎样解释两数相加？"
            return latest

        self.chat_response = generate
        self.run_eve([
            *self.send(self.message("content-new-topic", "解释两数相加", expected="结果是四。因为二加二等于四。")),
            *self.send(self.message("content-other-user", "解释两数相加", user="another-user",
                                    expected="你希望怎样解释两数相加？")),
            *self.send(self.message("content-correct", "/correct-memory " + source + " " + corrected, contains="偏好已修正")),
            *self.send(self.message("content-corrected-topic", "解释两数相加", expected="二加二等于四，所以结果是四。")),
        ], self_learning=True, training=True)
        self.run_eve([
            *self.send(self.message("content-restored-topic", "解释两数相加", expected="二加二等于四，所以结果是四。")),
            *self.send(self.message("content-forget", "/forget " + source, contains="偏好已撤销")),
            *self.send(self.message("content-revoked-topic", "解释两数相加", expected="你希望怎样解释两数相加？")),
        ], self_learning=True, training=True)
        self.assertEqual(len(self.learning_requests()), 1, "restart must not replay consumed evidence")
        self.assertEqual(self.preferences()[0]["status"], "Revoked")
        for request in self.chat_requests()[3:]:
            self.assertEqual(request["body"]["messages"][-1]["content"], "解释两数相加")
            # 新主题不会重新注入天气示范原文；保留的真实会话历史仍按原契约装配。
            for message in request["body"]["messages"]:
                if message["role"] == "system" and "eve-confirmed-preferences-v1" in message.get("content", ""):
                    self.assertNotIn("天气例子", message["content"])

    def test_autonomous_confirmation_changes_context_and_delivery_recovers_and_respects_manual_settings(self):
        self.candidate_text = "回复最多分成两段，段间不要停顿。"
        self.seed(feedback=self.candidate_text)
        self.inspect_run(lambda: self.wait_until(lambda: len(self.preferences()) == 1,
                                               "candidate was not automatically confirmed"), self_learning=True)
        candidate = self.jobs()[0]["candidates"][0]
        source = "learned-" + candidate["id"]
        preference = self.preferences()[0]
        self.assertEqual(preference["id"], source)
        self.assertEqual(preference["status"], "Confirmed")
        self.assertIn(preference["history"][0]["evidence_id"], candidate["draft"]["evidence_ids"])
        self.assertEqual(len(self.evidence()), 3, "automatic approval must not forge a statement")
        self.assertNotIn("eve.segment.preferences", self.documents())
        before = self.memory()
        self.inspect_run(self.quiet, self_learning=True)
        self.assertEqual(self.memory(), before)
        self.assertEqual(len(self.learning_requests()), 1)
        text = "可以。\n\n我们接着聊。\n\n我会按原文顺序发送。"
        two = ["可以。\n\n我们接着聊。", "我会按原文顺序发送。"]
        three = text.split("\n\n")
        self.run_eve([
            *self.send(self.message("auto-status", "/self-learning status", contains="自主学习：开启")),
            *self.send(self.message("auto-candidates", "/memory-candidates", contains="状态：已自动保存")),
            *self.segment_send(self.message("auto-chat", text), two),
            *self.segment_send(self.message("auto-other-user", text, user="other-user"), three),
            *self.send(self.message("auto-manual", "/segment parts 3", contains="设为 3 段")),
            *self.segment_send(self.message("auto-override-chat", text), three),
            *self.send(self.message("auto-reset", "/segment reset", contains="恢复跟随有效学习偏好")),
            *self.segment_send(self.message("auto-reset-chat", text), two),
            *self.send(self.message("auto-correct", "/correct-memory " + source + " 回复不要分段。", contains="偏好已修正")),
            *self.send(self.message("auto-correct-chat", text, expected=text)),
        ], self_learning=True)
        current = self.preferences()[0]
        self.assertEqual(current["revision"], 2)
        self.assertEqual(current["text"], "回复不要分段。")
        memory_context = json.dumps(self.chat_requests()[3]["body"], ensure_ascii=False)
        self.assertIn(self.candidate_text, memory_context)
        other_context = json.dumps(self.chat_requests()[4]["body"], ensure_ascii=False)
        self.assertNotIn(source, other_context)
        self.run_eve([
            *self.send(self.message("auto-restart-chat", text, expected=text)),
            *self.send(self.message("auto-forget", "/forget " + source, contains="偏好已撤销")),
            *self.segment_send(self.message("auto-forgotten-chat", text), three),
        ], self_learning=True)
        revoked = self.preferences()[0]
        self.assertEqual(revoked["status"], "Revoked")
        self.inspect_run(self.quiet, self_learning=True)
        self.assertEqual(self.preferences()[0], revoked, "restart must not revive or overwrite a revoked candidate")
        self.assertEqual(len(self.learning_requests()), 1)

    def test_autonomous_pending_completed_batch_is_reconciled_without_another_model_request(self):
        self.seed()
        candidate_id = self.generate()
        self.assertFalse(self.preferences())
        self.inspect_run(lambda: self.wait_until(lambda: len(self.preferences()) == 1,
                                               "completed pending batch did not resume confirmation"), self_learning=True)
        self.assertEqual(self.preferences()[0]["id"], "learned-" + candidate_id)
        self.assertEqual(len(self.learning_requests()), 1)

    def test_autonomous_weak_or_single_source_candidate_stays_pending(self):
        self.seed()
        self.candidate_confidence = 79
        self.inspect_run(lambda: self.wait_jobs(1), self_learning=True)
        self.inspect_run(self.quiet, self_learning=True)
        self.assertFalse(self.preferences())
        self.assertEqual(len(self.learning_requests()), 1)
        self.candidate_confidence = 100
        self.candidate_sources = 1
        self.seed(prefix="single", user="single-source-user")
        self.inspect_run(lambda: self.wait_jobs(2), self_learning=True)
        self.inspect_run(self.quiet, self_learning=True)
        self.assertFalse(self.preferences())
        self.assertEqual(len(self.learning_requests()), 2)

    def test_autonomous_new_chat_feedback_updates_rhythm_and_latest_context_without_commands(self):
        self.candidate_text = "回复最多分成两段，段间不要停顿。"
        self.seed(feedback=self.candidate_text)
        self.inspect_run(lambda: self.wait_until(lambda: len(self.preferences()) == 1, "initial automatic learning missing"),
                         self_learning=True, cooldown_ms=0)
        old_id = self.preferences()[0]["id"]
        self.candidate_text = "回复不要分段。"
        feedback = []
        for index in range(3):
            feedback.extend(self.send(self.message(f"natural-correction-{index}", "我改主意了，以后回复不要分段。")))
        self.inspect_run(lambda: self.wait_until(lambda: len(self.preferences()) == 2, "new chat correction not learned"),
                         prefix=feedback, self_learning=True, cooldown_ms=0)
        new_id = next(p["id"] for p in self.preferences() if p["id"] != old_id)
        text = "第一段。\n\n第二段。\n\n第三段。"
        self.run_eve(self.send(self.message("latest-learned-chat", text)), self_learning=True, cooldown_ms=0)
        context = self.preference_data(self.chat_requests()[-1])[0]["preferences"]
        self.assertEqual(context[0]["id"], new_id)
        self.assertEqual(context[1]["id"], old_id)
        # 撤销新来源自动回到仍有效的旧节奏；新候选不能用新 ID 复活同文撤销偏好。
        self.inspect_run(lambda: self.wait_jobs(3), prefix=[
            *self.send(self.message("revoke-natural-correction", "/forget " + new_id, contains="偏好已撤销")),
            *self.segment_send(self.message("fallback-learned-chat", text), ["第一段。", "第二段。\n\n第三段。"]),
            *self.send(self.message("new-batch-after-revoke", "以后还是别分段。")),
        ], self_learning=True, cooldown_ms=0)
        self.inspect_run(self.quiet, self_learning=True, cooldown_ms=0)
        self.assertEqual(len(self.preferences()), 2)
        self.assertEqual(next(p for p in self.preferences() if p["id"] == new_id)["status"], "Revoked")
        self.assertEqual(len(self.learning_requests()), 3)

    def test_autonomous_mode_continues_past_four_batch_startup_budget(self):
        for index in range(5):
            self.seed(prefix=f"continuous-{index}", user=f"continuous-user-{index}")
        self.inspect_run(lambda: self.wait_until(lambda: len(self.jobs()) == 5 and
                                               sum(len(s["preferences"]) for s in self.snapshots()) == 5,
                                               "autonomous learning stopped at the manual four-batch budget"),
                         self_learning=True)
        self.assertEqual(len(self.learning_requests()), 5)
        before = self.memory()
        self.inspect_run(self.quiet, self_learning=True)
        self.assertEqual(self.memory(), before)
        self.assertEqual(len(self.learning_requests()), 5)

    def test_confirmed_chat_candidate_becomes_adoptable_rhythm_without_extra_model_calls(self):
        self.candidate_text = "回复最多分成两段，段间不要停顿。"
        self.seed(feedback=self.candidate_text)
        candidate_id = self.generate()
        source = "learned-" + candidate_id
        learning_before = self.learning_bytes()
        memory_before = self.memory()
        self.run_eve(self.send(self.message("unconfirmed-advice", "/segment suggestions",
                                            contains="没有可采用的节奏建议")), segmented=True)
        self.assertEqual(self.memory(), memory_before)
        self.run_eve([*self.send(self.message("confirm-rhythm", "/accept-memory " + candidate_id,
                                             contains="候选偏好已确认：")),
                      *self.send(self.message("confirmed-advice", "/segment suggestions",
                                             contains=[source, "第 1 版", "最多 2 段", "停顿 0%", "查看不改变设置"]))],
                     segmented=True)
        self.assertNotIn("eve.segment.preferences", self.documents())
        self.assertEqual(self.learning_bytes(), learning_before)
        parts = ["可以。\n\n我们接着聊。", "我会按原文顺序发送。"]
        text = "可以。\n\n我们接着聊。\n\n我会按原文顺序发送。"
        delivery = {**self.message("rhythm-delivery", text), "expected_segments": parts}
        self.run_eve([*self.send(self.message("adopt-rhythm", f"/segment adopt {source} 1", contains="已采用偏好")),
                      {"send": delivery}, self.wait_receipt("rhythm-delivery")], segmented=True)
        sent = [event for event in self.events if event["direction"] == "out" and event["type"] == "segment"]
        self.assertEqual([event["text"] for event in sent], parts)
        self.assertEqual(self.receipt("rhythm-delivery")["reply"], text)
        self.assertEqual(self.learning_bytes(), learning_before)
        self.assertEqual(len(self.learning_requests()), 1)
        self.assertEqual(len(self.chat_requests()), 4)

    def assert_reserved_before_request(self, request):
        body = request["body"]
        self.assertEqual([message["role"] for message in body["messages"]], ["system", "user"])
        self.assertFalse(body.get("tools"))
        batch = json.loads(body["messages"][1]["content"])
        record = next(record for record in request["state"]["eve.learning"]["learning.v1"]["jobs"]
                      if record["job"]["batch"]["id"] == batch["id"])
        self.assertEqual(record["job"]["status"], "Running")
        self.assertEqual(record["job"]["batch"], batch)
        self.assertEqual(record["job"]["candidates"], [])
        sources = next(scope["snapshot"] for scope in request["state"]["eve.memory"]["memory.v1"]["scopes"]
                       if scope["snapshot"]["scope"] == batch["scope"])
        for evidence in batch["evidence"]:
            self.assertIn(evidence, sources["evidence"])
            self.assertEqual(evidence["source"]["kind"], "CompletedInteraction")
        return batch

    def test_disabled_and_invalid_commands_make_zero_model_requests(self):
        script = []
        for index, command in enumerate(("/memory-candidates", "/accept-memory id")):
            script.extend(self.send(self.message(f"disabled-{index}", command, DISABLED)))
        self.run_eve(script, learning=False, memory=False)
        self.assertNotIn("eve.learning", self.documents())
        script = []
        for index, command in enumerate(("/memory-candidates 0", "/memory-candidates -1",
                                         "/memory-candidates extra", "/accept-memory", "/accept-memory a b")):
            script.extend(self.send(self.message(f"invalid-{index}", command, HELP)))
        script.extend(self.send(self.message("empty", "/memory-candidates", EMPTY)))
        script.extend(self.send(self.message("missing", "/accept-memory missing", NOT_OWNED)))
        self.run_eve(script)
        self.assertEqual(self.requests, [])
        self.assertFalse(self.jobs())
        self.assertFalse(self.snapshots())
        result = subprocess.run([str(BINARY), "--memory-learning"], env={}, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("需要同时开启 --memory", result.stderr)
        self.assertEqual(self.requests, [])

    def test_three_delivered_interactions_trigger_one_candidate_without_context_confirmation(self):
        self.seed(2)
        def below_threshold():
            self.quiet()
            self.assertEqual(self.learning_requests(), [])
            self.assertEqual(self.jobs(), [])
        self.inspect_run(below_threshold)
        self.seed(1, prefix="third")
        candidate_id = self.generate()
        batch = self.assert_reserved_before_request(self.learning_requests()[0])
        self.assertEqual(len(batch["evidence"]), 3)
        self.assertEqual(self.only_snapshot()["preferences"], [])
        learning_before = self.learning_bytes()
        self.run_eve([*self.send(self.message("list", "/memory-candidates",
                                            contains=[candidate_id, "待确认", CANDIDATE, "83%", "来源证据："])),
                      *self.send(self.message("unconfirmed-chat", "没有确认候选时的普通对话"))])
        self.assertEqual(self.preference_data(self.chat_requests()[-1]), [])
        self.assertEqual(self.only_snapshot()["preferences"], [])
        self.assertEqual(self.learning_bytes(), learning_before)
        self.assertEqual(len(self.learning_requests()), 1)

    def test_confirmation_is_atomic_and_correct_revoke_repeat_accept_keep_current_context(self):
        self.seed()
        candidate_id = self.generate()
        preference_id = "learned-" + candidate_id
        command = "/accept-memory " + candidate_id
        learning_before = self.learning_bytes()
        def confirmed_before_reply_delivery():
            snapshot = self.only_snapshot()
            self.assertEqual(snapshot["revision"], 4)
            self.assertEqual(len(snapshot["preferences"]), 1)
            preference = snapshot["preferences"][0]
            self.assertEqual((preference["id"], preference["text"], preference["status"]),
                             (preference_id, CANDIDATE, "Confirmed"))
            source = next(item for item in snapshot["evidence"] if item["id"] == preference["history"][0]["evidence_id"])
            self.assertEqual(source["source"], {"kind": "UserStatement", "message_id": "accept", "text": command})
            self.assertEqual(self.receipt("accept")["state"], "ReplyPending")
            self.assertEqual(self.learning_bytes(), learning_before)
        message = {**self.message("accept", command, contains="候选偏好已确认："), "hold_delivery": True}
        self.run_eve([{"send": message}, self.wait_reply("accept"), *self.checkpoint("atomic"),
                      {"delivery": "accept"}, self.wait_receipt("accept")],
                     checkpoints={"atomic": confirmed_before_reply_delivery})
        confirmed = self.memory()
        self.run_eve([*self.duplicate("accept", command),
                      *self.send(self.message("accept-again", command, contains="候选偏好已确认："))])
        self.assertEqual(self.memory(), confirmed)
        self.run_eve(self.send(self.message("chat-confirmed", "确认后的对话")))
        self.run_eve([*self.send(self.message("correct", f"/correct-memory {preference_id} {CORRECTION}", contains="偏好已修正：")),
                      *self.send(self.message("accept-corrected", command, contains="已有后续修改：")),
                      *self.send(self.message("chat-corrected", "修正后的对话"))])
        self.run_eve([*self.send(self.message("forget", "/forget " + preference_id, contains="偏好已撤销：")),
                      *self.send(self.message("accept-revoked", command, contains="目前已撤销：")),
                      *self.send(self.message("chat-revoked", "撤销后的对话")),
                      *self.send(self.message("list-revoked", "/memory-candidates", contains=[candidate_id, "已撤销"]))])
        contexts = [self.preference_data(request) for request in self.chat_requests()[-3:]]
        self.assertEqual([item["text"] for item in contexts[0][0]["preferences"]], [CANDIDATE])
        self.assertEqual([item["text"] for item in contexts[1][0]["preferences"]], [CORRECTION])
        self.assertEqual(contexts[2], [])
        preference = self.only_snapshot()["preferences"][0]
        self.assertEqual([version["status"] for version in preference["history"]], ["Confirmed", "Confirmed", "Revoked"])
        # 又积累三条经历，但五分钟冷却跨进程保存；确认命令和重复确认没有消费新额度。
        self.inspect_run(self.quiet)
        self.assertEqual(self.learning_bytes(), learning_before)
        self.assertEqual(len(self.learning_requests()), 1)
        self.assertEqual(len(self.interactions()), 6)
        self.assertEqual(len(self.evidence()), 9)

    def test_user_group_and_application_boundaries_do_not_mix_or_reveal_candidates(self):
        self.seed(prefix="owner")
        self.seed(2, prefix="foreign-user", user="user-2")
        self.seed(1, prefix="foreign-group", scope="group")
        candidate_id = self.generate()
        batch = self.assert_reserved_before_request(self.learning_requests()[0])
        owner = next(snapshot for snapshot in self.snapshots()
                     if any(item["source"].get("message_id", "").startswith("owner-")
                            for item in snapshot["evidence"]))
        self.assertEqual(batch["scope"], owner["scope"])
        self.assertTrue(all(item["source"]["message_id"].startswith("owner-") for item in batch["evidence"]))
        before = self.memory()
        script = []
        for index, route in enumerate(({"user": "user-2"}, {"scope": "group"},
                                        {"scope": "group", "target": "other-group"})):
            script.extend(self.send(self.message(f"other-list-{index}", "/memory-candidates", EMPTY, **route)))
            script.extend(self.send(self.message(f"other-accept-{index}", "/accept-memory " + candidate_id, NOT_OWNED, **route)))
        self.run_eve(script)
        self.assertEqual(self.memory(), before)
        self.assertFalse(any(CANDIDATE in event.get("text", "") for event in self.events))
        self.run_eve([*self.send(self.message("other-app-list", "/memory-candidates", EMPTY)),
                      *self.send(self.message("other-app-accept", "/accept-memory " + candidate_id, NOT_OWNED))], app="other-app")
        self.assertEqual(self.memory(), before)
        self.assertFalse(any(CANDIDATE in event.get("text", "") for event in self.events))
        self.assertEqual(len(self.learning_requests()), 1)

    def test_provider_empty_invalid_and_tool_results_are_persisted_without_retry(self):
        original_work = self.work
        for mode, expected in (("provider", {"Failed": "Provider"}), ("empty", "Completed"),
                               ("invalid", {"Failed": "InvalidOutput"}), ("tool", {"Failed": "InvalidOutput"})):
            with self.subTest(mode=mode):
                self.work = original_work / mode
                self.work.mkdir()
                self.state_path = self.work / "state/state.json"
                self.learning_response = mode
                requests_before = len(self.learning_requests())
                self.seed()
                self.inspect_run(lambda: self.wait_jobs(1, expected))
                self.assertEqual(self.jobs()[0]["candidates"], [])
                before = self.learning_bytes()
                memory_before = self.memory()
                self.inspect_run(self.quiet, prefix=self.send(self.message("list-empty", "/memory-candidates", EMPTY)))
                self.assertEqual(self.learning_bytes(), before)
                self.assertEqual(self.memory(), memory_before)
                self.assertEqual(len(self.learning_requests()), requests_before + 1)
                self.assert_reserved_before_request(self.learning_requests()[-1])

    @unittest.skipUnless(os.name == "posix", "owned child signals use the Unix process interface")
    def test_cancelled_and_abruptly_interrupted_requests_are_consumed_without_restart_replay(self):
        original_work = self.work
        for abrupt in (False, True):
            with self.subTest(abrupt=abrupt):
                self.work = original_work / ("abrupt" if abrupt else "cancel")
                self.work.mkdir()
                self.state_path = self.work / "state/state.json"
                self.seed()
                number = len(self.learning_requests()) + 1
                self.response_gates = {("learning", number): "never-release"}
                stop_at = self.gate("stop")
                self.run_eve([{"wait_file": str(self.arrived("learning", number))},
                              {"touch": str(stop_at)}, {"wait_file": str(self.gate("stay-open"))}],
                             stop_at=stop_at, abrupt=abrupt)
                self.assertEqual(self.jobs()[0]["status"], "Running" if abrupt else {"Failed": "Cancelled"})
                expected = "Interrupted" if abrupt else {"Failed": "Cancelled"}
                self.inspect_run(lambda: (self.wait_jobs(1, expected), self.quiet()))
                recovered = self.learning_bytes()
                self.inspect_run(self.quiet)
                self.assertEqual(self.learning_bytes(), recovered)
                self.assertEqual(self.jobs()[0]["candidates"], [])
                self.assertEqual(len(self.learning_requests()), number)
                self.assertEqual(self.only_snapshot()["preferences"], [])

    def test_four_request_startup_budget_is_bounded_and_next_start_only_consumes_unused_scope(self):
        for index in range(5):
            self.seed(prefix=f"budget-{index}", user=f"user-{index}")
        def budget_reached():
            self.wait_jobs(4)
            self.quiet()
            self.assertEqual(len(self.learning_requests()), 4)
        self.inspect_run(budget_reached)
        first_jobs = self.jobs()
        scopes = [job["batch"]["scope"] for job in first_jobs]
        self.assertEqual(len({scope["session_id"] for scope in scopes}), 4)
        for request in self.learning_requests():
            self.assertEqual(len(self.assert_reserved_before_request(request)["evidence"]), 3)
        self.inspect_run(lambda: self.wait_jobs(5))
        self.assertEqual(len(self.learning_requests()), 5)
        self.assertEqual(self.jobs()[:4], first_jobs)
        before = self.learning_bytes()
        self.inspect_run(self.quiet)
        self.assertEqual(self.learning_bytes(), before)
        self.assertEqual(len(self.learning_requests()), 5)

    def test_extractor_receives_only_saved_chat_evidence_without_training_preferences_or_reflection(self):
        preference = "EXPLICIT_PRIVATE_PREFERENCE：此标记不得进入偏好提炼输入。"
        self.run_eve(self.send(self.message("remember", "/remember " + preference, contains="偏好已保存：")), learning=False)
        self.seed()
        before = self.memory()
        description = "只生成独立反思草稿，不属于用户聊天证据。"
        script = [*self.send(self.message("goal", "/goal " + description, contains="待办已保存：")),
                  {"wait_cognition": {"path": str(self.state_path), "parent_description": description,
                                      "child_state": "Completed"}}]
        self.inspect_run(lambda: self.wait_jobs(1), prefix=script, cognition=True, training=True)
        self.assertEqual(len(self.learning_requests()), 1)
        batch = self.assert_reserved_before_request(self.learning_requests()[0])
        self.assertEqual(len(batch["evidence"]), 3)
        encoded = json.dumps(self.learning_requests()[0]["body"], ensure_ascii=False)
        self.assertNotIn(preference, encoded)
        self.assertNotIn(description, encoded)
        self.assertNotIn(ARTIFACT["summary"], encoded)
        self.assertNotIn("主动提问训练模式", encoded)
        self.assertEqual(self.memory(), before)
        self.assertEqual(sum(request["kind"] == "reflection" for request in self.requests), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
