"""Rebuildable SQLite search projection; originals are read-only and remain authoritative."""
import hashlib
import heapq
import json
import re
import sqlite3
import time
from contextlib import contextmanager
from pathlib import Path

COLLECTIONS = ("notes", "chat", "agents", "records", "memory")


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def terms(text):
    words = re.findall(r"[a-z0-9_./:-]+|[\u3400-\u9fff]", text.lower())
    chinese = re.findall(r"[\u3400-\u9fff]+", text)
    return list(dict.fromkeys(words + [s[i:i + 2] for s in chinese for i in range(len(s) - 1)]))[:128]


def searchable(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(searchable(x) for x in value)
    if isinstance(value, dict):
        return "\n".join(k + ": " + searchable(v) for k, v in value.items()
                         if k not in {"modelOutput", "dataUrl", "images", "usage", "sourceCommit"})
    return json.dumps(value, ensure_ascii=False)


class Index:
    def __init__(self, directory, backend, phone, encoder):
        self.directory, self.backend = Path(directory), Path(backend)
        self.phone = Path(phone) if phone else None
        self.encoder = encoder
        self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.path = self.directory / "index.sqlite3"
        with self.db() as db:
            db.executescript("""
              CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
              CREATE TABLE IF NOT EXISTS sources(path TEXT PRIMARY KEY,collection TEXT,seq INTEGER,
                stamp TEXT,state TEXT DEFAULT 'pending',error TEXT);
              CREATE INDEX IF NOT EXISTS source_work ON sources(state,collection,seq DESC);
              CREATE TABLE IF NOT EXISTS docs(id TEXT PRIMARY KEY,path TEXT,collection TEXT,seq INTEGER,
                at TEXT,kind TEXT,title TEXT,text TEXT,refs TEXT,checks TEXT,offset INTEGER DEFAULT 0,
                done INTEGER DEFAULT 0,active INTEGER DEFAULT 1,note_id TEXT,note_revision INTEGER);
              CREATE INDEX IF NOT EXISTS doc_work ON docs(active,done,collection,seq DESC);
              CREATE INDEX IF NOT EXISTS doc_note ON docs(note_id,note_revision);
              CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY,doc_id TEXT,start INTEGER,end INTEGER,
                text TEXT,vector BLOB,UNIQUE(doc_id,start));
              CREATE INDEX IF NOT EXISTS chunk_doc ON chunks(doc_id);
              CREATE VIRTUAL TABLE IF NOT EXISTS lexical USING fts5(tokens);
              CREATE TABLE IF NOT EXISTS latest_notes(id TEXT PRIMARY KEY,revision INTEGER);
            """)
            version = json.dumps([2, encoder.model, encoder.revision, encoder.dimensions])
            old = db.execute("SELECT value FROM meta WHERE key='version'").fetchone()
            if old and old[0] != version:
                raise ValueError("Model/index version changed; use a new index directory")
            db.execute("INSERT OR IGNORE INTO meta VALUES('version',?)", (version,))
        self.discovered = False
        self.scan_complete = False
        self.last_error = None
        self._files = iter(())
        self._next_scan = 0
        self._timeline_stamp = None
        self._timeline = {}

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=15)
        db.row_factory = sqlite3.Row
        db.execute("PRAGMA journal_mode=WAL")
        db.execute("PRAGMA cache_size=-4096")
        db.execute("PRAGMA busy_timeout=15000")
        try:
            with db:
                yield db
        finally:
            db.close()

    def files(self):
        for collection, folder in [("notes", self.backend / "notes/events"),
                                   ("agents", self.backend / "agents/events"),
                                   ("memory", self.backend / "memory/events"),
                                   ("records", self.backend / "consciousness/events")]:
            if folder.is_dir():
                for p in sorted(folder.glob("[0-9]*.json"), reverse=True):
                    yield collection, p, int(p.stem)
        if self.phone:
            for sub in ["inbound", "outbox/records"]:
                folder = self.phone / sub
                if folder.is_dir():
                    for p in sorted(folder.glob("*.json"), reverse=True):
                        yield "chat", p, int(p.name[:16]) if p.name[:16].isdigit() else 0

    def discover(self, limit=100):
        if time.monotonic() < self._next_scan:
            return
        with self.db() as db:
            for _ in range(limit):
                try:
                    collection, path, seq = next(self._files)
                except StopIteration:
                    if self.discovered:
                        self.scan_complete = True
                        self._next_scan = time.monotonic() + 30
                    self.discovered = True
                    self._files = self.files()
                    break
                st = path.stat()
                stamp = f"{st.st_mtime_ns}:{st.st_size}"
                old = db.execute("SELECT stamp FROM sources WHERE path=?", (str(path),)).fetchone()
                if not old or old[0] != stamp:
                    db.execute("INSERT INTO sources(path,collection,seq,stamp) VALUES(?,?,?,?) "
                               "ON CONFLICT(path) DO UPDATE SET stamp=excluded.stamp,state='pending',error=NULL",
                               (str(path), collection, seq, stamp))
                    # Immediately invalidate changed originals; never serve their previous projection.
                    db.execute("UPDATE docs SET active=0 WHERE path=?", (str(path),))
                    if collection == "notes":
                        note = json.loads(path.read_text())["data"]
                        db.execute("INSERT INTO latest_notes VALUES(?,?) ON CONFLICT(id) DO UPDATE "
                                   "SET revision=MAX(revision,excluded.revision)", (note["id"], note["revision"]))
                        db.execute("UPDATE docs SET active=0 WHERE note_id=? AND note_revision<?",
                                   (note["id"], note["revision"]))

    def extract(self, row):
        path = Path(row["path"])
        raw = path.read_bytes()
        event = json.loads(raw)
        checks = [{"path": str(path), "sha256": digest(raw)}]
        collection, seq = row["collection"], event.get("seq", row["seq"])
        data, kind = event.get("data", {}), event.get("kind", "message")
        refs = {"collection": collection, "seq": seq}
        at = event.get("at", event.get("receivedAt", ""))
        title, text, note_id, revision = kind, "", None, None
        if collection == "notes":
            note_id, revision = data["id"], data["revision"]
            refs.update(id=note_id, revision=revision, contextRecordSeq=data.get("contextRecordSeq"))
            title = data["title"]
            text = "\n".join([data.get("alias", ""), title, data.get("summary", ""), data["content"]])
            refs["group"] = data.get("group", "")
        elif collection == "memory" and kind == "formation":
            text = data["text"]
            refs.update(id=data["id"], contextRecordSeq=data.get("contextRecordSeq"), sources=data["sources"])
        elif collection == "agents" and kind in {"accepted", "reply"}:
            message = data["message"]
            body = self.backend / "agents/bodies" / (message["body"]["hash"] + ".json")
            body_raw = body.read_bytes()
            if digest(body_raw.rstrip(b"\n")) != message["body"]["hash"]:
                raise ValueError("Agent message body checksum mismatch")
            checks.append({"path": str(body), "sha256": digest(body_raw)})
            text = json.loads(body_raw)
            title = message["direction"] + " / " + message["status"]
            refs.update(agentId=data["agentId"], jobId=message["jobId"], status=message["status"])
        elif collection == "records":
            refs["digest"] = event["digest"]
            if kind == "model_response":
                text = data.get("response", {}).get("content", "")
            elif kind in {"thought", "compression", "context_state"}:
                text = searchable(data)
            elif kind == "tool_result":
                # Locate the corresponding complete tool_start, never split its call/receipt identity.
                paired = None
                for previous in range(seq - 1, max(0, seq - 128), -1):
                    p = path.parent / f"{previous:016d}.json"
                    if not p.exists():
                        continue
                    b = p.read_bytes()
                    candidate = json.loads(b)
                    if candidate.get("kind") == "tool_start" and candidate["data"].get("callId") == data.get("callId"):
                        paired = candidate
                        checks.append({"path": str(p), "sha256": digest(b)})
                        break
                if paired:
                    refs["toolStartSeq"] = paired["seq"]
                    text = "调用:\n" + searchable(paired["data"]["call"]) + "\n回执:\n" + searchable(data.get("result"))
                else:
                    text = "回执（调用原件未配对）:\n" + searchable(data.get("result"))
                refs["callId"] = data.get("callId")
            elif kind == "notice" and data.get("source") not in {"phone", "agents", "scripts"}:
                text = searchable(data)
        elif collection == "chat":
            if "message" in event:
                message = event["message"].get("message", event["message"])
                text = message.get("text", message.get("content", ""))
                target = message.get("conversation", {})
                refs.update(conversationId=message.get("conversationId", target.get("id")), incomingSeq=seq)
                at = message.get("sentAt", message.get("timestamp", at))
                title = "QQ 收到的消息"
            elif "request" in event:
                request = event["request"]
                text = request.get("text", request.get("content", ""))
                refs.update(actionId=event.get("actionId", event.get("id")), status=event.get("status"),
                            conversationId=request.get("conversationId"))
                title = "QQ 发出动作 / " + str(event.get("status", "unknown"))
                at = event.get("finishedAt", event.get("startedAt", at))
                destination = request.get("destination", {})
                if not refs.get("conversationId") and destination.get("openId"):
                    refs["conversationId"] = "qq:" + destination.get("kind", "") + ":" + destination["openId"]
        if collection == "chat" and self.phone:
            timeline = self.phone / "timeline/index.json"
            if timeline.exists():
                stamp = timeline.stat().st_mtime_ns
                if stamp != self._timeline_stamp:
                    entries = json.loads(timeline.read_text())["entries"]
                    self._timeline = {e["key"]: e["seq"] for e in entries}
                    self._timeline_stamp = stamp
                source_key = "incoming:" + str(seq) if "message" in event else "outgoing:" + str(event.get("actionId"))
                if source_key in self._timeline:
                    refs["seq"] = self._timeline[source_key]
        return {"id": str(path), "path": str(path), "collection": collection, "seq": seq,
                "at": str(at), "kind": kind, "title": title, "text": text if isinstance(text, str) else searchable(text),
                "refs": json.dumps(refs, ensure_ascii=False), "checks": json.dumps(checks),
                "note_id": note_id, "note_revision": revision}

    def prepare(self):
        with self.db() as db:
            row = db.execute("SELECT * FROM sources WHERE state='pending' ORDER BY "
                             "CASE collection WHEN 'notes' THEN 0 WHEN 'agents' THEN 1 WHEN 'chat' THEN 2 "
                             "WHEN 'memory' THEN 3 ELSE 4 END,seq DESC LIMIT 1").fetchone()
        if not row:
            return False
        try:
            doc = self.extract(row)
            with self.db() as db:
                old = db.execute("SELECT id FROM chunks WHERE doc_id=?", (doc["id"],)).fetchall()
                db.executemany("DELETE FROM lexical WHERE rowid=?", [(r[0],) for r in old])
                db.execute("DELETE FROM chunks WHERE doc_id=?", (doc["id"],))
                db.execute("DELETE FROM docs WHERE id=?", (doc["id"],))
                if doc["text"].strip():
                    active = 1
                    if doc["note_id"]:
                        latest = db.execute("SELECT revision FROM latest_notes WHERE id=?", (doc["note_id"],)).fetchone()
                        active = int(not latest or latest[0] == doc["note_revision"])
                    keys = list(doc) + ["active"]
                    db.execute("INSERT INTO docs(" + ",".join(keys) + ") VALUES(" + ",".join("?" for _ in keys) + ")",
                               list(doc.values()) + [active])
                db.execute("UPDATE sources SET state='done' WHERE path=?", (row["path"],))
            return True
        except Exception as e:
            with self.db() as db:
                db.execute("UPDATE sources SET state='error',error=? WHERE path=?", (type(e).__name__, row["path"]))
            self.last_error = {"source": row["collection"], "seq": row["seq"], "error": type(e).__name__}
            return False

    def embed_one(self):
        with self.db() as db:
            row = db.execute("SELECT id,offset,substr(text,offset+1,1200) AS remaining,length(text) AS total "
                             "FROM docs WHERE active=1 AND done=0 ORDER BY CASE collection WHEN 'notes' THEN 0 "
                             "WHEN 'agents' THEN 1 WHEN 'chat' THEN 2 WHEN 'memory' THEN 3 ELSE 4 END,offset,seq DESC LIMIT 1").fetchone()
        if not row:
            return False
        text = self.encoder.chunk(row["remaining"])
        vector = self.encoder.embed(text, "passage")
        end = row["offset"] + len(text)
        done = end >= row["total"]
        with self.db() as db:
            db.execute("INSERT OR REPLACE INTO chunks(doc_id,start,end,text,vector) VALUES(?,?,?,?,?)",
                       (row["id"], row["offset"], end, text, vector))
            cid = db.execute("SELECT id FROM chunks WHERE doc_id=? AND start=?", (row["id"], row["offset"])).fetchone()[0]
            db.execute("INSERT OR REPLACE INTO lexical(rowid,tokens) VALUES(?,?)", (cid, " ".join(terms(text))))
            db.execute("UPDATE docs SET offset=?,done=? WHERE id=?", (end if done else max(row["offset"] + 1, end - 40), int(done), row["id"]))
        return True

    def status(self):
        with self.db() as db:
            collections = {}
            for c in COLLECTIONS:
                rows = db.execute("SELECT COUNT(*),SUM(done) FROM docs WHERE collection=? AND active=1", (c,)).fetchone()
                pending = db.execute("SELECT COUNT(*) FROM sources WHERE collection=? AND state='pending'", (c,)).fetchone()[0]
                collections[c] = {"documents": rows[0], "completed": rows[1] or 0, "pendingSources": pending}
            errors = db.execute("SELECT COUNT(*) FROM sources WHERE state='error'").fetchone()[0]
            chunks = db.execute("SELECT COUNT(*) FROM chunks").fetchone()[0]
        return {"model": self.encoder.model, "revision": self.encoder.revision, "dimensions": self.encoder.dimensions,
                "collections": collections, "chunks": chunks, "discoveryStarted": self.discovered, "discoveryComplete": self.scan_complete,
                "errors": errors, "lastError": self.last_error}

    def search(self, args):
        query = args.get("query")
        collection = args.get("collection", "all")
        mode = args.get("mode", "hybrid")
        limit = args.get("limit", 8)
        if not isinstance(query, str) or not query.strip() or len(query) > 2048 or collection not in (*COLLECTIONS, "all"):
            raise ValueError("Invalid search query or collection")
        if mode not in {"semantic", "hybrid", "literal"} or type(limit) is not int or not 1 <= limit <= 40:
            raise ValueError("Invalid search mode or limit")
        where, values = "d.active=1", []
        if collection != "all":
            where += " AND d.collection=?"
            values.append(collection)
        for field in ["afterSeq", "beforeSeq"]:
            if field in args:
                if type(args[field]) is not int or args[field] < 0:
                    raise ValueError("Invalid sequence filter")
                where += " AND d.seq" + (">?" if field == "afterSeq" else "<?")
                values.append(args[field])
        if "group" in args:
            where += " AND json_extract(d.refs,'$.group')=?"
            values.append(args["group"])
        if "conversationId" in args:
            where += " AND json_extract(d.refs,'$.conversationId')=?"
            values.append(args["conversationId"])
        rankings, scores = [], {}
        with self.db() as db:
            if mode != "literal":
                vector = self.encoder.embed(query, "query")
                top = []
                cursor = db.execute("SELECT c.id,c.vector FROM chunks c JOIN docs d ON d.id=c.doc_id WHERE " + where, values)
                while rows := cursor.fetchmany(256):
                    for cid, score in self.encoder.scores(vector, rows):
                        if len(top) < 80:
                            heapq.heappush(top, (score, cid))
                        elif score > top[0][0]:
                            heapq.heapreplace(top, (score, cid))
                scores.update({cid: score for score, cid in top})
                rankings.append([cid for score, cid in sorted(top, reverse=True) if score >= 0.5])
            if mode != "semantic":
                tokens = terms(query)
                if tokens:
                    match = " OR ".join('"' + t.replace('"', '""') + '"' for t in tokens)
                    lexical = db.execute("SELECT c.id FROM lexical JOIN chunks c ON c.id=lexical.rowid "
                                         "JOIN docs d ON d.id=c.doc_id WHERE lexical MATCH ? AND " + where +
                                         " ORDER BY bm25(lexical) LIMIT 80", [match] + values).fetchall()
                    rankings.append([r[0] for r in lexical])
            merged = {}
            for rank in rankings:
                for i, cid in enumerate(rank):
                    merged[cid] = merged.get(cid, 0) + 1 / (60 + i + 1)
            items, seen = [], set()
            for cid in sorted(merged, key=merged.get, reverse=True):
                row = db.execute("SELECT d.id,d.refs,d.checks,d.at,d.kind,d.title,c.text AS excerpt,c.start,c.end FROM chunks c "
                                 "JOIN docs d ON d.id=c.doc_id WHERE c.id=?", (cid,)).fetchone()
                if row["id"] in seen:
                    continue
                valid = True
                for check in json.loads(row["checks"]):
                    try:
                        if digest(Path(check["path"]).read_bytes()) != check["sha256"]:
                            valid = False
                    except OSError:
                        valid = False
                if not valid:
                    db.execute("UPDATE docs SET active=0 WHERE id=?", (row["id"],))
                    continue
                seen.add(row["id"])
                items.append({"source": json.loads(row["refs"]), "at": row["at"], "kind": row["kind"],
                              "title": row["title"], "excerpt": row["excerpt"][:700], "startChar": row["start"],
                              "endChar": row["end"], "score": merged[cid],
                              "semanticScore": scores.get(cid), "untrusted": True})
                if len(items) >= limit:
                    break
        return {"items": items, "retrieval": mode, "index": self.status()}
