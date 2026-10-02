"""Actual Eve process + production plugin + fake Node/loopback Chat HTTP."""
import json
import os
import pathlib
import subprocess
import signal
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINARY = ROOT / "target/debug/eve-qqbot"
FAKE = ROOT / "connectors/qqbot/test/fake-bridge.mjs"

class Acceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.requests = []
        self.expected_model = "deepseek-v4.1-flash"
        self.started = threading.Event()
        self.release = threading.Event()
        outer = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                assert self.path == "/v1/chat/completions"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                outer.requests.append(body)
                assert body["model"] == outer.expected_model
                assert body["reasoning_effort"] == "none"
                messages = body["messages"]
                latest = messages[-1]
                reason = "stop"
                if latest.get("content") == "wait":
                    outer.started.set()
                    outer.release.wait(timeout=20)
                if latest["role"] == "tool":
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

    def message(self, id, text, expected=None, scope="c2c", user="user-1"):
        return {"id": id, "scope": scope, "target_id": user if scope == "c2c" else "group-1",
            "user_id": user, "text": text, "expected": expected if expected is not None else text}

    def run_eve(self, messages, send_fail=False, app="1904159860", success=True, cancel=False, environment=None):
        scenario = self.work / "scenario.json"
        scenario.write_text(json.dumps({"messages": messages, "send_fail": send_fail, "pid_file": str(self.work / "child.pid")}), encoding="utf8")
        env = {k: os.environ[k] for k in ("PATH", "SystemRoot", "TEMP", "TMP") if k in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID=app,
            EVE_OPENAI_API_KEY="test-model-secret",
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
            EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        env.update(environment or {})
        command = [str(BINARY), "--state-dir", str(self.work / "state"),
            "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE),
            "--bridge-arg", str(scenario)]
        if cancel:
            child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
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

if __name__ == "__main__":
    unittest.main()
