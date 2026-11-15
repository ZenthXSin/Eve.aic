import json
import hashlib
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
            "notify": True, "message": {"text": "猫吃饭", "conversationId": "qq:group:test", "sentAt": "2026-11-15"}}}, ensure_ascii=False))
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

    def test_agent_accepted_reads_job_id_from_message(self):
        folder = self.backend / "agents/events"
        bodies = self.backend / "agents/bodies"
        folder.mkdir(parents=True)
        bodies.mkdir()
        raw = json.dumps("猫的编码任务", ensure_ascii=False).encode()
        hashed = hashlib.sha256(raw).hexdigest()
        (bodies / (hashed + ".json")).write_bytes(raw + b"\n")
        (folder / "0000000000000001.json").write_text(json.dumps({"seq": 1, "kind": "accepted", "at": "2026-11-15", "data": {
            "agentId": "agent_coding", "job": {"jobId": "job1"}, "message": {"jobId": "job1", "direction": "outgoing",
            "status": "queued", "body": {"hash": hashed}}}}))
        self.drain()
        result = self.index.search({"query": "feline", "collection": "agents", "mode": "semantic"})
        self.assertEqual(result["items"][0]["source"]["jobId"], "job1")
        self.assertEqual(self.index.status()["errors"], 0)

    def test_model_version_cannot_mix(self):
        other = Encoder()
        other.revision = "v2"
        with self.assertRaises(ValueError):
            Index(self.root / "index", self.backend, self.phone, other)

    def incoming(self, seq, *, group="test", event="GROUP_MESSAGE_CREATE", own=None, reply=None, mentions=None):
        private = event == "C2C_MESSAGE_CREATE"
        kind = "user" if private else "group"
        text = f"猫的消息{seq}"
        p = self.phone / "inbound" / f"{seq:016d}-fixture.json"
        p.write_text(json.dumps({"seq": seq, "raw": {"t": event, "d": {"content": text, "mentions": mentions or []}},
                                "message": {"destination": {"kind": kind, "openId": group},
                                "deduplication": {"messageIndex": own}, "referenceIndex": reply,
                                "notify": private or event == "GROUP_AT_MESSAGE_CREATE",
                                "message": {"conversationId": f"qq:{kind}:{group}", "text": text}}}))
        return p

    def outgoing(self, action, *, group="test", status="sent", own=None, reply=None):
        folder = self.phone / "outbox/records"
        folder.mkdir(parents=True, exist_ok=True)
        p = folder / (action + ".json")
        p.write_text(json.dumps({"actionId": action, "status": "finished" if status != "started" else "started",
                                "request": {"destination": {"kind": "group", "openId": group},
                                            "text": "猫的回答", "referenceIndex": reply},
                                "result": {"status": status, "messageId": "not-a-reference-index", "referenceIndex": own}}))
        return p

    def chat_paths(self):
        with self.index.db() as db:
            return {Path(r[0]).name for r in db.execute("SELECT path FROM docs WHERE collection='chat' AND active=1")}

    def test_chat_routes_only_direct_interaction_and_confirmed_sends(self):
        self.incoming(1)  # ordinary group chat
        private = self.incoming(2, event="C2C_MESSAGE_CREATE")
        mention = self.incoming(3, event="GROUP_AT_MESSAGE_CREATE")
        direct = self.incoming(4, mentions=[{"scope": "single", "is_you": True}])
        self.incoming(5, mentions=[{"scope": "all", "is_you": True}])
        self.incoming(6, mentions=[{"scope": "single", "is_you": False}])
        sent = self.outgoing("sent")
        for state in ["failed", "unknown", "started"]:
            self.outgoing(state, status=state)
        self.drain()
        self.assertEqual(self.chat_paths(), {p.name for p in [private, mention, direct, sent]})
        self.assertEqual(self.index.status()["errors"], 0)

    def test_later_reply_admits_question_and_quoted_eve_with_unique_indexes(self):
        question = self.incoming(1, own="question")
        quote = self.incoming(2, reply="answer")
        self.incoming(3, group="another", reply="answer")
        self.incoming(4, reply="not-a-reference-index")
        self.drain()
        self.assertFalse(self.chat_paths())
        answer = self.outgoing("answer", own="answer", reply="question")
        self.drain()
        self.assertEqual(self.chat_paths(), {p.name for p in [question, quote, answer]})
        hits = self.index.search({"query": "feline", "collection": "chat", "mode": "semantic"})["items"]
        self.assertEqual(len(hits), 3)
        # A duplicate index makes the quote ambiguous, even if it belongs to someone else.
        duplicate = self.incoming(5, own="answer")
        self.drain()
        self.assertNotIn(quote.name, self.chat_paths())
        duplicate.unlink()
        self.drain()
        self.assertIn(quote.name, self.chat_paths())
        # A changed delivery receipt cannot keep its old dependent Q&A searchable.
        self.outgoing("answer", status="unknown", own="answer", reply="question")
        self.assertFalse(self.index.search({"query": "feline", "collection": "chat"})["items"])
        self.drain()
        self.assertFalse(self.chat_paths())

    def test_chat_routing_migration_purges_old_vectors_and_preserves_other_sources(self):
        question = self.incoming(1, event="GROUP_AT_MESSAGE_CREATE")
        self.note(1, 1, "猫的便签")
        self.drain()
        with self.index.db() as db:
            old_chunks = [r[0] for r in db.execute("SELECT id FROM chunks WHERE doc_id=?", (str(question),))]
            self.assertTrue(old_chunks)
            db.execute("DELETE FROM meta WHERE key='chat_routing'")
        # The source is ordinary group chatter under the new rules.
        self.incoming(1)
        self.index = Index(self.root / "index", self.backend, self.phone, Encoder())
        with self.index.db() as db:
            self.assertFalse(db.execute("SELECT 1 FROM docs WHERE collection='chat'").fetchone())
            for cid in old_chunks:
                self.assertFalse(db.execute("SELECT 1 FROM chunks WHERE id=?", (cid,)).fetchone())
                self.assertFalse(db.execute("SELECT 1 FROM lexical WHERE rowid=?", (cid,)).fetchone())
            self.assertTrue(db.execute("SELECT 1 FROM docs WHERE collection='notes'").fetchone())
        self.drain()
        self.assertFalse(self.chat_paths())
        self.assertTrue(question.is_file())
        self.assertTrue(self.index.search({"query": "feline", "collection": "notes"})["items"])

    def test_large_notes_do_not_block_chat_embedding(self):
        self.note(1, 1, "猫的长文" * 500)
        self.incoming(1, event="C2C_MESSAGE_CREATE")
        self.index._files = self.index.files()
        self.index.discover(1000)
        while self.index.prepare():
            pass
        self.index.embed_one()
        self.index.embed_one()
        self.assertEqual(self.index.status()["collections"]["notes"]["completed"], 0)
        self.assertEqual(self.index.status()["collections"]["chat"]["completed"], 1)


if __name__ == "__main__":
    unittest.main()
