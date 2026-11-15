import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "services/runtime"))
from semantic_index import Index


class Encoder:
    model, revision, dimensions = "fixture", "v1", 2

    def chunk(self, text):
        return text[:80]

    def embed(self, text, purpose):
        return struct.pack("<ff", 1.0, 0.0) if any(w in text.lower() for w in ["猫", "feline"]) else struct.pack("<ff", 0.0, 1.0)

    def scores(self, query, rows):
        q = struct.unpack("<ff", query)
        return [(r[0], sum(a * b for a, b in zip(q, struct.unpack("<ff", r[1])))) for r in rows]


class SearchTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.backend = self.root / "backend"
        self.phone = self.root / "phone"
        for p in [self.backend / "notes/events", self.backend / "consciousness/events", self.phone / "inbound", self.phone / "timeline"]:
            p.mkdir(parents=True)
        self.index = Index(self.root / "index", self.backend, self.phone, Encoder())

    def tearDown(self):
        self.temp.cleanup()

    def note(self, seq, revision, text):
        p = self.backend / "notes/events" / f"{seq:016d}.json"
        p.write_text(json.dumps({"seq": seq, "kind": "note", "at": "2026-11-15", "data": {
            "id": "note1", "revision": revision, "title": "照顾", "content": text, "summary": "", "group": "life"}}, ensure_ascii=False))
        return p

    def drain(self):
        self.index._next_scan = 0
        self.index._files = self.index.files()
        self.index.discover(1000)
        for _ in range(1000):
            if not self.index.prepare():
                break
        for _ in range(1000):
            if not self.index.embed_one():
                break
        else:
            self.fail("Index did not converge")

    def test_tail_semantics_restart_and_invalidation(self):
        self.note(1, 1, "旧预算1000")
        newest = self.note(2, 2, "新的预算2000。" + "背景信息" * 80 + "猫需要每天梳毛")
        self.drain()
        result = self.index.search({"query": "feline", "collection": "notes", "mode": "semantic"})
        self.assertEqual(result["items"][0]["source"]["revision"], 2)
        self.assertIn("猫", result["items"][0]["excerpt"])
        self.assertGreater(result["items"][0]["startChar"], 80)
        count = self.index.status()["chunks"]
        self.index = Index(self.root / "index", self.backend, self.phone, Encoder())
        self.assertEqual(self.index.status()["chunks"], count)
        self.note(3, 3, "最新预算3000，植物浇水")
        self.drain()
        self.assertFalse(self.index.search({"query": "feline", "collection": "notes", "mode": "semantic"})["items"])
        result = self.index.search({"query": "3000", "collection": "notes", "mode": "hybrid"})
        self.assertEqual(result["items"][0]["source"]["revision"], 3)
        newest.unlink()
        (self.backend / "notes/events/0000000000000003.json").unlink()
        self.assertFalse(self.index.search({"query": "3000", "collection": "notes"})["items"])

    def test_chat_handles_and_complete_tool_pair(self):
        (self.phone / "timeline/index.json").write_text(json.dumps({"entries": [{"key": "incoming:1", "seq": 99}]}))
        (self.phone / "inbound/0000000000000001-fixture.json").write_text(json.dumps({"seq": 1, "message": {
            "message": {"text": "猫吃饭", "conversationId": "qq:group:test", "sentAt": "2026-11-15"}}}, ensure_ascii=False))
        for seq, kind, data in [(1, "tool_start", {"callId": "same", "call": {"name": "cat", "args": {}}}),
                                (2, "tool_result", {"callId": "same", "result": {"text": "猫已喂好"}})]:
            (self.backend / "consciousness/events" / f"{seq:016d}.json").write_text(json.dumps({"seq": seq, "kind": kind,
                "at": "2026-11-15", "digest": "a" * 64, "data": data}, ensure_ascii=False))
        self.drain()
        result = self.index.search({"query": "feline", "collection": "chat", "mode": "semantic"})
        self.assertEqual(result["items"][0]["source"]["seq"], 99)
        self.assertEqual(result["items"][0]["source"]["conversationId"], "qq:group:test")
        result = self.index.search({"query": "feline", "collection": "records", "mode": "semantic"})
        self.assertEqual(result["items"][0]["source"]["toolStartSeq"], 1)
        self.assertTrue(result["items"][0]["untrusted"])
        self.assertFalse(self.index.search({"query": "feline", "collection": "records", "afterSeq": 2})["items"])

    def test_model_version_cannot_mix(self):
        other = Encoder()
        other.revision = "v2"
        with self.assertRaises(ValueError):
            Index(self.root / "index", self.backend, self.phone, other)


if __name__ == "__main__":
    unittest.main()
