import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("training", Path(__file__).resolve().parents[1] / "training-window.py")
training = importlib.util.module_from_spec(spec)
spec.loader.exec_module(training)


class Reports(unittest.TestCase):
    def test_new_round_preserves_source_history_and_counts_only_new_receipts_and_turns(self):
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        message = {"id": "old-1", "scope": "c2c", "target_id": "user-1", "user_id": "user-1", "text": "你好"}
        receipt = {"app_id": "1904159860", "message": message, "state": "Sent", "reply": "你好呀"}
        turn = {"input": "你好", "status": {"state": "Completed", "messages": [{"role": "Assistant", "text": "你好呀"}]}}
        receipts = {"version": 1, "entries": [receipt]}
        sessions = {"format_version": 1, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity}, "turns": [turn]}}}
        def state():
            return {"version": 1, "entries": {"eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "previous-state"
            source.mkdir()
            training.write_json(source / "state.json", state())
            before = (source / "state.json").read_bytes()
            next_round = root / "next"
            next_round.mkdir()
            training.seed_round(next_round, source)
            path = next_round / "state/state.json"
            self.assertEqual(training.collect(path)["counts"]["received_records"], 0)
            receipts["entries"].append({**receipt, "message": {**message, "id": "new-1"}})
            sessions["sessions"][identity]["turns"].append(turn)
            training.write_json(path, state())
            result = training.collect(path)
            self.assertEqual(result["counts"]["received_records"], 1)
            self.assertEqual(result["counts"]["completed_turns"], 1)
            self.assertEqual(result["counts"]["model_sent"], 1)
            self.assertEqual(result["evidence"][0]["message_id"], "new-1")
            self.assertEqual((source / "state.json").read_bytes(), before)
            third = root / "third"
            third.mkdir()
            training.seed_round(third, path.parent)
            self.assertEqual(training.collect(third / "state/state.json")["counts"]["model_sent"], 0)
            baseline_path = path.parent / "round-baseline.v1.json"
            baseline = json.loads(baseline_path.read_bytes())
            baseline["turns"][identity] = 99
            training.write_json(baseline_path, baseline)
            with self.assertRaisesRegex(ValueError, "round_baseline_mismatch"):
                training.collect(path)

    def test_report_only_uses_completed_and_sent_model_pairs(self):
        message = {"id": "g.1!", "scope": "group", "target_id": "group-1", "user_id": "user-1", "text": "我喜欢简短分段"}
        reply = "收到。\n\n每段两句合适吗？"
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "group", "group-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        receipts = {"version": 1, "entries": [{"app_id": "1904159860", "message": message, "state": "Sent", "reply": reply},
            {"app_id": "1904159860", "message": {**message, "id": "g.2", "text": "/train status"}, "state": "Sent", "reply": "当前已开启"},
            {"app_id": "1904159860", "message": {**message, "id": "g.3"}, "state": "ReplyPending", "reply": "未确认问题？"},
            {"app_id": "1904159860", "message": {**message, "id": "g.4"}, "state": "Failed", "reply": "失败问题？"}]}
        sessions = {"format_version": 1, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity}, "turns": [
            {"input": message["text"], "status": {"state": "Completed", "messages": [{"role": "User", "text": message["text"]}, {"role": "Assistant", "text": reply}]}},
            {"input": "未知", "status": {"state": "Pending"}}]}}}
        state = {"version": 1, "entries": {"eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                                               "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(json.dumps(state))
            report = training.collect(path)
        self.assertEqual(len(report["evidence"]), 1)
        self.assertEqual(report["evidence"][0]["input"], message["text"])
        self.assertEqual(report["counts"]["question_replies"], 1)
        self.assertEqual(report["counts"]["paragraphs"], 2)
        self.assertEqual(report["counts"]["feedback_candidates"], 1)
        self.assertEqual(report["counts"]["group_at_sent"], 2)
        self.assertEqual(report["counts"]["model_sent"], 1)
        self.assertEqual(report["counts"]["unconfirmed"], 1)

    def test_segmented_receipts_count_as_one_full_reply(self):
        message = {"id": "c.1", "scope": "c2c", "target_id": "user-1", "user_id": "user-1", "text": "分段问题"}
        reply = "第一段。\n\n第二段要问吗？"
        first = len("第一段。".encode())
        parts = [{"start": 0, "end": first, "state": "Sent"},
                 {"start": first + 2, "end": len(reply.encode()), "state": "Sent"}]
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        receipts = {"version": 2, "entries": [{"app_id": "1904159860", "message": message, "state": "Sent", "reply": reply,
                                               "segments": {"planner": "paragraph-v1", "parts": parts}},
            {"app_id": "1904159860", "message": {**message, "id": "c.2"}, "state": "Failed", "reply": reply,
             "segments": {"planner": "paragraph-v1", "parts": [{**parts[0]}, {**parts[1], "state": "Failed"}]}}]}
        sessions = {"format_version": 1, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity}, "turns": [
            {"input": message["text"], "status": {"state": "Completed", "messages": [{"role": "User", "text": message["text"]}, {"role": "Assistant", "text": reply}]}}]}}}
        state = {"version": 1, "entries": {"eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                                           "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(json.dumps(state))
            report = training.collect(path)
        self.assertEqual((report["counts"]["model_sent"], report["counts"]["failed"]), (1, 1))
        self.assertEqual(report["counts"]["paragraphs"], 2)

    def test_encrypted_state_roundtrip_authentication_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, QQBOT_APP_SECRET="test-only-password"):
            root = Path(directory) / "root"
            (root / "state").mkdir(parents=True)
            text = '{"input":"交流正文", "reply":"训练回复"}'
            (root / "state/state.json").write_text(text)
            (root / "stdout").write_text("private process diagnostics must be excluded")
            training.write_json(root / "training-report.json", {"evidence": ["交流正文"]})
            training.write_json(root / "window.json", {"ready": True})
            encrypted = Path(directory) / "result.enc"
            training.seal(root, encrypted)
            ciphertext = encrypted.read_bytes()
            self.assertNotIn("交流正文".encode(), ciphertext)
            self.assertNotIn(b"test-only-password", ciphertext)
            restored = Path(directory) / "restored"
            training.unseal(encrypted, restored)
            self.assertEqual((restored / "state/state.json").read_text(), text)
            self.assertFalse((restored / "stdout").exists())
            with self.assertRaisesRegex(ValueError, "destination_exists"):
                training.unseal(encrypted, restored)
            with patch.dict(os.environ, QQBOT_APP_SECRET="wrong-password"):
                with self.assertRaisesRegex(ValueError, "authentication_failed"):
                    training.unseal(encrypted, Path(directory) / "wrong")
            encrypted.write_bytes(ciphertext[:-1] + bytes([ciphertext[-1] ^ 1]))
            with self.assertRaisesRegex(ValueError, "authentication_failed"):
                training.unseal(encrypted, Path(directory) / "tampered")
            self.assertFalse((Path(directory) / "tampered").exists())

    def test_corrupt_training_state_is_encrypted_instead_of_cleared(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, QQBOT_APP_SECRET="test-only-password"):
            root = Path(directory) / "root"
            (root / "state").mkdir(parents=True)
            (root / "state/state.json").write_bytes(b"corrupt-evidence")
            training.write_json(root / "window.json", {"returncode": 1, "ready": False})
            output = Path(directory) / "output"
            safe = training.finish(root, output)
            self.assertFalse(safe["process_ok"])
            self.assertFalse(safe["interaction_ok"])
            self.assertEqual(safe["evidence_status"], "analysis_failed_state_preserved")
            training.unseal(output / "training-state.enc", Path(directory) / "restored")
            self.assertEqual((Path(directory) / "restored/state/state.json").read_bytes(), b"corrupt-evidence")


if __name__ == "__main__":
    unittest.main()
