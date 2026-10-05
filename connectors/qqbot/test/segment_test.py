"""实际 eve-qqbot 的分段投递验收；仅连接本地 HTTP 与替身 QQ 桥接。

替身模型原样回复用户正文，因此多段输入会得到同样分段的完整回复。
停止只针对本测试创建的 Popen；时序通过桥接命令与回执握手，不靠固定延时。
"""
import hashlib
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
PARTS = ["好的主人，结论是可以分段发送。",
         "完整回复仍然只保存一次，分段只决定在哪里断开，以及每一段之前停顿多久，不会改写内容。",
         "需要我再演示一次吗？"]
TEXT = "\n\n".join(PARTS)
CANCELLED = "已取消当前任务；已完成的工具操作不会撤销。"
TRAINING_STOPPED = "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。"
SEGMENT_OFF = "已关闭本会话分段：下一条回复起整条发送；发送 /segment on 可重新开启。"
SEGMENT_RESET = "已恢复本会话默认分段：开启，最多 3 段，段间停顿 100%，从下一条回复生效。"
SEGMENT_DISABLED = "分段投递未开启：通道启动时未加 --segmented，回复始终整条发送。"


class SegmentAcceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_path = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.events = []
        self.runs = 0
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    assert self.path == "/v1/chat/completions"
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    outer.requests.append(body)
                    reply = body["messages"][-1]["content"]
                    encoded = json.dumps({"choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": reply}}]}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
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

    def ledger(self):
        return self.documents()["eve.channel.qqbot"]["receipts.v1"]

    def receipt(self, id):
        return next(entry for entry in self.ledger()["entries"] if entry["message"]["id"] == id)

    def part_states(self, id):
        return [part["state"] for part in self.receipt(id)["segments"]["parts"]]

    def interactions(self):
        memory = self.documents().get("eve.memory", {}).get("memory.v1") or {}
        return [item for scope in memory.get("scopes", []) for item in scope["snapshot"]["evidence"]
                if item["source"]["kind"] == "CompletedInteraction"]

    def message(self, id, text=TEXT, segments=PARTS, **extra):
        message = {"id": id, "text": text, "scope": "c2c", "user_id": "user-1", "target_id": "user-1",
                   "expected_type": "reply", **extra}
        if segments is None:
            message.setdefault("expected", text)
        else:
            message["expected_segments"] = segments
        return message

    def wait_segment(self, id, count):
        return {"wait_command": {"id": id, "type": "segment", "count": count}}

    def wait_receipt(self, id, state):
        return {"wait_receipt": {"path": str(self.state_path), "id": id, "state": state}}

    def run_eve(self, script, segmented=True, memory=False, training=False, terminate_at=None, success=True):
        self.runs += 1
        events_path = self.work / f"events-{self.runs}.jsonl"
        error_path = self.work / f"error-{self.runs}.txt"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events_path),
                                        "error_file": str(error_path)}), encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        command = [str(BINARY), "--state-dir", str(self.work / "state"), "--agent", str(ROOT / "AGENT.md"),
                   "--bridge-script", str(FAKE), "--bridge-arg", str(scenario)]
        if segmented:
            command.append("--segmented")
        if memory:
            command.append("--memory")
        if training:
            command.append("--training")
        child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            if terminate_at:
                deadline = time.monotonic() + 15
                while not terminate_at.exists() and child.poll() is None:
                    self.assertLess(time.monotonic(), deadline, "SIGTERM synchronization was not reached")
                    time.sleep(0.01)
                self.assertTrue(terminate_at.exists(), "process exited before SIGTERM synchronization")
                child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=30)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
        self.events = ([json.loads(line) for line in events_path.read_text().splitlines()]
                       if events_path.exists() else [])
        self.assertFalse(error_path.exists(), error_path.read_text() if error_path.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ["test-model-secret", "test-app-secret"]:
            self.assertNotIn(secret, stdout + stderr)
        if not success:
            self.assertNotEqual(child.returncode, 0)
            return stderr
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])
        return summary

    def sent(self, kind):
        return [event for event in self.events if event["direction"] == "out" and event["type"] == kind]

    def test_disabled_by_default_sends_one_reply_and_keeps_format_one(self):
        message = self.message("whole", segments=None)
        summary = self.run_eve([{"send": message}, self.wait_receipt("whole", "Sent")], segmented=False)
        self.assertEqual((summary["completed"], summary["sent"]), (1, 1))
        self.assertEqual([event["text"] for event in self.sent("reply")], [TEXT])
        self.assertFalse(self.sent("segment"))
        self.assertEqual(self.ledger()["version"], 1)
        self.assertNotIn("segments", self.receipt("whole"))

    def test_model_reply_is_delivered_in_order_with_pauses_and_memory_keeps_full_reply(self):
        summary = self.run_eve([{"send": self.message("seg")}, self.wait_receipt("seg", "Sent")], memory=True)
        self.assertEqual((summary["received"], summary["completed"], summary["sent"], summary["failed"]),
                         (1, 1, 1, 0))
        segments = self.sent("segment")
        self.assertEqual([(event["index"], event["count"], event["text"]) for event in segments],
                         [(index, 3, part) for index, part in enumerate(PARTS)])
        self.assertFalse(self.sent("reply"))
        # 停顿从上一段回执开始计时：400 ms 基础值加每字 25 ms，上限 2.5 秒。
        for index in (1, 2):
            pause = min(2500, 400 + 25 * len(PARTS[index]))
            self.assertGreaterEqual(segments[index]["at"] - segments[index - 1]["at"], pause - 50)
        ledger = self.ledger()
        receipt = self.receipt("seg")
        self.assertEqual((ledger["version"], receipt["state"], receipt["reply"]), (2, "Sent", TEXT))
        self.assertEqual(receipt["segments"]["planner"], "paragraph-v3")
        reply = receipt["reply"].encode()
        self.assertEqual([reply[p["start"]:p["end"]].decode() for p in receipt["segments"]["parts"]], PARTS)
        self.assertEqual(self.part_states("seg"), ["Sent"] * 3)
        turns = [turn for session in self.documents()["eve.session"]["sessions.v1"]["sessions"].values()
                 for turn in session["turns"]]
        self.assertEqual(len(turns), 1)
        evidence = self.interactions()
        self.assertEqual(len(evidence), 1)
        self.assertEqual(evidence[0]["source"]["assistant_text"], TEXT)
        self.assertEqual(len(self.requests), 1)

    def test_short_replies_and_commands_stay_whole(self):
        short = self.message("short", text="好的，收到。", segments=None)
        command = self.message("cmd", text="/cancel", segments=None, expected=CANCELLED)
        self.run_eve([{"send": short}, self.wait_receipt("short", "Sent"),
                      {"send": command}, self.wait_receipt("cmd", "Sent")])
        self.assertEqual([event["text"] for event in self.sent("reply")], ["好的，收到。", CANCELLED])
        self.assertFalse(self.sent("segment"))
        self.assertNotIn("segments", self.receipt("short"))
        self.assertEqual(self.ledger()["version"], 1)

    def test_short_natural_paragraphs_are_sent_separately_and_remembered_once(self):
        parts = ["好。", "你先说，我听着。"]
        text = "\n\n".join(parts)
        message = self.message("short-parts", text=text, segments=parts)
        summary = self.run_eve([{"send": message}, self.wait_receipt("short-parts", "Sent")], memory=True)
        self.assertEqual((summary["completed"], summary["sent"], summary["failed"]), (1, 1, 0))
        self.assertEqual([(event["index"], event["count"], event["text"])
                          for event in self.sent("segment")],
                         [(index, 2, part) for index, part in enumerate(parts)])
        self.assertFalse(self.sent("reply"))
        self.assertEqual(self.part_states("short-parts"), ["Sent", "Sent"])
        self.assertEqual(self.receipt("short-parts")["reply"], text)
        evidence = self.interactions()
        self.assertEqual(len(evidence), 1)
        self.assertEqual(evidence[0]["source"]["assistant_text"], text)
        self.assertEqual(len(self.requests), 1)

    def test_cancel_between_parts_stops_remaining_and_restart_does_not_resend(self):
        held = self.message("seg", hold_segments=[0], allow_finish=True)
        cancel = self.message("stop", text="/cancel", segments=None, expected=CANCELLED)
        self.run_eve([{"send": held}, self.wait_segment("seg", 1), {"send": cancel},
                      {"deliver_segment": {"id": "seg", "index": 0}},
                      {"wait_command": {"id": "seg", "type": "finish"}}, self.wait_receipt("stop", "Sent")])
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS[:1])
        self.assertEqual(self.receipt("seg")["state"], "Failed")
        self.assertEqual(self.part_states("seg"), ["Sent", "Skipped", "Skipped"])
        before = self.state_path.read_bytes()
        duplicate = {**self.message("seg"), "expected_type": "finish"}
        self.run_eve([{"send": duplicate}, {"wait_command": {"id": "seg", "type": "finish"}}])
        self.assertFalse(self.sent("segment"))
        self.assertEqual(self.state_path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)

    def test_correction_between_parts_retires_old_parts_and_answers_new_generation(self):
        held = self.message("seg", hold_segments=[0], allow_finish=True)
        correct = self.message("fix", text="/correct 只要结论", segments=None, expected_contains=["只要结论"])
        del correct["expected"]
        self.run_eve([{"send": held}, self.wait_segment("seg", 1), {"send": correct},
                      {"deliver_segment": {"id": "seg", "index": 0}},
                      {"wait_command": {"id": "seg", "type": "finish"}}, self.wait_receipt("fix", "Sent")])
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS[:1])
        self.assertEqual(self.part_states("seg"), ["Sent", "Skipped", "Skipped"])
        self.assertEqual(len(self.requests), 2)

    def test_training_toggle_between_parts_closes_remaining_parts(self):
        held = self.message("seg", hold_segments=[0], allow_finish=True)
        stop = self.message("train", text="/train stop", segments=None, expected=TRAINING_STOPPED)
        self.run_eve([{"send": held}, self.wait_segment("seg", 1), {"send": stop},
                      {"deliver_segment": {"id": "seg", "index": 0}},
                      {"wait_command": {"id": "seg", "type": "finish"}}, self.wait_receipt("train", "Sent")],
                     training=True)
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS[:1])
        self.assertEqual(self.receipt("seg")["state"], "Failed")
        self.assertEqual(self.part_states("seg"), ["Sent", "Skipped", "Skipped"])
        self.assertEqual(len(self.requests), 1)

    def test_stop_during_pause_keeps_unsent_parts_and_restart_never_resends(self):
        marker = self.work / "paused"
        self.run_eve([{"send": self.message("seg")},
                      {"wait_part": {"path": str(self.state_path), "id": "seg", "index": 0, "state": "Sent"}},
                      {"touch": str(marker)}, {"wait_file": str(self.work / "never")}], terminate_at=marker)
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS[:1])
        self.assertEqual(self.receipt("seg")["state"], "ReplyPending")
        self.assertEqual(self.part_states("seg"), ["Sent", "Pending", "Pending"])
        before = self.state_path.read_bytes()
        duplicate = {**self.message("seg"), "expected_type": "finish"}
        self.run_eve([{"send": duplicate}, {"wait_command": {"id": "seg", "type": "finish"}}])
        self.assertFalse(self.sent("segment"))
        self.assertEqual(self.state_path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)

    def test_part_failure_is_not_retried_or_remembered(self):
        failing = self.message("seg", fail_segment=1)
        summary = self.run_eve([{"send": failing}, self.wait_receipt("seg", "Failed")], memory=True)
        self.assertEqual((summary["sent"], summary["failed"]), (0, 1))
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS[:2])
        self.assertEqual(self.part_states("seg"), ["Sent", "Failed", "Skipped"])
        self.assertFalse(self.interactions())

    def test_stop_while_part_is_in_flight_keeps_uncertain_receipt_without_resend(self):
        marker = self.work / "in-flight"
        held = self.message("seg", hold_segments=[1])
        self.run_eve([{"send": held}, self.wait_segment("seg", 2), {"touch": str(marker)},
                      {"wait_file": str(self.work / "never")}], terminate_at=marker)
        self.assertEqual(self.receipt("seg")["state"], "ReplyPending")
        self.assertEqual(self.part_states("seg"), ["Sent", "Sending", "Pending"])
        before = self.state_path.read_bytes()
        duplicate = {**self.message("seg"), "expected_type": "finish"}
        self.run_eve([{"send": duplicate}, {"wait_command": {"id": "seg", "type": "finish"}}])
        self.assertFalse(self.sent("segment"))
        self.assertEqual(self.state_path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)

    def test_inconsistent_part_progress_is_rejected_without_clearing(self):
        self.run_eve([{"send": self.message("seg")}, self.wait_receipt("seg", "Sent")])
        state = json.loads(self.state_path.read_text())
        ledger = self.ledger()
        ledger["entries"][0]["segments"]["parts"][1]["state"] = "Pending"
        state["entries"]["eve.channel.qqbot"]["receipts.v1"] = list(json.dumps(ledger).encode())
        self.state_path.write_text(json.dumps(state))
        before = self.state_path.read_bytes()
        stderr = self.run_eve([{"send": self.message("other", segments=None)}], success=False)
        self.assertIn("回执状态损坏或版本不兼容", stderr)
        self.assertEqual(self.state_path.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)


    def preferences(self):
        return self.documents().get("eve.segment.preferences", {}).get("preferences.v1")

    def command(self, id, text, expected, user="user-1"):
        return self.message(id, text=text, segments=None, expected=expected, user_id=user, target_id=user)

    def test_session_can_disable_segments_and_setting_survives_restart(self):
        self.run_eve([{"send": self.command("off", "/segment off", SEGMENT_OFF)}, self.wait_receipt("off", "Sent"),
                      {"send": self.message("whole-1", segments=None)}, self.wait_receipt("whole-1", "Sent")])
        self.assertEqual([event["text"] for event in self.sent("reply")], [SEGMENT_OFF, TEXT])
        self.assertFalse(self.sent("segment"))
        self.assertNotIn("segments", self.receipt("whole-1"))
        self.assertEqual(self.preferences(), {"version": 1, "scopes": [
            {"scope": {"channel": "qq", "session_id": self.session_id(), "user_id": self.session_id()},
             "enabled": False, "max_segments": None, "pause_percent": None}]})
        self.run_eve([{"send": self.message("whole-2", segments=None)}, self.wait_receipt("whole-2", "Sent"),
                      {"send": self.command("reset", "/segment reset", SEGMENT_RESET)}, self.wait_receipt("reset", "Sent"),
                      {"send": self.message("seg")}, self.wait_receipt("seg", "Sent")])
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS)
        self.assertEqual(self.preferences(), {"version": 1, "scopes": []})
        self.assertEqual(len(self.requests), 3)

    def test_parts_and_pace_narrow_only_this_session(self):
        user2 = {**self.message("other"), "user_id": "user-2", "target_id": "user-2"}
        self.run_eve([{"send": self.command("parts", "/segment parts 2", "已将本会话分段上限设为 2 段，从下一条回复生效。")},
                      self.wait_receipt("parts", "Sent"),
                      {"send": self.command("pace", "/segment pace 0", "已将本会话段间停顿设为 0%（单次不超过 0 秒），从下一条回复生效。")},
                      self.wait_receipt("pace", "Sent"),
                      {"send": self.message("narrow", segments=[PARTS[0], PARTS[1] + "\n\n" + PARTS[2]])},
                      self.wait_receipt("narrow", "Sent"),
                      {"send": user2}, self.wait_receipt("other", "Sent")])
        narrow = [event for event in self.sent("segment") if event["id"] == "narrow"]
        self.assertLess(narrow[1]["at"] - narrow[0]["at"], 700)
        other = [event for event in self.sent("segment") if event["id"] == "other"]
        self.assertEqual([event["text"] for event in other], PARTS)
        self.assertGreaterEqual(other[1]["at"] - other[0]["at"], min(2500, 400 + 25 * len(PARTS[1])) - 50)

    def test_setting_during_pause_applies_from_next_reply(self):
        held = self.message("seg", hold_segments=[0])
        self.run_eve([{"send": held}, self.wait_segment("seg", 1),
                      {"send": self.command("off", "/segment off", SEGMENT_OFF)},
                      {"deliver_segment": {"id": "seg", "index": 0}},
                      self.wait_receipt("seg", "Sent"), self.wait_receipt("off", "Sent"),
                      {"send": self.message("next", segments=None)}, self.wait_receipt("next", "Sent")])
        self.assertEqual([event["text"] for event in self.sent("segment")], PARTS)
        out = [event for event in self.events if event["direction"] == "out" and event["type"] in ("segment", "reply")]
        self.assertEqual([(event["id"], event["type"]) for event in out],
                         [("seg", "segment")] * 3 + [("off", "reply"), ("next", "reply")])

    def test_rejected_values_and_disabled_flag_never_write_or_call_the_model(self):
        self.run_eve([{"send": self.command("p6", "/segment parts 6", "段数须为 2 至 5 的整数；需要整条发送请用 /segment off。设置未改变。")},
                      self.wait_receipt("p6", "Sent"),
                      {"send": self.command("p300", "/segment pace 300", "停顿比例须为 0 至 200 的整数（默认 100，0 为不停顿）。设置未改变。")},
                      self.wait_receipt("p300", "Sent")])
        self.assertIsNone(self.preferences())
        self.run_eve([{"send": self.command("disabled", "/segment off", SEGMENT_DISABLED)},
                      self.wait_receipt("disabled", "Sent"),
                      {"send": self.message("whole", segments=None)}, self.wait_receipt("whole", "Sent")],
                     segmented=False)
        self.assertIsNone(self.preferences())
        self.assertEqual(len(self.requests), 1)

    def test_corrupt_segment_settings_refuse_startup_without_clearing(self):
        self.run_eve([{"send": self.command("off", "/segment off", SEGMENT_OFF)}, self.wait_receipt("off", "Sent")])
        state = json.loads(self.state_path.read_text())
        state["entries"]["eve.segment.preferences"]["preferences.v1"] = list(b'{"version":9,"scopes":[]}')
        self.state_path.write_text(json.dumps(state))
        before = self.state_path.read_bytes()
        stderr = self.run_eve([{"send": self.message("seg")}], success=False)
        self.assertIn("分段设置状态版本不兼容；未清空", stderr)
        self.assertEqual(self.state_path.read_bytes(), before)
        self.assertEqual(len(self.requests), 0)

    def session_id(self):
        routing = json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()
        return "qq:" + hashlib.sha256(routing).hexdigest()

    def remember_rhythm(self, text):
        self.run_eve([{"send": self.message("rhythm-source", text="/remember " + text,
                                            segments=None, expected_contains="偏好已保存：")},
                      self.wait_receipt("rhythm-source", "Sent")], memory=True)
        snapshot = self.documents()["eve.memory"]["memory.v1"]["scopes"][0]["snapshot"]
        return snapshot["preferences"][0]["id"]

    def advice_command(self, id, text, contains, **route):
        return self.message(id, text=text, segments=None, expected_contains=contains, **route)

    def test_advice_is_read_only_until_adopted_and_delivery_survives_restart(self):
        source = self.remember_rhythm("回复最多分成两段，段间不要停顿。")
        before = self.documents()["eve.memory"]["memory.v1"]
        self.run_eve([{"send": self.advice_command("suggest", "/segment suggestions",
                                                   [source, "第 1 版", "最多 2 段", "停顿 0%", "查看不改变设置"])},
                      self.wait_receipt("suggest", "Sent")], memory=True)
        self.assertIsNone(self.preferences())
        self.assertEqual(self.documents()["eve.memory"]["memory.v1"], before)
        self.assertFalse(self.requests)
        adopt = f"/segment adopt {source} 1"
        parts = [PARTS[0], PARTS[1] + "\n\n" + PARTS[2]]
        self.run_eve([{"send": self.advice_command("adopt", adopt, "已采用偏好")},
                      self.wait_receipt("adopt", "Sent"),
                      {"send": self.message("adopted-chat", segments=parts)},
                      self.wait_receipt("adopted-chat", "Sent")], memory=True)
        setting = self.preferences()
        self.assertEqual(setting["scopes"][0]["max_segments"], 2)
        self.assertEqual(setting["scopes"][0]["pause_percent"], 0)
        self.assertIsNone(setting["scopes"][0]["enabled"])
        self.assertEqual(self.receipt("adopt")["message"]["text"], adopt)
        self.assertEqual(self.receipt("adopted-chat")["reply"], TEXT)
        self.assertEqual(len(self.interactions()), 1)
        frozen = self.state_path.read_bytes()
        duplicate = self.advice_command("adopt", adopt, "不得重复采用", expected_type="finish")
        self.run_eve([{"send": duplicate}, {"wait_command": {"id": "adopt", "type": "finish"}}], memory=True)
        self.assertEqual(self.state_path.read_bytes(), frozen)
        self.run_eve([{"send": self.message("after-restart", segments=parts)},
                      self.wait_receipt("after-restart", "Sent"),
                      {"send": self.message("other-default", user_id="other", target_id="other")},
                      self.wait_receipt("other-default", "Sent")], memory=True)
        self.assertEqual(self.preferences(), setting)
        self.assertEqual(len(self.requests), 3)

    def test_correction_rejects_old_revision_and_revoke_does_not_reapply(self):
        source = self.remember_rhythm("回复最多两段，段间不要停顿。")
        corrected = f"/correct-memory {source} 回复最多三段，段间停顿50%。"
        self.run_eve([{"send": self.advice_command("correct-rhythm", corrected, "偏好已修正：")},
                      self.wait_receipt("correct-rhythm", "Sent"),
                      {"send": self.advice_command("stale", f"/segment adopt {source} 1", "版本已变化")},
                      self.wait_receipt("stale", "Sent"),
                      {"send": self.advice_command("fresh-list", "/segment suggestions", ["第 2 版", "最多 3 段", "停顿 50%", source])},
                      self.wait_receipt("fresh-list", "Sent")], memory=True)
        self.assertIsNone(self.preferences())
        self.run_eve([{"send": self.advice_command("fresh-adopt", f"/segment adopt {source} 2", "已采用偏好")},
                      self.wait_receipt("fresh-adopt", "Sent")], memory=True)
        saved = self.preferences()
        self.run_eve([{"send": self.advice_command("forget-rhythm", "/forget " + source, "偏好已撤销：")},
                      self.wait_receipt("forget-rhythm", "Sent"),
                      {"send": self.advice_command("no-revival", f"/segment adopt {source} 2", "已撤销的偏好不能采用")},
                      self.wait_receipt("no-revival", "Sent"),
                      {"send": self.advice_command("revoked-list", "/segment suggestions", "没有可采用的节奏建议")},
                      self.wait_receipt("revoked-list", "Sent")], memory=True)
        self.assertEqual(self.preferences(), saved)
        self.run_eve([{"send": self.command("reset-rhythm", "/segment reset", SEGMENT_RESET)},
                      self.wait_receipt("reset-rhythm", "Sent")], memory=True)
        self.assertEqual(self.preferences()["scopes"], [])
        frozen = self.state_path.read_bytes()
        duplicate = self.advice_command("fresh-adopt", f"/segment adopt {source} 2", "不得复活", expected_type="finish")
        self.run_eve([{"send": duplicate}, {"wait_command": {"id": "fresh-adopt", "type": "finish"}}], memory=True)
        self.assertEqual(self.state_path.read_bytes(), frozen)
        self.assertFalse(self.requests)

    def test_foreign_sources_disabled_memory_and_unsupported_feedback_do_not_write(self):
        self.run_eve([{"send": self.advice_command("no-memory", "/segment suggestions", "需要同时启用")},
                      self.wait_receipt("no-memory", "Sent")])
        source = self.remember_rhythm("回复最多两段。")
        other = self.advice_command("foreign", f"/segment adopt {source} 1", "当前会话没有这条已确认偏好",
                                    user_id="other", target_id="other")
        self.run_eve([{"send": other}, self.wait_receipt("foreign", "Sent"),
                      {"send": self.advice_command("vague", f"/correct-memory {source} 说话自然一点。", "偏好已修正：")},
                      self.wait_receipt("vague", "Sent"),
                      {"send": self.advice_command("vague-adopt", f"/segment adopt {source} 2", "没有可采用的明确节奏建议")},
                      self.wait_receipt("vague-adopt", "Sent")], memory=True)
        self.assertIsNone(self.preferences())
        self.assertFalse(self.requests)

    def test_capacity_rejection_never_claims_adoption_or_changes_sources(self):
        source = self.remember_rhythm("回复最多两段，段间不要停顿。")
        state = json.loads(self.state_path.read_text())
        full = {"version": 1, "scopes": [{"scope": {"channel": "qq", "session_id": f"filled-{i}", "user_id": "fixture"},
                 "enabled": False, "max_segments": None, "pause_percent": None} for i in range(1024)]}
        state["entries"]["eve.segment.preferences"] = {"preferences.v1": list(json.dumps(full).encode())}
        self.state_path.write_text(json.dumps(state))
        memory = self.documents()["eve.memory"]["memory.v1"]
        self.run_eve([{"send": self.advice_command("full-adopt", f"/segment adopt {source} 1", "容量已满")},
                      self.wait_receipt("full-adopt", "Sent")], memory=True)
        self.assertNotIn("已采用偏好", self.receipt("full-adopt")["reply"])
        self.assertEqual(self.preferences(), full)
        self.assertEqual(self.documents()["eve.memory"]["memory.v1"], memory)
        self.assertFalse(self.requests)


if __name__ == "__main__":
    unittest.main()
