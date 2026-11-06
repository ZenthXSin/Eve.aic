"""真实 Rust 宿主与生产 Node 桥接验收；模型 HTTP 和腾讯 SDK 使用本地替身。"""
from contextlib import closing
from fixture_auth import configure_fixture_progress
from fixture_auth import bootstrap_fixture_group_ids
import json
import os
import pathlib
import signal
import sqlite3
import subprocess
import unittest

import memory_test as memory


class TransportAcceptance(memory.MemoryAcceptance):
    for _name in dir(memory.MemoryAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.children = []

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
        super().tearDown()

    def launch(self, **options):
        number = len(self.children)
        scenario = {name: str(self.work / f"{number}-{name}")
                    for name in ("sending", "stopped", "finished")}
        scenario.update(options)
        path = self.work / f"{number}-scenario.json"
        path.write_text(json.dumps(scenario))
        bootstrap_fixture_group_ids(self.work / "state", [message["groupOpenid"]
            for message in scenario.get("incoming", []) if message.get("kind") == "group"])
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(EVE_QQ_LIVE_INTERACTION="false", QQBOT_APP_ID="1904159860", QQBOT_APP_SECRET="synthetic-app-secret",
                   EVE_OPENAI_API_KEY="synthetic-model-secret", EVE_OPENAI_PROTOCOL="chat",
                   EVE_LLM_RESPONSE_MODE="complete",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}")
        stdout = self.work / f"{number}-stdout"
        stderr = self.work / f"{number}-stderr"
        with stdout.open("w") as out, stderr.open("w") as err:
            child = subprocess.Popen([str(memory.BINARY), "--minimal-profile", "--memory", "--state-dir", str(self.work / "state"),
                "--agent", str(memory.ROOT / "AGENT.md"), "--bridge-script",
                str(pathlib.Path(__file__).with_name("transport-bridge.mjs")), "--bridge-arg", str(path)],
                env=configure_fixture_progress(env), stdout=out, stderr=err)
        self.children.append(child)
        return child, scenario, stdout, stderr

    def stop(self, run):
        child, _, stdout, stderr = run
        child.send_signal(signal.SIGTERM)
        child.wait(timeout=10)
        self.assertEqual(child.returncode, 0, stderr.read_text())
        for secret in ("synthetic-app-secret", "synthetic-model-secret"):
            self.assertNotIn(secret, stdout.read_text() + stderr.read_text())

    def test_slow_platform_send_stops_and_unknown_receipt_is_not_replayed_on_restart(self):
        run = self.launch(hold_send=True)
        self.wait_until(lambda: pathlib.Path(run[1]["sending"]).exists(), "SDK send not reached")
        self.assertEqual(self.receipt("transport-message")["state"], "ReplyPending")
        self.stop(run)
        self.assertTrue(pathlib.Path(run[1]["stopped"]).exists(), "stop was blocked behind the SDK send")
        self.assertEqual(self.receipt("transport-message")["state"], "ReplyPending")
        self.assertEqual(self.interactions(), [])
        request_count = len(self.requests)
        restarted = self.launch()
        self.wait_until(lambda: pathlib.Path(restarted[1]["finished"]).exists(), "unknown delivery was not retired")
        self.stop(restarted)
        self.assertFalse(pathlib.Path(restarted[1]["sending"]).exists())
        self.assertEqual(len(self.requests), request_count)
        self.assertEqual(self.receipt("transport-message")["state"], "ReplyPending")

    def test_invalid_protocol_frames_cannot_start_work_or_close_channel(self):
        run = self.launch(raw_frames=[
            {"version": 2, "type": "message", "id": "invalid-version", "scope": "c2c",
             "target_id": "transport-user", "user_id": "transport-user", "text": "不得执行"},
            {"version": 2, "type": "fatal", "code": "not-a-current-protocol-failure"},
        ])
        self.wait_until(lambda: self.state_path.exists() and any(r["state"] == "Sent" for r in
            self.documents().get("eve.channel.qqbot", {}).get("receipts.v1", {}).get("entries", [])),
            "valid message was not delivered")
        self.stop(run)
        self.assertEqual(len(self.requests), 1)
        entries = self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"]
        self.assertEqual([entry["message"]["id"] for entry in entries], ["transport-message"])
        self.assertTrue(pathlib.Path(run[1]["stopped"]).exists())

    def test_oversized_encoded_message_does_not_disconnect_real_rust_channel(self):
        # 入站上限已独立提升至512KiB；同时覆盖超限协议帧和SDK正文32KiB上限。
        run = self.launch(raw_frames=[{"version":1, "type":"message", "id":"oversized",
            "scope":"c2c", "target_id":"transport-user", "user_id":"transport-user",
            "text":"x" * (512 * 1024)}], contents=["x" * 32769, "后续正常消息"])
        self.wait_until(lambda: self.state_path.exists() and any(r["state"] == "Sent" for r in
            self.documents().get("eve.channel.qqbot", {}).get("receipts.v1", {}).get("entries", [])),
            "normal message after rejected frame was not delivered")
        self.stop(run)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.receipt("transport-message")["message"]["text"], "后续正常消息")

    def test_ingress_rejection_counts_and_shapes_reach_rust_without_private_values(self):
        run = self.launch(incoming=[{"senderId": "PRIVATE/USER", "content": "PRIVATE_BODY",
                                    "rawEventType": "PRIVATE_EVENT"}] * 3)
        self.wait_until(lambda: self.state_path.exists() and any(r["state"] == "Sent" for r in
            self.documents().get("eve.channel.qqbot", {}).get("receipts.v1", {}).get("entries", [])),
            "normal message after rejected input was not delivered")
        self.stop(run)
        rows = []
        for line in run[3].read_text().splitlines():
            if "bridge_invalid_route_or_text" in line:
                rows.append(json.loads(line[line.index("{"):]))
        self.assertTrue(rows, "Node rejection metadata did not reach Rust logging")
        self.assertEqual(rows[0]["ingress_reason"], "sender_id")
        self.assertEqual(rows[0]["ingress_stage"], "envelope")
        self.assertEqual(rows[0]["ingress_count"], "1")
        self.assertEqual(rows[0]["ingress_last_sender_id_type"], "string")
        self.assertEqual(rows[0]["ingress_last_sender_id_bytes"], str(len("PRIVATE/USER")))
        self.assertEqual(rows[0]["ingress_last_event"], "other")
        self.assertNotIn("PRIVATE", run[2].read_text() + run[3].read_text())
        self.assertEqual(len(self.requests), 1)
        self.assertEqual([entry["message"]["id"] for entry in
                          self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"]],
                         ["transport-message"])

    def test_pure_self_mention_reaches_real_rust_and_keeps_original_text_on_restart(self):
        original = " \t<@!1904159860> <@own-openid> \n"
        incoming = {"kind": "group", "rawEventType": "GROUP_MESSAGE_CREATE", "messageId": "only-mention",
                    "senderId": "transport-user", "groupOpenid": "transport-group", "content": original,
                    "mentions": [{"member_openid": "own-openid", "is_you": True}],
                    "replyTarget": {"scope": "group", "targetId": "transport-group", "msgId": "only-mention"}}
        run = self.launch(contents=[], incoming=[incoming])
        self.wait_until(lambda: self.state_path.exists() and any(r["state"] == "Sent" for r in
            self.documents().get("eve.channel.qqbot", {}).get("receipts.v1", {}).get("entries", [])),
            "pure self mention was not admitted by the real Rust channel")
        self.stop(run)
        self.assertEqual(self.receipt("only-mention")["message"]["text"], original.strip())
        self.assertEqual(len(self.requests), 1)
        self.assertIn(original.strip(), json.dumps(self.requests[0], ensure_ascii=False))
        with closing(sqlite3.connect(self.work / "state/qq-receipts.sqlite3")) as connection:
            stored = connection.execute("SELECT text FROM qq_originals WHERE message_id = ?",
                                        ("only-mention",)).fetchall()
        self.assertEqual(stored, [(original,)])
        restarted = self.launch(contents=[], incoming=[incoming])
        self.wait_until(lambda: pathlib.Path(restarted[1]["finished"]).exists(),
                        "restarted channel did not retire the duplicate mention")
        self.stop(restarted)
        self.assertEqual(len(self.requests), 1, "restart replayed the already handled mention")
        self.assertFalse(pathlib.Path(restarted[1]["sending"]).exists())


if __name__ == "__main__":
    unittest.main()
