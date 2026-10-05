"""实际 eve-qqbot 的交互记忆验收；仅连接本地 HTTP 与替身 QQ 桥接。

所有停止操作都使用本测试创建并持有的 Popen；不读取 PID 文件发送信号。
快照检查通过桥接 touch/wait_file 握手，避免用固定延时猜测提交时序。
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

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINARY = pathlib.Path(os.environ.get("EVE_QQBOT_BINARY", ROOT / "target/debug/eve-qqbot"))
FAKE = ROOT / "connectors/qqbot/test/fake-bridge.mjs"
DISABLED = "交互记忆未启用。"
EMPTY = "当前会话没有偏好。发送 /remember 偏好内容 保存。"
NOT_OWNED = "当前会话没有这条偏好。发送 /memories 查看偏好 ID。"
HELP = "用法：/remember 偏好内容、/memories [页码]、/correct-memory 偏好ID 新内容、/forget 偏好ID。"
MEMORY_KIND = "eve-confirmed-preferences-v1"
PREFERENCE = "PRIVATE_MEMORY_MARKER：回答请先给简短结论。"
CORRECTION = "CORRECTED_MEMORY_MARKER：先给必要证据，再给结论。"
ARTIFACT = {"summary": "LOCAL_REFLECTION_MARKER：待办仍需用户确认。",
            "next_step": "请求用户明确执行范围。", "needs_user_input": True}


class MemoryAcceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_path = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.runs = 0
        self.response_gates = {}
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
                    deadline = time.monotonic() + 15
                    while not outer.gate(gate).exists():
                        if release.wait(0.01):
                            return
                        assert time.monotonic() < deadline, "response gate timed out"
                reply = json.dumps(ARTIFACT, ensure_ascii=False) if kind == "reflection" else latest
                encoded = json.dumps({"choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": reply}}]}).encode()
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

    def memory(self):
        return self.documents().get("eve.memory", {}).get("memory.v1")

    def snapshots(self):
        return [scope["snapshot"] for scope in (self.memory() or {}).get("scopes", [])]

    def only_snapshot(self):
        snapshots = self.snapshots()
        self.assertEqual(len(snapshots), 1)
        return snapshots[0]

    def evidence(self):
        return [evidence for snapshot in self.snapshots() for evidence in snapshot["evidence"]]

    def interactions(self):
        return [item for item in self.evidence() if item["source"]["kind"] == "CompletedInteraction"]

    def receipt(self, id):
        return next(entry for entry in self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"]
                    if entry["message"]["id"] == id)

    def turn(self, text):
        return next(turn for session in self.documents()["eve.session"]["sessions.v1"]["sessions"].values()
                    for turn in session["turns"] if turn["input"] == text)

    def gate(self, name):
        return self.work / f"{name}.gate"

    def arrived(self, kind, number=1):
        return self.work / f"{kind}-{number}.arrived"

    def checkpoint(self, name):
        return [{"touch": str(self.gate(name + "-inspect"))},
                {"wait_file": str(self.gate(name + "-continue"))}]

    def wait_until(self, predicate, message):
        deadline = time.monotonic() + 10
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            self.assertFalse(self.run_release.wait(0.01), message)

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

    def wait_receipt(self, id, state="Sent"):
        return {"wait_receipt": {"path": str(self.state_path), "id": id, "state": state}}

    def send(self, message):
        return [{"send": message}, self.wait_reply(message["id"]), self.wait_receipt(message["id"])]

    def duplicate(self, id, text, **route):
        return [{"send": {**self.message(id, text, **route), "expected_type": "finish"}},
                {"wait_command": {"id": id, "type": "finish"}}]

    def run_eve(self, script, memory=True, cognition=False, training=False, app="1904159860",
                checkpoints=None, send_fail=False, cancel_at=None, success=True):
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
        command = [str(BINARY), "--state-dir", str(self.work / "state"),
            "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE),
            "--bridge-arg", str(scenario)]
        if memory:
            command.append("--memory")
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
            if cancel_at:
                self.wait_until(lambda: cancel_at.exists() or child.poll() is not None,
                                "SIGTERM synchronization was not reached")
                self.assertTrue(cancel_at.exists(), "process exited before SIGTERM synchronization")
                child.send_signal(signal.SIGTERM)
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

    def remember(self, **route):
        self.run_eve(self.send(self.message("remember", "/remember " + PREFERENCE,
                                            contains="偏好已保存：", **route)))
        snapshot = self.only_snapshot()
        self.assertEqual(len(snapshot["preferences"]), 1)
        return snapshot["preferences"][0]["id"]

    def preference_data(self, request):
        data = []
        for message in request["body"]["messages"]:
            content = message.get("content", "") or ""
            if MEMORY_KIND in content:
                self.assertEqual(message["role"], "system")
                data.append(json.loads(content[content.index("{") :]))
        return data

    def test_disabled_and_invalid_commands_never_call_model(self):
        script = []
        for index, command in enumerate(["/remember 保存内容", "/memories", "/correct-memory id 修改", "/forget id"]):
            script.extend(self.send(self.message(f"off-{index}", command, DISABLED)))
        self.run_eve(script, memory=False)
        self.assertFalse(self.requests)
        self.assertNotIn("eve.memory", self.documents())
        script = []
        for index, command in enumerate(["/remember", "/memories 0", "/memories -1",
                                         "/correct-memory id", "/forget id extra"]):
            script.extend(self.send(self.message(f"invalid-{index}", command, HELP)))
        script.extend(self.send(self.message("empty", "/memories", EMPTY)))
        self.run_eve(script)
        self.assertFalse(self.requests)
        self.assertFalse(self.snapshots())

    def test_statement_and_preference_commit_together_duplicate_and_restart_do_not_write(self):
        command = "/remember " + PREFERENCE

        def assert_atomic():
            snapshot = self.only_snapshot()
            self.assertEqual(snapshot["revision"], 1)
            self.assertEqual(len(snapshot["evidence"]), 1)
            self.assertEqual(len(snapshot["preferences"]), 1)
            evidence = snapshot["evidence"][0]
            preference = snapshot["preferences"][0]
            self.assertEqual(evidence["source"], {"kind": "UserStatement", "message_id": "remember", "text": command})
            self.assertEqual((preference["text"], preference["status"], preference["revision"]),
                             (PREFERENCE, "Confirmed", 1))
            self.assertEqual(preference["history"][0]["evidence_id"], evidence["id"])
            self.assertEqual(self.receipt("remember")["state"], "ReplyPending")

        message = {**self.message("remember", command, contains="偏好已保存："), "hold_delivery": True}
        self.run_eve([{"send": message}, self.wait_reply("remember"),
                      *self.checkpoint("atomic"), {"delivery": "remember"}, self.wait_receipt("remember")],
                     checkpoints={"atomic": assert_atomic})
        before = self.memory()
        id = self.only_snapshot()["preferences"][0]["id"]
        self.run_eve([*self.duplicate("remember", "/remember 不得覆盖或产生新偏好"),
                      *self.send(self.message("list", "/memories", contains=[id, "已确认", PREFERENCE]))])
        self.assertEqual(self.memory(), before)
        self.assertFalse(self.requests)

    def test_confirm_correct_revoke_change_next_context_without_extra_model_calls(self):
        id = self.remember()
        self.run_eve(self.send(self.message("chat-first", "第一轮正常对话")), training=True)
        self.run_eve([*self.send(self.message("correct", f"/correct-memory {id} {CORRECTION}", contains="偏好已修正：")),
                      *self.send(self.message("chat-corrected", "第二轮正常对话"))])
        self.run_eve([*self.send(self.message("forget", "/forget " + id, contains="偏好已撤销：")),
                      *self.send(self.message("chat-revoked", "第三轮正常对话")),
                      *self.send(self.message("list-revoked", "/memories", contains=[id, "已撤销", CORRECTION]))])
        self.assertEqual(len(self.requests), 3)
        first, corrected, revoked = [self.preference_data(request) for request in self.requests]
        self.assertEqual([item["text"] for item in first[0]["preferences"]], [PREFERENCE])
        self.assertEqual([item["text"] for item in corrected[0]["preferences"]], [CORRECTION])
        self.assertEqual(revoked, [])
        self.assertTrue(any("主动提问训练模式" in (message.get("content") or "")
                            for message in self.requests[0]["body"]["messages"]))
        preference = self.only_snapshot()["preferences"][0]
        self.assertEqual([version["status"] for version in preference["history"]],
                         ["Confirmed", "Confirmed", "Revoked"])
        self.assertEqual([version["text"] for version in preference["history"]],
                         [PREFERENCE, CORRECTION, CORRECTION])
        self.assertEqual(len(self.interactions()), 3)
        self.assertEqual(len(self.evidence()), 6)

    def test_private_user_group_and_application_boundaries(self):
        id = self.remember(scope="group")
        before = self.memory()
        routes = [{"scope": "group", "user": "user-2"},
                  {"scope": "group", "target": "group-2"}, {"scope": "c2c"}]
        script = []
        for index, route in enumerate(routes):
            script.extend(self.send(self.message(f"scope-list-{index}", "/memories", EMPTY, **route)))
            script.extend(self.send(self.message(f"scope-correct-{index}", f"/correct-memory {id} 越界修改", NOT_OWNED, **route)))
            script.extend(self.send(self.message(f"scope-forget-{index}", "/forget " + id, NOT_OWNED, **route)))
        self.run_eve(script)
        self.assertEqual(self.memory(), before)
        script = []
        for index, route in enumerate(routes):
            script.extend(self.send(self.message(f"scope-chat-{index}", f"隔离作用域对话 {index}", **route)))
        self.run_eve(script)
        self.assertEqual(len(self.requests), 3)
        self.assertTrue(all(not self.preference_data(request) for request in self.requests))
        self.run_eve([*self.send(self.message("app-list", "/memories", EMPTY, scope="group")),
                      *self.send(self.message("app-forget", "/forget " + id, NOT_OWNED, scope="group")),
                      *self.send(self.message("app-chat", "另一个应用的对话", scope="group"))], app="other-app")
        self.assertEqual(len(self.requests), 4)
        self.assertEqual(self.preference_data(self.requests[-1]), [])
        self.assertFalse(any(PREFERENCE in event.get("text", "") for event in self.events))
        self.run_eve(self.send(self.message("owner-list", "/memories", contains=[id, "已确认", PREFERENCE], scope="group")))

    def test_only_completed_and_sent_chat_becomes_interaction_and_replay_is_zero_write(self):
        text = " 只有完成并确认投递的这条交互才保存。\n保留原始正文和空白。 "
        self.response_gates = {("chat", 1): "complete-chat"}

        def assert_running():
            self.assertEqual(self.turn(text)["status"]["state"], "Pending")
            self.assertEqual(self.receipt("delivered")["state"], "Processing")
            self.assertEqual(self.interactions(), [])

        def assert_pending():
            self.assertEqual(self.turn(text)["status"]["state"], "Completed")
            self.assertEqual(self.receipt("delivered")["state"], "ReplyPending")
            self.assertEqual(self.interactions(), [])

        def assert_delivered():
            self.wait_until(lambda: len(self.interactions()) == 1, "Sent interaction was not imported")
            self.assertEqual(self.receipt("delivered")["state"], "Sent")
            source = self.interactions()[0]["source"]
            self.assertEqual(source["message_id"], "delivered")
            self.assertEqual(source["user_text"], text)
            self.assertEqual(source["assistant_text"], text)
            self.assertEqual(source["turn_id"], self.turn(text)["id"])
            self.assertEqual(self.only_snapshot()["preferences"], [])

        message = {**self.message("delivered", text), "hold_delivery": True}
        self.run_eve([{"send": message}, {"wait_file": str(self.arrived("chat"))},
                      *self.checkpoint("running"), {"touch": str(self.gate("complete-chat"))},
                      self.wait_reply("delivered"), *self.checkpoint("pending"),
                      {"delivery": "delivered"}, self.wait_receipt("delivered"), *self.checkpoint("sent")],
                     checkpoints={"running": assert_running, "pending": assert_pending, "sent": assert_delivered})
        before = self.memory()
        self.run_eve(self.duplicate("delivered", "重复消息不得重放或重复观察"))
        self.assertEqual(self.memory(), before)
        self.assertEqual(len(self.requests), 1)

    def test_failed_delivery_and_unacknowledged_reply_are_not_imported_on_restart(self):
        self.run_eve([{"send": self.message("failed", "完成但投递失败")}, self.wait_reply("failed"),
                      self.wait_receipt("failed", "Failed")], send_fail=True)
        self.assertEqual(self.turn("完成但投递失败")["status"]["state"], "Completed")
        self.assertEqual(self.interactions(), [])
        pending = {**self.message("pending", "完成但没有投递确认"), "hold_delivery": True}
        self.run_eve([{"send": pending}, self.wait_reply("pending"),
                      self.wait_receipt("pending", "ReplyPending")], success=False)
        self.assertEqual(self.turn("完成但没有投递确认")["status"]["state"], "Completed")
        self.assertEqual(self.interactions(), [])
        self.run_eve([*self.duplicate("failed", "不补发失败回复"),
                      *self.duplicate("pending", "不补发未确认回复")])
        self.assertEqual(self.interactions(), [])
        self.assertEqual(len(self.requests), 2)

    def test_internal_reflection_receives_no_memory_context_and_creates_no_chat_evidence(self):
        self.remember()
        before = self.memory()
        description = "准备只在本地形成的反思草稿"
        self.run_eve([*self.send(self.message("goal", "/goal " + description, contains="待办已保存：")),
                      {"wait_cognition": {"path": str(self.state_path), "parent_description": description,
                                          "child_state": "Completed"}},
                      *self.send(self.message("mind", "/mind", contains=[ARTIFACT["summary"], "原待办未完成"]))],
                     cognition=True, training=True)
        self.assertEqual(len(self.requests), 1)
        request = self.requests[0]
        self.assertEqual(request["kind"], "reflection")
        self.assertEqual(self.preference_data(request), [])
        self.assertFalse(request["body"].get("tools"))
        encoded = json.dumps(request["body"], ensure_ascii=False)
        self.assertNotIn(PREFERENCE, encoded)
        self.assertNotIn("主动提问训练模式", encoded)
        self.assertEqual(self.memory(), before)

    @unittest.skipUnless(os.name == "posix", "SIGTERM uses the Unix child process interface")
    def test_cancelled_model_has_no_interaction_or_restart_import(self):
        self.response_gates = {("chat", 1): "never-complete"}
        stop_at = self.gate("cancel-ready")
        text = "取消前不能成为已完成交互"
        self.run_eve([{"send": self.message("cancelled", text)},
                      {"wait_file": str(self.arrived("chat"))},
                      {"touch": str(stop_at)}, {"wait_file": str(self.gate("keep-open"))}], cancel_at=stop_at)
        self.assertEqual(self.turn(text)["status"]["state"], "Failed")
        self.assertEqual(self.interactions(), [])
        self.run_eve(self.duplicate("cancelled", "取消后不补学"))
        self.assertEqual(self.interactions(), [])
        self.assertEqual(len(self.requests), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
