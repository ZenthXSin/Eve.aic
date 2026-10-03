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
        self.assertEqual(report["counts"]["unconfirmed"], 1)

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
