"""实际 eve-qqbot 消息判断验收；所有模型与 QQ 都由本地替身提供。

这些场景验证接线、回退和代际边界，不代表 Jev/主模型的真实语义质量。
进程停止只调用本测试持有的 Popen；不读取 PID 文件，不向进程组发信号。
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

import eve_e2e as support

ROOT = support.ROOT
BINARY = support.BINARY
CANCELLED = support.CANCELLED
NO_CURRENT = support.NO_CURRENT
SIDE_EFFECTS = support.SIDE_EFFECTS
BRIDGE = ROOT / "connectors/qqbot/test/message-judge-bridge.mjs"
CLARIFY = "请明确这条消息是补充、纠正、澄清答复还是新任务。"
UNCHANGED = "当前任务保持不变。"
LABELS = ("supplement", "correction", "answer", "new_task", "cancel", "continue",
          "unrelated", "ambiguous", "pause", "resume")


class MessageJudgeAcceptance(unittest.TestCase):
    # Shared scenario vocabulary and state decoding, without inheriting other suites.
    message = support.Acceptance.message
    gate = support.Acceptance.gate
    wait_command = support.Acceptance.wait_command
    open_gate = support.Acceptance.open_gate
    documents = support.Acceptance.documents
    sessions = support.Acceptance.sessions
    replies = support.Acceptance.replies
    assert_cancelled = support.Acceptance.assert_cancelled

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.calls = {kind: [] for kind in ("task", "primary", "jev")}
        self.steps = {}
        self.errors = []
        self.lock = threading.Lock()
        self.release = threading.Event()
        self.runs = 0
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    self.respond()
                except (BrokenPipeError, ConnectionResetError):
                    pass  # The owned Eve process cancelled this request.
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    try:
                        self.send_error(500)
                    except (BrokenPipeError, ConnectionResetError):
                        pass

            def respond(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path == "/v1/systemone":
                    kind = "jev"
                    assert self.headers["Authorization"] == "Bearer test-jev-secret"
                    assert body["model"] == "test-jev-model"
                    assert set(body) == {"model", "state", "questions"}
                    assert set(body["questions"]) == {"intent", "whole_message"}
                else:
                    assert self.path == "/v1/chat/completions"
                    assert self.headers["Authorization"] == "Bearer test-model-secret"
                    assert body["model"] == "test-primary-role"
                    assert body["stream"] is False
                    assert body["max_tokens"] == 256
                    messages = body["messages"]
                    judge = (len(messages) == 2 and messages[0]["role"] == "system"
                             and messages[0]["content"].startswith("判断最新消息与当前任务的关系。"))
                    kind = "primary" if judge else "task"
                    if judge:
                        assert not body.get("tools")
                if kind != "task":
                    state = body["state"] if kind == "jev" else json.loads(body["messages"][-1]["content"])
                    assert set(state) == {"message", "task_text", "phase", "cancel_requested",
                                          "started_tools", "clarification"}
                    # Routing identifiers and complete task history remain in the host.
                    assert isinstance(state["message"], str)
                    assert isinstance(state["task_text"], str)
                with outer.lock:
                    index = len(outer.calls[kind]) + 1
                    outer.calls[kind].append({"body": body, "at": time.monotonic(),
                                              "wall": time.time(), "documents": outer.documents()})
                outer.arrived(kind, index).touch()
                step = outer.steps.get((kind, index), {})
                if "wait" in step:
                    deadline = time.monotonic() + 15
                    while not outer.gate(step["wait"]).exists():
                        if outer.release.wait(0.01):
                            return
                        assert time.monotonic() < deadline, "response gate timed out"
                if "status" in step:
                    self.send_json({"error": "synthetic failure"}, step["status"])
                    return
                if kind == "jev":
                    intent = step.get("intent", "correction")
                    answer = {"answers": {
                        "intent": {"type": "choice", "choice": intent,
                                   "confidence": step.get("confidence", 0.99),
                                   "probabilities": {label: 1.0 if label == intent else 0.0 for label in LABELS}},
                        "whole_message": {"type": "noul", "noul": 0.99}}}
                    self.send_json({"answers": {}} if step.get("invalid") else answer)
                    return
                if kind == "primary":
                    intent = step.get("intent", "correction")
                    needs_text = intent in ("supplement", "correction", "answer", "new_task")
                    answer = {"parts": [{"intent": intent, "confidence": step.get("confidence", 99),
                                          "text": state["message"] if needs_text else None}],
                              "explanation": "离线替身的固定分类"}
                    text = "invalid response" if step.get("invalid") else json.dumps(answer, ensure_ascii=False)
                    self.send_chat({"role": "assistant", "content": text})
                    return
                latest = body["messages"][-1]
                if "text" in step:
                    reply = step["text"]
                elif latest["role"] == "tool":
                    reply = json.loads(latest["content"])["echo"]
                elif latest["content"].startswith("echo:"):
                    marker = latest["content"].split(":", 1)[1]
                    self.send_chat({"role": "assistant", "content": None,
                                    "tool_calls": [{"id": "echo-call", "type": "function",
                                                    "function": {"name": "echo", "arguments": json.dumps({"text": marker})}}]},
                                   reason="tool_calls")
                    return
                else:
                    reply = latest["content"]
                self.send_chat({"role": "assistant", "content": reply})

            def send_chat(self, message, reason="stop"):
                self.send_json({"choices": [{"index": 0, "finish_reason": reason, "message": message}]})

            def send_json(self, document, status=200):
                encoded = json.dumps(document, ensure_ascii=False).encode()
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
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.directory.cleanup()

    def arrived(self, kind, index=1):
        return self.work / f"{kind}-{index}.arrived"

    def wait_request(self, kind, index=1):
        return {"wait_file": str(self.arrived(kind, index))}

    def wait_receipt(self, id, state="Sent"):
        return {"wait_receipt": {"path": str(self.work / "state/state.json"), "id": id, "state": state}}

    def run_eve(self, script, mode="jev", timeout_ms=2000, terminate_at=None, environment=None, success=True):
        self.runs += 1
        events = self.work / f"events-{self.runs}.jsonl"
        error = self.work / f"error-{self.runs}.txt"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events), "error_file": str(error)}),
                            encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete",
                   EVE_OPENAI_MODEL_ROLE="primary", EVE_MODELS_PRIMARY_ENABLED="true",
                   EVE_MODELS_PRIMARY_PROVIDER="openai", EVE_MODELS_PRIMARY_MODEL="test-primary-role",
                   EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS="256", EVE_MODELS_PRIMARY_TIMEOUT_MS="10000",
                   EVE_JEV_API_KEY="test-jev-secret", EVE_JEV_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_MODELS_JEV_ENABLED="true", EVE_MODELS_JEV_PROVIDER="jev",
                   EVE_MODELS_JEV_MODEL="test-jev-model", EVE_MODELS_JEV_TIMEOUT_MS="10000",
                   EVE_MODELS_JEV_MAX_CONCURRENT_REQUESTS="1", EVE_MESSAGE_JUDGE_TIMEOUT_MS=str(timeout_ms))
        for name, value in (environment or {}).items():
            if value is None:
                env.pop(name, None)
            else:
                env[name] = value
        command = [str(BINARY), "--state-dir", str(self.work / "state"), "--agent", str(ROOT / "AGENT.md"),
                   "--bridge-script", str(BRIDGE), "--bridge-arg", str(scenario)]
        if mode is not None:
            command.extend(["--message-judge", mode])
        child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            if terminate_at:
                deadline = time.monotonic() + 12
                while not terminate_at.exists() and child.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(terminate_at.exists(), "owned process did not reach stop synchronization")
                child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=25)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=10)
        self.bridge_events = ([json.loads(line) for line in events.read_text().splitlines()] if events.exists() else [])
        self.assertFalse(error.exists(), error.read_text() if error.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ("test-app-secret", "test-model-secret", "test-jev-secret"):
            self.assertNotIn(secret, stdout + stderr)
        if not success:
            self.assertNotEqual(child.returncode, 0)
            return stderr
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])
        return summary

    def counts(self):
        return tuple(len(self.calls[kind]) for kind in ("task", "primary", "jev"))

    def test_default_queues_ordinary_messages_without_any_judgement(self):
        self.steps[("task", 1)] = {"wait": "release-base"}
        self.run_eve([
            {"send": self.message("base", "原始任务")}, self.wait_request("task"),
            {"send": self.message("queued", "改成两页")},
            {"send": self.message("barrier", "/continue", UNCHANGED)}, self.wait_command("barrier"),
            self.open_gate("release-base"), self.wait_receipt("base"), self.wait_receipt("queued")], mode=None)
        self.assertEqual(self.counts(), (2, 0, 0))
        self.assertEqual([turn["input"] for turn in self.sessions()[0]["turns"]], ["原始任务", "改成两页"])

    def test_slash_rules_and_malformed_slash_never_call_a_judge(self):
        self.steps[("task", 1)] = {"wait": "release-base"}
        self.run_eve([
            {"send": self.message("base", "原始任务")}, self.wait_request("task"),
            {"send": self.message("keep", "/continue", UNCHANGED)}, self.wait_command("keep"),
            {"send": self.message("malformed", "/correct", CLARIFY)}, self.wait_command("malformed"),
            self.open_gate("release-base"), self.wait_receipt("base")])
        self.assertEqual(self.counts(), (1, 0, 0))

    def revision_scenario(self, mode="jev", jev_step=None, timeout_ms=2000):
        text = "  把页数改为两页，保留预算 📚。\n原文中的‘取消’只是引用。  "
        self.steps[("task", 1)] = {"wait": "never-release-old", "text": "不得发送旧回复"}
        self.steps[("task", 2)] = {"text": "修订完成"}
        if jev_step:
            self.steps[("jev", 1)] = jev_step
        original = self.message("base", "原始任务", expected_type="finish")
        revised = self.message("revision", text, "修订完成", expected_type="reply")
        self.run_eve([{"send": original}, self.wait_request("task"), {"send": revised},
                      self.wait_command("base", "finish"), self.wait_receipt("revision")],
                     mode=mode, timeout_ms=timeout_ms)
        self.assertEqual(self.replies(), [("revision", "修订完成")])
        replacement = self.calls["task"][1]
        request = json.loads(replacement["body"]["messages"][-1]["content"])
        self.assertEqual(request["request_kind"], "revision")
        self.assertEqual(request["base_request"], "原始任务")
        self.assertEqual(request["changes"], [{"kind": "correction", "text": text}])
        before_reply = self.sessions(replacement["documents"])[0]["turns"]
        self.assert_cancelled(before_reply[0])
        self.assertEqual(before_reply[1]["status"]["state"], "Pending")
        return original, revised

    def test_jev_revision_keeps_raw_utf8_filters_old_reply_and_restart_deduplicates(self):
        original, revised = self.revision_scenario()
        self.assertEqual(self.counts(), (2, 0, 1))
        replay = {**revised, "expected_type": "finish"}
        self.run_eve([{"send": [original, replay]}, self.wait_command("base", "finish"),
                      self.wait_command("revision", "finish"), {"send": self.message("fresh", "新消息")},
                      self.wait_receipt("fresh")])
        self.assertEqual(self.counts(), (3, 0, 1))
        self.assertEqual(self.replies(), [("fresh", "新消息")])

    def test_primary_mode_resolves_the_primary_role_without_jev(self):
        self.revision_scenario(mode="primary")
        self.assertEqual(self.counts(), (2, 1, 0))

    def test_low_confidence_jev_falls_back_once(self):
        self.revision_scenario(jev_step={"confidence": 0.4})
        self.assertEqual(self.counts(), (2, 1, 1))

    def test_jev_http_error_falls_back_once(self):
        self.revision_scenario(jev_step={"status": 503})
        self.assertEqual(self.counts(), (2, 1, 1))

    def test_jev_protocol_error_falls_back_once(self):
        self.revision_scenario(jev_step={"invalid": True})
        self.assertEqual(self.counts(), (2, 1, 1))

    def test_jev_timeout_leaves_half_of_total_budget_for_primary(self):
        self.revision_scenario(jev_step={"wait": "never-release-jev"}, timeout_ms=600)
        self.assertEqual(self.counts(), (2, 1, 1))
        elapsed = self.calls["primary"][0]["at"] - self.calls["jev"][0]["at"]
        self.assertGreaterEqual(elapsed, 0.20)
        self.assertLess(elapsed, 0.80)

    def clarification_scenario(self, primary_step, jev_step, timeout_ms=2000):
        self.steps[("task", 1)] = {"wait": "release-base"}
        self.steps[("jev", 1)] = jev_step
        self.steps[("primary", 1)] = primary_step
        self.run_eve([
            {"send": self.message("base", "原始任务")}, self.wait_request("task"),
            {"send": self.message("ambiguous", "帮我调整一下", CLARIFY)}, self.wait_receipt("ambiguous"),
            self.open_gate("clarified"), self.open_gate("release-base"), self.wait_receipt("base")],
            timeout_ms=timeout_ms)
        self.assertEqual(self.counts(), (1, 1, 1))
        self.assertEqual(self.sessions()[0]["turns"][0]["status"]["state"], "Completed")
        self.assertEqual(self.replies(), [("ambiguous", CLARIFY), ("base", "原始任务")])

    def test_both_judges_fail_clarifies_without_cancelling_current_task(self):
        self.clarification_scenario({"invalid": True}, {"invalid": True})

    def test_two_slow_judges_share_one_deadline_then_clarify(self):
        self.clarification_scenario({"wait": "never-release-primary"}, {"wait": "never-release-jev"},
                                    timeout_ms=600)
        # Marker follows actual persisted clarification delivery, not a fixed test sleep.
        elapsed = self.gate("clarified").stat().st_mtime - self.calls["jev"][0]["wall"]
        self.assertGreaterEqual(elapsed, 0.45)
        self.assertLess(elapsed, 1.20)

    def test_other_users_and_groups_have_no_access_to_inflight_task(self):
        self.steps[("task", 1)] = {"wait": "release-owner"}
        others = [self.message("user-two", "改成两页", scope="group", user="user-2"),
                  self.message("group-two", "改成三页", scope="group", target="group-2"),
                  self.message("private", "改成四页")]
        self.run_eve([
            {"send": self.message("owner", "群内原始任务", scope="group")}, self.wait_request("task"),
            {"send": others},
            {"send": self.message("barrier", "/continue", NO_CURRENT, scope="group", user="user-3")},
            self.wait_command("barrier"), self.open_gate("release-owner"), self.wait_receipt("owner"),
            *[self.wait_receipt(message["id"]) for message in others]])
        self.assertEqual(self.counts(), (4, 0, 0))
        self.assertEqual(len(self.sessions()), 4)
        self.assertTrue(all(s["turns"][0]["status"]["state"] == "Completed" for s in self.sessions()))

    def test_revision_after_tool_start_clarifies_without_replaying_tool(self):
        self.steps[("task", 2)] = {"wait": "never-release-tool"}
        self.run_eve([
            {"send": self.message("tool", "echo:side-effect-marker", expected_type="finish")},
            self.wait_request("task", 2),
            {"send": self.message("revision", "改成另一个值", SIDE_EFFECTS)},
            self.wait_command("tool", "finish"), self.wait_receipt("revision"),
            {"send": self.message("fresh", "/new 新任务", "新任务")}, self.wait_receipt("fresh")])
        self.assertEqual(self.counts(), (3, 0, 1))
        self.assertEqual(self.calls["task"][1]["body"]["messages"][-1]["role"], "tool")
        self.assert_cancelled(self.sessions()[0]["turns"][0], tools=1)
        self.assertEqual(len(self.sessions()[0]["turns"]), 2)

    def test_explicit_cancel_supersedes_pending_natural_judgement(self):
        self.steps[("task", 1)] = {"wait": "never-release-old"}
        self.steps[("jev", 1)] = {"wait": "never-release-jev"}
        self.run_eve([
            {"send": self.message("base", "原始任务", expected_type="finish")}, self.wait_request("task"),
            {"send": self.message("natural", "改成两页", expected_type="finish")}, self.wait_request("jev"),
            {"send": self.message("cancel", "/cancel", CANCELLED)}, self.wait_command("base", "finish"),
            self.wait_command("natural", "finish"), self.wait_receipt("cancel")], timeout_ms=10000)
        self.assertEqual(self.counts(), (1, 0, 1))
        self.assertEqual(self.replies(), [("cancel", CANCELLED)])
        self.assert_cancelled(self.sessions()[0]["turns"][0])

    def test_sigterm_cancels_judgement_and_restart_never_replays_old_messages(self):
        self.steps[("task", 1)] = {"wait": "never-release-old"}
        self.steps[("jev", 1)] = {"wait": "never-release-jev"}
        original = self.message("base", "原始任务", expected_type="finish")
        natural = self.message("natural", "改成两页", expected_type="finish")
        summary = self.run_eve([{"send": original}, self.wait_request("task"), {"send": natural},
                               self.wait_request("jev"), {"wait_file": str(self.gate("owned-stop"))}],
                              timeout_ms=10000, terminate_at=self.arrived("jev"))
        self.assertEqual(summary["sent"], 0)
        self.assertEqual(self.counts(), (1, 0, 1))
        self.assert_cancelled(self.sessions()[0]["turns"][0])
        self.run_eve([{"send": [original, natural]}, self.wait_command("base", "finish"),
                      self.wait_command("natural", "finish"), {"send": self.message("fresh", "新任务")},
                      self.wait_receipt("fresh")])
        self.assertEqual(self.counts(), (2, 0, 1))
        self.assertEqual(self.replies(), [("fresh", "新任务")])

    def test_jev_key_is_required_independently_of_primary_key(self):
        self.run_eve([], environment={"EVE_JEV_API_KEY": None}, success=False)
        self.assertEqual(self.counts(), (0, 0, 0))


if __name__ == "__main__":
    unittest.main()
