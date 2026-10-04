"""Actual Eve process + production plugin + fake Node/loopback Chat HTTP."""
import json
import os
import pathlib
import subprocess
import signal
import tempfile
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINARY = pathlib.Path(os.environ.get("EVE_QQBOT_BINARY", ROOT / "target/debug/eve-qqbot"))
FAKE = ROOT / "connectors/qqbot/test/fake-bridge.mjs"
CANCELLED = "已取消当前任务；已完成的工具操作不会撤销。"
NO_CURRENT = "当前会话没有可控制的任务，请先发送普通文字开始任务。"
SIDE_EFFECTS = "已有工具执行或副作用未知；请确认接下来要执行的新任务。"
UNSUPPORTED = "尚未支持任务暂停恢复；请明确继续、取消或开始新任务。"

class Acceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.requests = []
        self.request_documents = []
        self.provider_steps = {}
        self.server_errors = []
        self.request_lock = threading.Lock()
        self.runs = 0
        self.expected_model = "deepseek-v4.1-flash"
        self.started = threading.Event()
        self.release = threading.Event()
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
                    outer.server_errors.append(traceback.format_exc())
                    self.send_error(500)
            def respond(self):
                assert self.path == "/v1/chat/completions"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with outer.request_lock:
                    outer.requests.append(body)
                    index = len(outer.requests)
                    outer.request_documents.append(outer.documents())
                assert body["model"] == outer.expected_model
                assert body["reasoning_effort"] == "none"
                outer.request_arrived(index).touch()
                messages = body["messages"]
                latest = messages[-1]
                reason = "stop"
                step = outer.provider_steps.get(index, {})
                if "wait" in step:
                    deadline = time.monotonic() + 15
                    while not outer.gate(step["wait"]).exists():
                        if outer.release.wait(timeout=0.02):
                            return
                        assert time.monotonic() < deadline, "HTTP response gate timed out"
                if latest.get("content") == "wait":
                    outer.started.set()
                    outer.release.wait(timeout=20)
                if "text" in step:
                    message = {"role": "assistant", "content": step["text"]}
                elif latest["role"] == "tool":
                    text = json.loads(latest["content"])["echo"]
                    message = {"role": "assistant", "content": text, "reasoning_content": "auxiliary"}
                elif latest["content"].startswith("echo:"):
                    marker = latest["content"].split(":", 1)[1]
                    reason = "tool_calls"
                    message = {"role": "assistant", "content": "tool explanation",
                        "tool_calls": [{"id": "call-1", "type": "function",
                        "function": {"name": "echo", "arguments": json.dumps({"text": marker})}}]}
                elif latest["content"].startswith("recall:"):
                    marker = latest["content"].split(":", 1)[1]
                    assert any(m.get("role") == "assistant" and m.get("content") == marker for m in messages[:-1])
                    message = {"role": "assistant", "content": marker}
                else:
                    message = {"role": "assistant", "content": latest["content"]}
                encoded = json.dumps({"choices": [{"index": 0, "finish_reason": reason, "message": message}]}).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded))); self.end_headers()
                try:
                    self.wfile.write(encoded)
                except BrokenPipeError:
                    pass
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.release.set()
        self.server.shutdown(); self.server.server_close(); self.thread.join()
        self.directory.cleanup()

    def message(self, id, text, expected=None, scope="c2c", user="user-1", target=None,
            expected_type=None):
        message = {"id": id, "scope": scope, "target_id": target or (user if scope == "c2c" else "group-1"),
            "user_id": user, "text": text, "expected": expected if expected is not None else text}
        if expected_type:
            message["expected_type"] = expected_type
        return message

    def gate(self, name):
        return self.work / (name + ".gate")

    def request_arrived(self, index):
        return self.work / f"request-{index}.arrived"

    def wait_request(self, index):
        return {"wait_file": str(self.request_arrived(index))}

    def wait_command(self, id, kind="reply", count=1):
        return {"wait_command": {"id": id, "type": kind, "count": count}}

    def open_gate(self, name):
        return {"touch": str(self.gate(name))}

    def run_eve(self, messages=(), send_fail=False, app="1904159860", success=True, cancel=False,
            environment=None, script=None, cancel_when=None, training=False):
        self.runs += 1
        scenario = self.work / "scenario.json"
        events = self.work / f"bridge-events-{self.runs}.jsonl"
        bridge_error = self.work / f"bridge-error-{self.runs}.txt"
        scenario.write_text(json.dumps({"messages": messages, "script": script, "send_fail": send_fail,
            "pid_file": str(self.work / "child.pid"), "events_file": str(events),
            "error_file": str(bridge_error)}), encoding="utf8")
        env = {k: os.environ[k] for k in ("PATH", "SystemRoot", "TEMP", "TMP") if k in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID=app,
            EVE_OPENAI_API_KEY="test-model-secret",
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
            EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        env.update(environment or {})
        command = [str(BINARY), "--state-dir", str(self.work / "state"),
            "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE),
            "--bridge-arg", str(scenario)]
        if training:
            command.append("--training")
        if cancel:
            child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                if cancel_when:
                    deadline = time.monotonic() + 10
                    while not cancel_when.exists() and child.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.02)
                    self.assertTrue(cancel_when.exists(), "stop synchronization point not reached")
                else:
                    self.assertTrue(self.started.wait(timeout=10), "model request did not start")
                child.send_signal(signal.SIGTERM)
                stdout, stderr = child.communicate(timeout=10)
            finally:
                if child.poll() is None:
                    child.kill(); child.wait()
                self.release.set()
            result = subprocess.CompletedProcess(command, child.returncode, stdout, stderr)
        else:
            result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20)
        self.bridge_events = [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []
        self.assertFalse(bridge_error.exists(), bridge_error.read_text() if bridge_error.exists() else "")
        self.assertFalse(self.server_errors, "\n".join(self.server_errors))
        self.assertNotIn("test-app-secret", result.stdout + result.stderr)
        self.assertNotIn("test-model-secret", result.stdout + result.stderr)
        if not success:
            self.assertNotEqual(result.returncode, 0); return result
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(result.stdout)
        self.assertTrue(summary["closed"]); self.assertFalse(summary["terminal_error"])
        return summary

    def documents(self):
        state = json.loads((self.work / "state/state.json").read_text())
        return {owner: {key: json.loads(bytes(value)) for key, value in entries.items()}
            for owner, entries in state["entries"].items()}

    def has_training(self, request):
        return any(m["role"] == "system" and "主动提问训练模式" in m.get("content", "") for m in request["messages"])

    def test_builtin_expression_learning_from_old_receipts_is_local_idempotent_and_resettable(self):
        # 旧版记录没有表达统计；启动新版仅在本地导入，不回放请求或投递。
        self.run_eve([self.message(f"el-{i}", "今天不错", scope="group") for i in range(8)])
        state_path = self.work / "state/state.json"
        saved = json.loads(state_path.read_text())
        saved["entries"].pop("eve.training", None)
        state_path.write_text(json.dumps(saved))
        before = len(self.requests)
        self.run_eve(training=True)
        self.assertEqual(len(self.requests), before)
        self.assertEqual(self.replies(), [])
        learned = self.documents()["eve.training"]["expression.v1"]
        self.assertEqual(len(learned["rows"]), 8)
        self.assertNotIn("今天不错", json.dumps(learned, ensure_ascii=False))
        self.run_eve(training=True)
        self.assertEqual(self.documents()["eve.training"]["expression.v1"], learned)
        self.run_eve([self.message("el-stats", "/train stats", "本会话已学习 8 条有效用户表达：字数中位数 4，短消息 100%，单段消息 100%。当前要求始终优先；这是本地表达统计。", scope="group"),
                      self.message("el-other", "/train stats", "本会话暂无有效表达样本；训练开启后会从普通交流中学习。", scope="group", user="user-2"),
                      self.message("el-task", "详细解释这个任务", scope="group")], training=True)
        self.assertEqual(len(self.requests), before + 1)
        self.assertTrue(any(m["role"] == "system" and "当前可信会话" in m.get("content", "") for m in self.requests[-1]["messages"]))
        self.run_eve([self.message("el-reset", "/train reset", "已重置本会话的表达统计，原始聊天记录保留；旧消息不会再次计入，可继续从新消息学习。", scope="group")], training=True)
        self.run_eve(training=True)
        self.assertFalse(any(r["active"] for r in self.documents()["eve.training"]["expression.v1"]["rows"]))
        self.assertEqual(len(self.requests), before + 1)
        self.assertEqual(len(self.sessions()[0]["turns"]), 9)

    def test_training_questions_use_real_history_stop_persists_and_ordinary_chat_continues(self):
        self.provider_steps = {1: {"text": "你喜欢我怎样称呼你？"}, 2: {"text": "收到。你希望回复更简短吗？"}}
        self.run_eve([self.message("train-1", "/train start", "你喜欢我怎样称呼你？"),
                      self.message("train-2", "叫我小星", "收到。你希望回复更简短吗？"),
                      self.message("train-3", "/train stop", "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。"),
                      self.message("train-4", "普通任务")])
        self.assertEqual(len(self.requests), 3)
        self.assertTrue(self.has_training(self.requests[0]))
        self.assertTrue(self.has_training(self.requests[1]))
        self.assertFalse(self.has_training(self.requests[2]))
        self.assertIn({"role": "assistant", "content": "你喜欢我怎样称呼你？"}, self.requests[1]["messages"])
        self.assertEqual([t["input"] for t in self.sessions()[0]["turns"]], ["/train start", "叫我小星", "普通任务"])
        # 显式 stop 的持久开关覆盖下一进程的默认训练选项。
        self.run_eve([self.message("train-5", "重启后聊天")], training=True)
        self.assertFalse(self.has_training(self.requests[-1]))

    def test_training_group_scope_does_not_change_other_sender_or_group(self):
        self.run_eve([self.message("tg-1", "/train stop", "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。", scope="group"),
                      self.message("tg-2", "同群另一人", scope="group", user="user-2"),
                      self.message("tg-3", "另一群", scope="group", target="group-2"),
                      self.message("tg-4", "原会话", scope="group")], training=True)
        self.assertEqual([self.has_training(r) for r in self.requests], [True, True, False])
        for request in self.requests:
            self.assertFalse(any(m["role"] == "assistant" for m in request["messages"]))

    def test_training_stop_cancels_inflight_question_and_never_sends_it(self):
        self.provider_steps = {1: {"wait": "training-release", "text": "不应发送的旧问题？"}}
        self.run_eve(script=[{"send": self.message("ts-1", "/train start", expected_type="finish")},
            self.wait_request(1), {"send": self.message("ts-2", "/train stop", "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。")},
            self.wait_command("ts-1", "finish"), self.wait_command("ts-2")])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.replies(), [("ts-2", "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。")])
        self.assertEqual(self.sessions()[0]["turns"][0]["status"]["state"], "Failed")
        self.assertFalse(self.documents()["eve.training"]["modes.v1"]["modes"][0]["enabled"])

    def test_training_start_receipt_not_replayed_and_corrupt_modes_retained(self):
        self.run_eve([self.message("tr-1", "/train start")])
        self.run_eve([self.message("tr-1", "/train start")])
        self.assertEqual(len(self.requests), 1)
        state_path = self.work / "state/state.json"
        state = json.loads(state_path.read_text())
        state["entries"]["eve.training"]["modes.v1"] = list(b'{"version":99,"modes":[]}')
        state_path.write_text(json.dumps(state))
        before = state_path.read_bytes()
        self.run_eve(success=False)
        self.assertEqual(state_path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)

    def test_training_stop_suppresses_completed_question_waiting_for_delivery(self):
        self.provider_steps = {1: {"wait": "training-complete", "text": "应丢弃的完成问题？"}}
        held = self.message("tc-status", "/train status", "当前会话：主动提问训练已开启。")
        held["hold_delivery"] = True
        self.run_eve(script=[{"send": self.message("tc-start", "/train start", expected_type="finish")},
            self.wait_request(1), {"send": held}, self.wait_command("tc-status"), self.open_gate("training-complete"),
            {"wait_turn": {"path": str(self.work / "state/state.json"), "input": "/train start", "state": "Completed"}},
            {"send": self.message("tc-stop", "/train stop", "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。")},
            self.wait_command("tc-start", "finish"), {"delivery": "tc-status"}, self.wait_command("tc-stop")])
        self.assertEqual(self.replies(), [("tc-status", "当前会话：主动提问训练已开启。"),
            ("tc-stop", "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。")])

    def test_training_window_ready_checkpoint_and_encrypted_completed_results(self):
        self.provider_steps = {1: {"text": "你希望每段几句话？"}}
        launcher = self.work / "launch-eve"
        scenario = self.work / "window-scenario.json"
        scenario.write_text(json.dumps({"script": [
            {"send": self.message("tw-1", "/train start", "你希望每段几句话？")},
            self.wait_command("tw-1"), {"wait_file": str(self.gate("window-close"))}]}))
        launcher.write_text("#!/usr/bin/env python3\nimport os, sys\nos.execv(" + repr(str(BINARY)) + ", [" + repr(str(BINARY)) + ", *sys.argv[1:], '--bridge-script', " + repr(str(FAKE)) + ", '--bridge-arg', " + repr(str(scenario)) + "])\n")
        launcher.chmod(0o700)
        env = {k: os.environ[k] for k in ("PATH", "SystemRoot", "TEMP", "TMP") if k in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_SANDBOX="false", EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}", EVE_OPENAI_PROTOCOL="chat")
        script = ROOT / "connectors/qqbot/training-window.py"
        root, output = self.work / "window", self.work / "results"
        # 共享既有 HTTP 夹具的持久化准入检查目录。
        (self.work / "state").symlink_to(root / "state", target_is_directory=True)
        child = subprocess.Popen(["python3", str(script), "--root", str(root), "--output", str(output),
                                  "--seconds", "60", "--binary", str(launcher)], cwd=ROOT, env=env,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready = subprocess.run(["python3", str(script), "--wait-ready", "--root", str(root), "--output", str(output)],
                                   cwd=ROOT, env=env, capture_output=True, text=True, timeout=15)
            self.assertEqual(ready.returncode, 0, ready.stdout + ready.stderr)
            checkpoint = json.loads((output / "ready.json").read_text())
            self.assertTrue(checkpoint["ready"])
            self.assertFalse(checkpoint["sandbox"])
            self.gate("window-close").touch()
            stdout, stderr = child.communicate(timeout=15)
            self.assertEqual(child.returncode, 0, stdout + stderr)
            self.assertNotIn("你希望每段几句话", stdout + stderr)
            self.assertNotIn("test-app-secret", stdout + stderr)
            report = json.loads((output / "training-summary.json").read_text())
            self.assertTrue(report["process_ok"])
            self.assertTrue(report["interaction_ok"])
            self.assertEqual(report["counts"]["question_replies"], 1)
            decrypted = self.work / "decrypted"
            result = subprocess.run(["python3", str(script), "--decrypt", str(output / "training-state.enc"), "--output", str(decrypted)],
                                    cwd=ROOT, env=env, capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            evidence = json.loads((decrypted / "training-report.json").read_text())["evidence"]
            self.assertEqual(evidence[0]["input"], "/train start")
            self.assertEqual(evidence[0]["reply"], "你希望每段几句话？")
        finally:
            if child.poll() is None:
                child.kill(); child.wait()

    def test_tool_reply_restart_recall_and_duplicate_no_replay(self):
        first = self.run_eve([self.message("ROBOT1.0_.b6nx.CVryAO0nR58RXuU6SC.m92gc19j02qKqdm8ek!", "echo:marker", "marker")])
        self.assertEqual((first["completed"], first["sent"], len(self.requests)), (1, 1, 2))
        second = self.run_eve([self.message("ROBOT1.0_.b6nx.CVryAO0nR58RXuU6SC.m92gc19j02qKqdm8ek!", "echo:marker", "marker"),
            self.message("in-2", "recall:marker", "marker")])
        self.assertEqual((second["received"], second["sent"], len(self.requests)), (1, 1, 3))
        docs = self.documents()
        session = next(iter(docs["eve.session"]["sessions.v1"]["sessions"].values()))
        self.assertEqual(session["revision"], 4)
        self.assertEqual(len(session["turns"]), 2)
        self.assertEqual(session["turns"][0]["status"]["state"], "Completed")

    def test_group_user_and_app_routing_isolation(self):
        self.run_eve([self.message("g-1", "one", scope="group")])
        self.run_eve([self.message("g-2", "two", scope="group", user="user-2")])
        self.run_eve([self.message("g-3", "three", scope="group")], app="other-app")
        self.assertEqual(len(self.documents()["eve.session"]["sessions.v1"]["sessions"]), 3)
        for request, expected in zip(self.requests, ["one", "two", "three"]):
            messages = request["messages"]
            self.assertEqual([m["content"] for m in messages if m["role"] == "user"], [expected])
            self.assertFalse(any(m["role"] in ("assistant", "tool") for m in messages))

    def test_long_group_message_ids_reply_restore_and_deduplicate(self):
        observed_length_id = "ROBOT1.0_" + "g" * 128
        boundary_id = "m" * 253
        self.run_eve([self.message(observed_length_id, "群聊记忆", scope="group"),
                      self.message(boundary_id, "recall:群聊记忆", "群聊记忆", scope="group")])
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.replies(), [(observed_length_id, "群聊记忆"), (boundary_id, "群聊记忆")])
        self.run_eve([self.message(observed_length_id, "重复不重放", scope="group"),
                      self.message("restore-long-group", "recall:群聊记忆", "群聊记忆", scope="group")])
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(self.replies(), [("restore-long-group", "群聊记忆")])

    def test_message_id_utf8_boundary_rejects_oversize_before_model_and_ledger(self):
        valid = "界" * 84 + "!"
        self.run_eve(script=[
            {"send": self.message("x" * 254, "超长不执行", scope="group")},
            {"send": self.message("界" * 85, "UTF-8 超长不执行", scope="group")},
            {"send": self.message(valid, "字节边界", scope="group")},
            self.wait_command(valid)])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.replies(), [(valid, "字节边界")])
        entries = self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"]
        self.assertEqual([(e["message"]["id"], e["state"]) for e in entries], [(valid, "Sent")])

    def test_send_failure_preserves_commit_no_model_or_send_retry(self):
        result = self.run_eve([self.message("in-1", "echo:marker", "marker")], send_fail=True)
        self.assertEqual((result["completed"], result["sent"], result["failed"]), (1, 0, 1))
        again = self.run_eve([self.message("in-1", "changed", "ignored")])
        self.assertEqual((again["received"], again["sent"], len(self.requests)), (0, 0, 2))
        record = self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"][0]
        self.assertEqual(record["state"], "Failed"); self.assertEqual(record["reply"], "marker")

    def test_sigterm_cancels_active_model_and_reaps_node(self):
        summary = self.run_eve([self.message("in-wait", "wait")], cancel=True)
        self.assertEqual((summary["received"], summary["completed"], summary["sent"]), (1, 0, 0))
        pid = int((self.work / "child.pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        documents = self.documents()
        turn = next(iter(documents["eve.session"]["sessions.v1"]["sessions"].values()))["turns"][0]
        self.assertEqual(turn["status"]["state"], "Failed")
        self.assertEqual(turn["status"]["failure"]["code"], "Cancelled")
        record = documents["eve.channel.qqbot"]["receipts.v1"]["entries"][0]
        self.assertEqual(record["state"], "Processing")
        after = self.run_eve([self.message("in-wait", "wait"), self.message("in-fresh", "fresh")])
        self.assertEqual((after["received"], after["sent"]), (1, 1))
        self.assertEqual(len(self.requests), 2)
        self.assertEqual([m["content"] for m in self.requests[-1]["messages"] if m["role"] == "user"], ["fresh"])

    def test_processing_receipt_does_not_reexecute_after_restart(self):
        self.run_eve([self.message("in-1", "one")])
        path = self.work / "state/state.json"
        state = json.loads(path.read_text())
        key = state["entries"]["eve.channel.qqbot"]["receipts.v1"]
        ledger = json.loads(bytes(key))
        ledger["entries"][0].update(state="Processing", reply=None)
        state["entries"]["eve.channel.qqbot"]["receipts.v1"] = list(json.dumps(ledger).encode())
        path.write_text(json.dumps(state))
        summary = self.run_eve([self.message("in-1", "one")])
        self.assertEqual(summary["received"], 0)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"][0]["state"], "Processing")

    def test_corrupt_receipts_not_cleared(self):
        self.run_eve([self.message("in-1", "one")])
        path = self.work / "state/state.json"
        state = json.loads(path.read_text())
        state["entries"]["eve.channel.qqbot"]["receipts.v1"] = list(b'{"version":999,"entries":[]}')
        path.write_text(json.dumps(state))
        before = path.read_bytes()
        self.run_eve([self.message("in-2", "two")], success=False)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)

    def primary_environment(self, model, output_limit="64"):
        return {
            "EVE_OPENAI_MODEL_ROLE": "primary",
            "EVE_MODELS_PRIMARY_ENABLED": "true",
            "EVE_MODELS_PRIMARY_PROVIDER": "openai",
            "EVE_MODELS_PRIMARY_MODEL": model,
            "EVE_MODELS_PRIMARY_TIMEOUT_MS": "5000",
            "EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS": output_limit,
        }

    def test_primary_role_controls_qq_model_and_restart_keeps_history(self):
        self.expected_model = "qq-primary-one"
        first = self.run_eve([self.message("role-1", "echo:marker", "marker")],
            environment=self.primary_environment(self.expected_model))
        self.assertEqual((first["completed"], first["sent"], len(self.requests)), (1, 1, 2))
        self.assertTrue(all(request["model"] == "qq-primary-one" and request["max_tokens"] == 64
            for request in self.requests))
        self.expected_model = "qq-primary-two"
        second = self.run_eve([self.message("role-2", "recall:marker", "marker")],
            environment=self.primary_environment(self.expected_model, "32"))
        self.assertEqual((second["completed"], second["sent"], len(self.requests)), (1, 1, 3))
        self.assertEqual(self.requests[-1]["model"], "qq-primary-two")
        self.assertEqual(self.requests[-1]["max_tokens"], 32)
        session = next(iter(self.documents()["eve.session"]["sessions.v1"]["sessions"].values()))
        self.assertEqual(session["revision"], 4)

    def test_disabled_primary_role_preserves_qq_state_and_starts_no_request(self):
        self.run_eve([self.message("existing", "saved")])
        path = self.work / "state/state.json"
        before = path.read_bytes()
        environment = self.primary_environment("unused")
        environment["EVE_MODELS_PRIMARY_ENABLED"] = "false"
        self.run_eve([self.message("must-not-run", "new")], success=False, environment=environment)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)

    def sessions(self, documents=None):
        return list((documents or self.documents())["eve.session"]["sessions.v1"]["sessions"].values())

    def replies(self):
        return [(event["id"], event["text"]) for event in self.bridge_events
            if event.get("direction") == "out" and event.get("type") == "reply"]

    def assert_cancelled(self, turn, tools=0):
        self.assertEqual(turn["status"], {"state": "Failed", "failure": {
            "code": "Cancelled", "started_tools": tools}})

    def test_explicit_cancel_inflight_filters_old_reply_and_restart_does_not_replay(self):
        self.provider_steps[1] = {"wait": "never-release-old", "text": "old-result"}
        original = self.message("active", "original", expected_type="finish")
        cancel = self.message("cancel", "/cancel", CANCELLED, expected_type="reply")
        summary = self.run_eve(script=[{"send": original}, self.wait_request(1), {"send": cancel},
            self.wait_command("active", "finish"), self.wait_command("cancel")])
        self.assertEqual((summary["received"], summary["sent"], len(self.requests)), (2, 1, 1))
        self.assertEqual(self.replies(), [("cancel", CANCELLED)])
        self.assert_cancelled(self.sessions()[0]["turns"][0])
        self.run_eve([original, cancel, self.message("after-restart-control", "/cancel", NO_CURRENT),
            self.message("fresh", "fresh")])
        self.assertEqual(len(self.requests), 2)
        self.assertEqual([m["content"] for m in self.requests[-1]["messages"] if m["role"] == "user"], ["fresh"])
        self.assertEqual(self.replies(), [("after-restart-control", NO_CURRENT), ("fresh", "fresh")])

    def revision_scenario(self, command, expected_changes):
        self.provider_steps.update({1: {"wait": "never-release-old", "text": "old-result"},
            2: {"text": "revised-result"}})
        original = self.message("original", "base-request", expected_type="finish")
        revision = self.message("revision", command, "revised-result", expected_type="reply")
        summary = self.run_eve(script=[{"send": original}, self.wait_request(1), {"send": revision},
            self.wait_command("original", "finish"), self.wait_command("revision")])
        self.assertEqual((summary["received"], summary["sent"], len(self.requests)), (2, 1, 2))
        self.assertEqual(self.replies(), [("revision", "revised-result")])
        replacement = json.loads(self.requests[1]["messages"][-1]["content"])
        self.assertEqual(replacement["request_kind"], "revision")
        self.assertEqual(replacement["base_request"], "base-request")
        self.assertEqual(replacement["changes"], expected_changes)
        # This snapshot is taken inside the replacement HTTP handler, before its
        # reply: the original must already be durably settled when it starts.
        before_response = self.sessions(self.request_documents[1])[0]["turns"]
        self.assertEqual(len(before_response), 2)
        self.assert_cancelled(before_response[0])
        self.assertEqual(before_response[1]["status"]["state"], "Pending")
        self.assertEqual([m["role"] for m in self.requests[1]["messages"] if m["role"] != "system"], ["user"])
        self.run_eve([original, revision, self.message("recall", "recall:revised-result", "revised-result")])
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(self.replies(), [("recall", "revised-result")])
        turns = self.sessions()[0]["turns"]
        self.assertEqual([turn["status"]["state"] for turn in turns], ["Failed", "Completed", "Completed"])

    def test_add_waits_for_cancel_commit_before_replacement_and_recovers(self):
        self.revision_scenario("/add added-condition", [{"kind": "supplement", "text": "added-condition"}])

    def test_correct_and_add_preserve_original_order_before_replacement(self):
        self.revision_scenario("/correct corrected-request\n/add added-condition", [
            {"kind": "correction", "text": "corrected-request"},
            {"kind": "supplement", "text": "added-condition"}])

    def test_executed_tool_clarifies_without_replay_until_explicit_new_task(self):
        self.provider_steps[2] = {"wait": "never-release-tool", "text": "old-tool-result"}
        original = self.message("tool-original", "echo:tool-marker", expected_type="finish")
        correction = self.message("tool-correction", "/correct changed", SIDE_EFFECTS, expected_type="reply")
        fresh = self.message("tool-new", "/new fresh", "fresh", expected_type="reply")
        self.run_eve(script=[{"send": original}, self.wait_request(2), {"send": correction},
            self.wait_command("tool-original", "finish"), self.wait_command("tool-correction"),
            {"send": fresh}, self.wait_command("tool-new")])
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(self.requests[1]["messages"][-1]["role"], "tool")
        self.assertEqual(json.loads(self.requests[1]["messages"][-1]["content"]), {"echo": "tool-marker"})
        self.assertEqual(self.replies(), [("tool-correction", SIDE_EFFECTS), ("tool-new", "fresh")])
        turns = self.sessions()[0]["turns"]
        self.assert_cancelled(turns[0], tools=1)
        self.assertEqual(turns[1]["input"], "fresh")
        self.assertEqual([m["role"] for m in self.requests[2]["messages"] if m["role"] != "system"], ["user"])

    def test_completed_tool_correction_keeps_history_and_never_reexecutes(self):
        self.run_eve([self.message("done-tool", "echo:tool-marker", "tool-marker"),
            self.message("done-correct", "/correct changed", SIDE_EFFECTS),
            self.message("done-new", "/new recall:tool-marker", "tool-marker")])
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(self.replies(), [("done-tool", "tool-marker"),
            ("done-correct", SIDE_EFFECTS), ("done-new", "tool-marker")])
        self.assertEqual([turn["status"]["state"] for turn in self.sessions()[0]["turns"]],
            ["Completed", "Completed"])

    def test_control_cannot_cancel_another_user_or_group_inflight(self):
        self.provider_steps[1] = {"wait": "release-isolated", "text": "owner-result"}
        original = self.message("owner", "owner-task", scope="group", expected="owner-result",
            expected_type="reply")
        strangers = [
            self.message("other-user", "/cancel", NO_CURRENT, scope="group", user="user-2", expected_type="reply"),
            self.message("other-group", "/correct intrusion", NO_CURRENT, scope="group", target="group-2", expected_type="reply"),
            self.message("other-scope", "/cancel", NO_CURRENT, expected_type="reply"),
        ]
        self.run_eve(script=[{"send": original}, self.wait_request(1), {"send": strangers},
            *[self.wait_command(message["id"]) for message in strangers], self.open_gate("release-isolated"),
            self.wait_command("owner")])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(len(self.sessions()), 1)
        self.assertEqual(self.sessions()[0]["turns"][0]["status"]["state"], "Completed")
        self.assertCountEqual(self.replies(), [(m["id"], NO_CURRENT) for m in strangers] + [("owner", "owner-result")])

    def test_duplicate_control_and_changed_route_do_not_resubmit(self):
        self.provider_steps.update({1: {"wait": "never-release-old", "text": "old-result"},
            2: {"wait": "release-revision", "text": "revised"}})
        original = self.message("dupe-original", "base", expected_type="finish")
        revision = self.message("dupe-control", "/correct revised", "revised", expected_type="reply")
        changed = self.message("dupe-control", "/cancel", "ignored", user="user-2")
        self.run_eve(script=[{"send": [original, original]}, self.wait_request(1),
            {"send": [revision, revision]}, self.wait_request(2), {"send": changed},
            self.open_gate("release-revision"), self.wait_command("dupe-original", "finish"),
            self.wait_command("dupe-control")])
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.replies(), [("dupe-control", "revised")])
        entries = self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"]
        self.assertEqual(len(entries), 2)
        saved = next(entry for entry in entries if entry["message"]["id"] == "dupe-control")
        self.assertEqual(saved["message"]["user_id"], "user-1")
        self.assertEqual(saved["message"]["text"], "/correct revised")

    def test_sigterm_during_replacement_reaps_bridge_and_restart_does_not_replay(self):
        self.provider_steps.update({1: {"wait": "never-release-old", "text": "old-result"},
            2: {"wait": "never-release-revision", "text": "revision-result"}})
        original = self.message("stop-original", "original", expected_type="finish")
        revision = self.message("stop-revision", "/add condition", expected_type="finish")
        summary = self.run_eve(script=[{"send": original}, self.wait_request(1), {"send": revision},
            self.wait_request(2), {"wait_file": str(self.gate("process-stop"))}],
            cancel=True, cancel_when=self.request_arrived(2))
        self.assertEqual(summary["sent"], 0)
        self.assertEqual(self.replies(), [])
        for turn in self.sessions()[0]["turns"]:
            self.assert_cancelled(turn)
        with self.assertRaises(ProcessLookupError):
            os.kill(int((self.work / "child.pid").read_text()), 0)
        self.run_eve([original, revision, self.message("stop-fresh", "fresh")])
        self.assertEqual(len(self.requests), 3)
        self.assertEqual([m["content"] for m in self.requests[-1]["messages"] if m["role"] == "user"], ["fresh"])

    def test_pause_resume_and_answer_clarify_without_cancelling(self):
        self.provider_steps[1] = {"wait": "release-kept", "text": "kept-result"}
        controls = [self.message("pause", "/pause", UNSUPPORTED, expected_type="reply"),
            self.message("resume", "/resume", UNSUPPORTED, expected_type="reply"),
            self.message("answer", "/answer detail", "请引用当前任务最近一次澄清问题，或明确说明新要求。", expected_type="reply")]
        self.run_eve(script=[{"send": self.message("kept", "kept", "kept-result", expected_type="reply")},
            self.wait_request(1), *[step for message in controls for step in (
                {"send": message}, self.wait_command(message["id"]))],
            self.open_gate("release-kept"), self.wait_command("kept")])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.sessions()[0]["turns"][0]["status"]["state"], "Completed")
        self.assertEqual(self.replies(), [(message["id"], message["expected"]) for message in controls]
            + [("kept", "kept-result")])

    def test_completed_old_result_waiting_for_delivery_is_filtered_after_revision(self):
        self.provider_steps.update({1: {"wait": "release-completed", "text": "committed-old-result"},
            2: {"text": "revised-result"}})
        keep = self.message("queued-keep", "/continue", "当前任务保持不变。", expected_type="reply")
        keep["hold_delivery"] = True
        original = self.message("queued-old", "old-input", expected_type="finish")
        revision = self.message("queued-revise", "/correct replacement", "revised-result", expected_type="reply")
        self.run_eve(script=[{"send": original}, self.wait_request(1), {"send": keep},
            self.wait_command("queued-keep"), self.open_gate("release-completed"),
            {"wait_turn": {"path": str(self.work / "state/state.json"), "input": "old-input", "state": "Completed"}},
            {"send": revision}, self.wait_request(2), {"delivery": "queued-keep"},
            self.wait_command("queued-old", "finish"), self.wait_command("queued-revise")])
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.replies(), [("queued-keep", "当前任务保持不变。"), ("queued-revise", "revised-result")])
        turns = self.sessions()[0]["turns"]
        self.assertEqual([turn["status"]["state"] for turn in turns], ["Completed", "Completed"])
        self.assertTrue(any(message["role"] == "assistant" and message["content"] == "committed-old-result"
            for message in self.requests[1]["messages"]))

if __name__ == "__main__":
    unittest.main()
