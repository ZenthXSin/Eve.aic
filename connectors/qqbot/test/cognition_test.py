"""实际 eve-qqbot 进程的内生反思验收；只使用本地 HTTP 和替身 QQ 桥接。"""
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

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINARY = pathlib.Path(os.environ.get("EVE_QQBOT_BINARY", ROOT / "target/debug/eve-qqbot"))
FAKE = ROOT / "connectors/qqbot/test/fake-bridge.mjs"
ARTIFACT = {
    "summary": "PRIVATE_REFLECTION_MARKER：当前只有待办，尚缺少验收条件。",
    "next_step": "请用户说明执行范围，再决定是否采取行动。",
    "needs_user_input": True,
}
DISABLED = "内生反思未启用。"
NO_GOALS = "当前会话还没有待办，发送 /goal 内容 添加。"
NO_OWNED = "当前会话未找到该待办，发送 /goals 查看。"
HELP = "用法：/goal 待办内容、/goals、/mind [目标ID]。"
CANCELLED = "已取消当前任务；已完成的工具操作不会撤销。"


class CognitionAcceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_path = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.runs = 0
        self.response_gates = {}
        self.reflection_text = json.dumps(ARTIFACT, ensure_ascii=False)
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
                kind = "reflection" if "unverified_waiting_input" in latest else "chat"
                release = outer.run_release
                with outer.lock:
                    outer.requests.append({"kind": kind, "body": body, "run": outer.runs,
                                           "state": outer.documents()})
                    number = sum(request["kind"] == kind for request in outer.requests)
                outer.arrived(kind, number).touch()
                gate = outer.response_gates.get((kind, number))
                if gate is not None:
                    deadline = time.monotonic() + 20
                    while not outer.gate(gate).exists():
                        if release.wait(0.01):
                            return
                        assert time.monotonic() < deadline, "response gate timed out"
                text = outer.reflection_text if kind == "reflection" else latest
                encoded = json.dumps({"choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": text}}]}).encode()
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
        if not self.state_path.exists():
            return {}
        document = json.loads(self.state_path.read_text())
        return {owner: {key: json.loads(bytes(value)) for key, value in entries.items()}
                for owner, entries in document["entries"].items()}

    def goals(self):
        return self.documents()["eve.cognition"]["cognition.v1"]["state"]["goals"]

    def parent(self, description):
        return next(goal for goal in self.goals().values()
                    if goal["source"]["kind"] == "User" and goal["description"] == description)

    def child(self, description):
        parent = self.parent(description)
        return next(goal for goal in self.goals().values()
                    if goal["source"]["reference"] == parent["id"]
                    and goal["verification"] == "reflection:v1")

    def gate(self, name):
        return self.work / f"{name}.gate"

    def arrived(self, kind, number=1):
        return self.work / f"{kind}-{number}.arrived"

    def message(self, id, text, expected=None, contains=None, scope="c2c", user="user-1", target=None):
        message = {"id": id, "text": text, "scope": scope, "user_id": user,
                   "target_id": target or (user if scope == "c2c" else "group-1"),
                   "expected_type": "reply"}
        if contains is None:
            message["expected"] = text if expected is None else expected
        else:
            message["expected_contains"] = contains
        return message

    def wait_reply(self, id):
        return {"wait_command": {"id": id, "type": "reply"}}

    def wait_sent(self, id):
        return {"wait_receipt": {"path": str(self.state_path), "id": id, "state": "Sent"}}

    def wait_child(self, description, state="Completed"):
        return {"wait_cognition": {"path": str(self.state_path),
                "parent_description": description, "child_state": state}}

    def send(self, message):
        return [{"send": message}, self.wait_reply(message["id"]), self.wait_sent(message["id"])]

    def create_and_inspect(self, description, id="goal-1", **route):
        return [*self.send(self.message(id, "/goal " + description,
                    contains="待办已保存：", **route)),
                self.wait_child(description),
                *self.send(self.message(id + "-mind", "/mind",
                    contains=["反思草稿", ARTIFACT["summary"], "原待办未完成"], **route))]

    def run_eve(self, script, cognition=True, training=False, app="1904159860", cancel_at=None):
        self.runs += 1
        self.run_release = threading.Event()
        events_path = self.work / f"events-{self.runs}.jsonl"
        error_path = self.work / f"error-{self.runs}.txt"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events_path),
            "error_file": str(error_path), "pid_file": str(self.work / "child.pid")}), encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID=app,
            EVE_OPENAI_API_KEY="test-model-secret",
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
            EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        command = [str(BINARY), "--state-dir", str(self.work / "state"),
            "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE),
            "--bridge-arg", str(scenario)]
        if cognition:
            command.extend(["--cognition", "--cognition-max-executions", "1"])
        if training:
            command.append("--training")
        child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            if cancel_at:
                deadline = time.monotonic() + 15
                while not cancel_at.exists() and child.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(cancel_at.exists(), "SIGTERM synchronization was not reached")
                child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=25)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
            self.run_release.set()
        self.events = [json.loads(line) for line in events_path.read_text().splitlines()] if events_path.exists() else []
        self.assertFalse(error_path.exists(), error_path.read_text() if error_path.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ["test-model-secret", "test-app-secret"]:
            self.assertNotIn(secret, stdout + stderr)
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])
        self.assertEqual(summary["failed"], 0)
        return summary

    def assert_one_reflection(self, description):
        requests = [request for request in self.requests if request["kind"] == "reflection"]
        self.assertEqual(len(requests), 1)
        request = requests[0]["body"]
        observed_goals = requests[0]["state"]["eve.cognition"]["cognition.v1"]["state"]["goals"]
        self.assertTrue(any(goal["description"] == description and goal["status"] == "Waiting"
                            for goal in observed_goals.values()))
        self.assertTrue(any(goal["verification"] == "reflection:v1" and goal["status"] == "Executing"
                            for goal in observed_goals.values()))
        self.assertFalse(request.get("tools"))
        self.assertFalse(any(message["role"] == "tool" for message in request["messages"]))
        self.assertFalse(any("主动提问训练模式" in message.get("content", "")
                             for message in request["messages"]))
        parent = self.parent(description)
        child = self.child(description)
        self.assertEqual(parent["status"], "Waiting")
        self.assertEqual(child["status"], "Completed")
        self.assertTrue(child["feedback"]["verification_met"])
        self.assertEqual(child["feedback"]["started_tools"], 0)
        self.assertEqual(child["budget"]["max_model_requests"], 1)
        self.assertEqual(child["budget"]["max_tool_calls"], 0)
        sessions = self.documents()["eve.session"]["sessions.v1"]["sessions"]
        turn = next(turn for session in sessions.values() for turn in session["turns"]
                    if session["key"]["session_id"] == child["execution"]["session_id"]
                    and turn["id"] == child["execution"]["turn_id"])
        self.assertEqual(turn["status"]["state"], "Completed")
        self.assertFalse(any(message["tool_calls"] or message["tool_results"]
                             for message in turn["status"]["messages"]))

    def test_disabled_and_malformed_commands_make_no_model_requests(self):
        script = []
        for index, text in enumerate(["/goal 不执行的待办", "/goals", "/mind"]):
            script.extend(self.send(self.message(f"off-{index}", text, DISABLED)))
        self.run_eve(script, cognition=False)
        self.assertFalse(self.requests)
        self.assertNotIn("eve.cognition", self.documents())
        script = []
        for index, text in enumerate(["/goal", "/goals extra", "/mind one two"]):
            script.extend(self.send(self.message(f"bad-{index}", text, HELP)))
        script.extend(self.send(self.message("unknown", "/goalx",
            "当前会话没有可控制的任务，请先发送普通文字开始任务。")))
        self.run_eve(script)
        self.assertFalse(self.requests)
        saved = self.documents().get("eve.cognition", {}).get("cognition.v1")
        self.assertTrue(saved is None or not saved["state"]["goals"])

    def test_saved_reflection_ignores_training_and_restarts_without_replay(self):
        description = "准备一份只在本地形成的测试计划"
        summary = self.run_eve(self.create_and_inspect(description), training=True)
        self.assert_one_reflection(description)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(summary["completed"], 0)
        self.assertEqual([event["id"] for event in self.events if event.get("type") == "reply"],
                         ["goal-1", "goal-1-mind"])
        parent = self.parent(description)
        script = [
            {"send": {**self.message("goal-1", "/goal 改写重复消息也不能新增"), "expected_type": "finish"}},
            {"wait_command": {"id": "goal-1", "type": "finish"}},
            *self.send(self.message("restart-mind", "/mind " + parent["id"],
                contains=[ARTIFACT["summary"], "原待办未完成"])),
            *self.send(self.message("restart-goals", "/goals", contains=[parent["id"], "Waiting"])),
        ]
        self.run_eve(script, training=True)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(len(self.goals()), 2)
        self.assert_one_reflection(description)

    def test_foreground_cancel_does_not_cancel_background_reflection(self):
        description = "与前台对话独立整理的待办"
        self.response_gates = {("chat", 1): "front-release", ("reflection", 1): "reflection-release"}
        script = [
            {"send": {**self.message("front", "front-wait"), "expected_type": "finish"}},
            {"wait_file": str(self.arrived("chat"))},
            *self.send(self.message("parallel-goal", "/goal " + description, contains="待办已保存：")),
            {"wait_file": str(self.arrived("reflection"))},
            *self.send(self.message("front-cancel", "/cancel", CANCELLED)),
            self.wait_child(description, "Executing"),
            {"touch": str(self.gate("reflection-release"))},
            self.wait_child(description),
            *self.send(self.message("parallel-mind", "/mind", contains=ARTIFACT["summary"])),
            *self.send(self.message("front-next", "front-after")),
        ]
        self.run_eve(script, training=True)
        self.assert_one_reflection(description)
        chats = [request["body"] for request in self.requests if request["kind"] == "chat"]
        self.assertEqual(len(chats), 2)
        self.assertTrue(all(any("主动提问训练模式" in message.get("content", "")
                               for message in chat["messages"]) for chat in chats))
        self.assertFalse(any(event.get("type") == "reply" and event.get("id") == "front"
                             for event in self.events))

    def test_owner_group_scope_and_app_isolation(self):
        description = "只属于群一用户一的私有待办"
        self.run_eve(self.create_and_inspect(description, scope="group"))
        parent_id = self.parent(description)["id"]
        script = []
        routes = [{"scope": "group", "user": "user-2"},
                  {"scope": "group", "target": "group-2"}, {"scope": "c2c"}]
        for index, route in enumerate(routes):
            script.extend(self.send(self.message(f"scope-list-{index}", "/goals", NO_GOALS, **route)))
            script.extend(self.send(self.message(f"scope-mind-{index}", "/mind " + parent_id, NO_OWNED, **route)))
        self.run_eve(script)
        self.assertFalse(any("PRIVATE_REFLECTION_MARKER" in event.get("text", "") for event in self.events))
        self.run_eve([*self.send(self.message("app-list", "/goals", NO_GOALS, scope="group")),
                      *self.send(self.message("app-mind", "/mind " + parent_id, NO_OWNED, scope="group"))],
                     app="other-app")
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(len(self.goals()), 2)

    def test_invalid_artifact_blocks_and_does_not_retry_after_restart(self):
        description = "不允许伪造完成的待办"
        self.reflection_text = '{"summary":"","next_step":"已完成全部操作","needs_user_input":false}'
        self.run_eve([
            *self.send(self.message("invalid-goal", "/goal " + description, contains="待办已保存：")),
            self.wait_child(description, "Blocked"),
            *self.send(self.message("invalid-mind", "/mind", contains=["Blocked", "原待办仍未完成"])),
        ])
        child = self.child(description)
        self.assertFalse(child["feedback"]["verification_met"])
        self.assertEqual(child["feedback"]["started_tools"], 0)
        self.assertEqual(self.parent(description)["status"], "Waiting")
        self.run_eve([*self.send(self.message("invalid-again", "/mind", contains="Blocked")),
                      *self.send(self.message("invalid-list", "/goals", contains="Waiting"))])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.child(description), child)

    @unittest.skipUnless(os.name == "posix", "SIGTERM cancellation uses the Unix process interface")
    def test_sigterm_settles_inflight_reflection_and_restart_does_not_replay(self):
        description = "停止时尚未完成的反思"
        self.response_gates = {("reflection", 1): "never-released"}
        synchronized = self.gate("cancel-ready")
        self.run_eve([
            *self.send(self.message("cancel-goal", "/goal " + description, contains="待办已保存：")),
            {"wait_file": str(self.arrived("reflection"))},
            self.wait_child(description, "Executing"),
            {"touch": str(synchronized)},
            {"wait_file": str(self.gate("keep-bridge-open"))},
        ], cancel_at=synchronized)
        child = self.child(description)
        self.assertEqual(child["status"], "Cancelled")
        self.assertFalse(child["feedback"]["verification_met"])
        self.assertEqual(child["feedback"]["started_tools"], 0)
        self.assertEqual(self.parent(description)["status"], "Waiting")
        with self.assertRaises(ProcessLookupError):
            os.kill(int((self.work / "child.pid").read_text()), 0)
        self.run_eve([*self.send(self.message("cancel-mind", "/mind", contains=["Cancelled", "原待办仍未完成"])),
                      *self.send(self.message("cancel-list", "/goals", contains="Waiting"))])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.child(description), child)


if __name__ == "__main__":
    unittest.main(verbosity=2)
