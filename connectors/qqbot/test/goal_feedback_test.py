"""QQ explicit feedback -> second reflection; local HTTP and an owned JSONL peer only."""
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINARY = pathlib.Path(os.environ.get("EVE_QQBOT_BINARY", ROOT / "target/debug/eve-qqbot"))
BRIDGE = ROOT / "connectors/qqbot/test/goal-feedback-bridge.mjs"
OLD = "FIRST_DRAFT_ONLY：还不知道执行范围。"
NEW = "SECOND_DRAFT_ONLY：已根据用户补充只整理书桌。"
THIRD = "THIRD_DRAFT_ONLY：保留书桌范围并将预算改为十分钟。"
FEEDBACK = "只整理书桌；不要移动书架，预算为二十分钟。"


class GoalFeedbackAcceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_file = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.gates = {}
        self.runs = 0
        self.events = []
        self.bindings = {}
        self.run_release = threading.Event()
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
                assert self.headers["Authorization"] == "Bearer test-model-secret"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert not body.get("tools")
                latest = body["messages"][-1]["content"]
                assert "unverified_waiting_input" in latest
                assert not any(message["role"] == "tool" for message in body["messages"])
                release = outer.run_release
                with outer.lock:
                    number = len(outer.requests) + 1
                    outer.requests.append({"body": body, "run": outer.runs, "state": outer.documents()})
                outer.arrived(number).touch()
                gate = outer.gates.get(number)
                deadline = time.monotonic() + 20
                while gate is not None and not outer.gate(gate).exists():
                    if release.wait(0.01):
                        return
                    assert time.monotonic() < deadline, "HTTP response gate timed out"
                artifact = {"summary": OLD if number == 1 else NEW if number == 2 else THIRD,
                            "next_step": "请用户确认本轮建议后再行动。", "needs_user_input": True}
                encoded = json.dumps({"choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": json.dumps(artifact, ensure_ascii=False)}}]}).encode()
                self.send_response(200)
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

    def documents(self):
        if not self.state_file.exists():
            return {}
        document = json.loads(self.state_file.read_text())
        return {owner: {key: json.loads(bytes(value)) for key, value in entries.items()}
                for owner, entries in document["entries"].items()}

    def cognition(self):
        return self.documents()["eve.cognition"]["cognition.v1"]["state"]

    def gate(self, name):
        return self.work / (name + ".gate")

    def arrived(self, number):
        return self.work / f"reflection-{number}.arrived"

    def message(self, message_id, text, contains=(), excludes=(), expected_type="reply",
                scope="c2c", user="user-1", target=None):
        return {"id": message_id, "text": text, "scope": scope, "user_id": user,
                "target_id": target or (user if scope == "c2c" else "group-1"),
                "expected_type": expected_type, "contains": list(contains), "excludes": list(excludes)}

    def send(self, message, count=1):
        script = [{"send": message}, {"wait_command": {"id": message["id"],
                  "type": message["expected_type"], "count": count}}]
        if message["expected_type"] == "reply":
            script.append({"wait_sent": message["id"]})
        return script

    def capture(self, name="before", source="goal"):
        return {"capture_goal": {"name": name, "source": source}}

    def wait_child(self, parent="before", status="Completed"):
        return {"wait_child": {"parent": parent, "status": status}}

    def add(self, **route):
        return [*self.send(self.message("goal", "/goal 整理房间的计划", contains=["待办已保存"], **route)),
                self.capture()]

    def feedback(self, message_id="feedback", parent="before", text=FEEDBACK, **route):
        return self.message(message_id, "/goal-feedback {{" + parent + ".id}} {{" + parent +
                            ".revision}} " + text, contains=["反馈已保存"], **route)

    def run_eve(self, script, budget=2, cognition=True, app="1904159860"):
        self.runs += 1
        self.run_release = threading.Event()
        events_file = self.work / f"events-{self.runs}.jsonl"
        error_file = self.work / f"error-{self.runs}.txt"
        bindings_file = self.work / f"bindings-{self.runs}.json"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "state_file": str(self.state_file),
            "events_file": str(events_file), "error_file": str(error_file),
            "bindings_file": str(bindings_file)}), encoding="utf8")
        environment = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        environment.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID=app,
            EVE_OPENAI_API_KEY="test-model-secret", EVE_OPENAI_PROTOCOL="chat",
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
            EVE_LLM_RESPONSE_MODE="complete")
        command = [str(BINARY), "--state-dir", str(self.work / "state"), "--agent", str(ROOT / "AGENT.md"),
                   "--bridge-script", str(BRIDGE), "--bridge-arg", str(scenario)]
        if cognition:
            command.extend(["--cognition", "--cognition-max-executions", str(budget)])
        child = subprocess.Popen(command, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            stdout, stderr = child.communicate(timeout=25)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
            self.run_release.set()
        self.events = [json.loads(line) for line in events_file.read_text().splitlines()] if events_file.exists() else []
        self.bindings = json.loads(bindings_file.read_text()) if bindings_file.exists() else {}
        self.assertFalse(error_file.exists(), error_file.read_text() if error_file.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ["test-model-secret", "test-app-secret"]:
            self.assertNotIn(secret, stdout + stderr)
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])
        self.assertEqual(summary["failed"], 0)
        return summary

    def assert_feedback_state(self, expected_requests=2):
        state = self.cognition()
        parent = state["goals"][self.bindings["after"]["id"]]
        self.assertEqual(parent["status"], "Waiting")
        self.assertGreater(parent["revision"], self.bindings["before"]["revision"])
        self.assertEqual(parent["description"], "整理房间的计划")
        self.assertIn(FEEDBACK, json.dumps(state, ensure_ascii=False))
        self.assertEqual(len(self.requests), expected_requests)
        self.assertFalse(any("主动提问训练模式" in json.dumps(request["body"], ensure_ascii=False)
                             for request in self.requests))
        for goal in state["goals"].values():
            if goal["source"]["kind"] == "Inference":
                self.assertEqual(goal["budget"]["max_tool_calls"], 0)
                self.assertEqual(goal["feedback"]["started_tools"], 0)

    def test_feedback_drives_second_reflection_hides_old_draft_and_restarts_without_replay(self):
        self.gates = {2: "release-second"}
        self.run_eve([
            *self.add(), self.wait_child(),
            *self.send(self.message("mind-first", "/mind {{before.id}}", contains=[OLD])),
            *self.send(self.feedback()), self.capture("after"),
            {"wait_file": str(self.arrived(2))},
            *self.send(self.message("mind-pending", "/mind {{after.id}}", excludes=[OLD, NEW])),
            {"touch": str(self.gate("release-second"))}, self.wait_child("after"),
            *self.send(self.message("mind-second", "/mind {{after.id}}", contains=[NEW], excludes=[OLD])),
            *self.send(self.message("stale-feedback", "/goal-feedback {{before.id}} {{before.revision}} STALE_FEEDBACK",
                                   contains=["目标版本已变化"], excludes=["反馈已保存", OLD, NEW])),
            *self.send(self.message("feedback", "duplicate changed payload", expected_type="finish")),
        ])
        self.assert_feedback_state()
        self.assertIn(FEEDBACK, self.requests[1]["body"]["messages"][-1]["content"])
        self.assertNotIn(FEEDBACK, self.requests[0]["body"]["messages"][-1]["content"])
        self.assertNotIn("STALE_FEEDBACK", json.dumps(self.cognition(), ensure_ascii=False))
        persisted = self.cognition()
        parent_id = self.bindings["after"]["id"]
        self.run_eve([
            *self.send(self.message("feedback", "duplicate after restart", expected_type="finish")),
            *self.send(self.message("restart-mind", "/mind " + parent_id, contains=[NEW], excludes=[OLD])),
        ])
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.cognition(), persisted)

    def test_feedback_while_old_reflection_is_in_flight_never_exposes_old_result(self):
        self.gates = {1: "release-first", 2: "release-second"}
        self.run_eve([
            *self.add(), {"wait_file": str(self.arrived(1))}, self.wait_child(status="Executing"),
            *self.send(self.feedback()), self.capture("after"),
            *self.send(self.message("mind-inflight", "/mind {{after.id}}", excludes=[OLD, NEW])),
            {"touch": str(self.gate("release-first"))}, {"wait_file": str(self.arrived(2))},
            *self.send(self.message("mind-old-done", "/mind {{after.id}}", excludes=[OLD, NEW])),
            {"touch": str(self.gate("release-second"))}, self.wait_child("after"),
            *self.send(self.message("mind-new-done", "/mind {{after.id}}", contains=[NEW], excludes=[OLD])),
        ])
        self.assert_feedback_state()
        self.assertIn(FEEDBACK, self.requests[1]["body"]["messages"][-1]["content"])

    def test_third_reflection_keeps_earlier_scope_and_uses_new_budget_feedback(self):
        newest = "时间预算改为十分钟；必须先归类文具。"
        self.run_eve([
            *self.add(), self.wait_child(),
            *self.send(self.feedback()), self.capture("after"), self.wait_child("after"),
            *self.send(self.feedback("feedback-third", parent="after", text=newest)),
            self.capture("third"), self.wait_child("third"),
            *self.send(self.message("mind-third", "/mind {{third.id}}", contains=[THIRD], excludes=[OLD, NEW])),
        ], budget=3)
        self.assertEqual(len(self.requests), 3)
        third_input = self.requests[2]["body"]["messages"][-1]["content"]
        self.assertIn(FEEDBACK, third_input)
        self.assertIn(newest, third_input)
        self.assertIn("整理房间的计划", third_input)
        self.assertNotIn(OLD, third_input)
        self.assertNotIn(NEW, third_input)
        payload = json.loads(third_input.splitlines()[-1])
        self.assertEqual(payload["unverified_user_feedback"]["text"], newest)
        self.assertEqual(payload["previous_user_feedback"][0]["text"], FEEDBACK)
        self.assertFalse(payload["history_truncated"])
        parent = self.cognition()["goals"][self.bindings["third"]["id"]]
        self.assertEqual(parent["status"], "Waiting")
        self.assertGreater(parent["revision"], self.bindings["after"]["revision"])
        current_state = self.cognition()
        parent_id = parent["id"]
        self.run_eve(self.send(self.message("third-restart", "/mind " + parent_id,
                                            contains=[THIRD], excludes=[OLD, NEW])))
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(self.cognition(), current_state)

    def test_exhausted_budget_still_saves_feedback_and_next_process_only_runs_new_revision(self):
        self.run_eve([
            *self.add(), self.wait_child(), *self.send(self.feedback()), self.capture("after"),
            *self.send(self.message("mind-exhausted", "/mind {{after.id}}", excludes=[OLD, NEW])),
        ], budget=1)
        self.assert_feedback_state(expected_requests=1)
        previous_children = {goal["id"]: goal for goal in self.cognition()["goals"].values()
                             if goal["source"]["kind"] == "Inference"}
        self.run_eve([
            self.capture("after"), self.wait_child("after"),
            *self.send(self.message("mind-recovered", "/mind {{after.id}}", contains=[NEW], excludes=[OLD])),
        ], budget=1)
        self.assertEqual(len(self.requests), 2)
        for child_id, child in previous_children.items():
            self.assertEqual(self.cognition()["goals"][child_id], child)

    def test_feedback_rejects_cross_user_group_and_app_without_model_requests(self):
        self.run_eve([*self.add(scope="group"), self.wait_child()], budget=1)
        parent_id = self.bindings["before"]["id"]
        revision = self.bindings["before"]["revision"]
        text = f"/goal-feedback {parent_id} {revision} FOREIGN_FEEDBACK"
        before = self.cognition()
        script = []
        for index, route in enumerate([{"scope": "group", "user": "user-2"},
                                       {"scope": "group", "target": "group-2"}, {"scope": "c2c"}]):
            script += self.send(self.message("foreign-" + str(index), text,
                                contains=["未找到"], excludes=[OLD, NEW], **route))
        self.run_eve(script)
        self.assertEqual(self.cognition(), before)
        self.run_eve(self.send(self.message("foreign-app", text, contains=["未找到"], scope="group")),
                     app="other-app")
        self.assertEqual(self.cognition(), before)
        self.assertEqual(len(self.requests), 1)

    def test_disabled_and_malformed_feedback_commands_never_call_a_model(self):
        self.run_eve(self.send(self.message("disabled", "/goal-feedback target 1 条件",
                                                contains=["未启用"])), cognition=False)
        script = []
        for index, text in enumerate(["/goal-feedback", "/goal-feedback target", "/goal-feedback target no 条件",
                                      "/goal-feedback target 1", "/goal-feedback target 0 条件",
                                      "/goal-feedback target 18446744073709551616 条件"]):
            script += self.send(self.message("malformed-" + str(index), text, contains=["用法"]))
        self.run_eve(script)
        self.assertEqual(self.requests, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
