"""QQ interaction routing. Only platform reference indexes establish reply edges."""
import json


def migrate(db):
    db.executescript("""
      CREATE TABLE IF NOT EXISTS chat_messages(path TEXT PRIMARY KEY,conversation TEXT,
        direction TEXT,own_index TEXT,reply_index TEXT,direct INTEGER,sent INTEGER,checks TEXT);
      CREATE INDEX IF NOT EXISTS chat_own ON chat_messages(conversation,own_index);
      CREATE INDEX IF NOT EXISTS chat_reply ON chat_messages(conversation,reply_index);
    """)
    version = "interactions-v1"
    old = db.execute("SELECT value FROM meta WHERE key='chat_routing'").fetchone()
    if old and old[0] == version:
        return
    # Remove the previous unrestricted projection before serving any search.
    db.execute("DELETE FROM lexical WHERE rowid IN (SELECT c.id FROM chunks c JOIN docs d "
               "ON d.id=c.doc_id WHERE d.collection='chat')")
    db.execute("DELETE FROM chunks WHERE doc_id IN (SELECT id FROM docs WHERE collection='chat')")
    db.execute("DELETE FROM docs WHERE collection='chat'")
    db.execute("DELETE FROM chat_messages")
    db.execute("UPDATE sources SET state='pending',error=NULL WHERE collection='chat'")
    db.execute("INSERT OR REPLACE INTO meta VALUES('chat_routing',?)", (version,))


def identifier(value):
    return value if isinstance(value, str) and value else None


def facts(event, path, checks):
    incoming = "message" in event
    envelope = event.get("message", {}) if incoming else event.get("request", {})
    message = envelope.get("message", envelope) if incoming else envelope
    destination = envelope.get("destination", {})
    conversation = message.get("conversationId")
    if not conversation and destination.get("openId"):
        conversation = "qq:" + destination.get("kind", "") + ":" + destination["openId"]
    raw = event.get("raw", {})
    data = raw.get("d", {})
    direct_mention = raw.get("t") == "GROUP_MESSAGE_CREATE" and data.get("content") == message.get("text") and any(
        isinstance(m, dict) and m.get("scope") == "single" and m.get("is_you") is True
        for m in (data.get("mentions") or [])
    )
    direct = incoming and (destination.get("kind") == "user" or
                           (conversation or "").startswith("qq:user:") or envelope.get("notify") is True or
                           raw.get("t") in {"C2C_MESSAGE_CREATE", "GROUP_AT_MESSAGE_CREATE"} or direct_mention)
    result = event.get("result") or {}
    sent = not incoming and event.get("status") == "finished" and result.get("status") == "sent" and bool(
        identifier(result.get("messageId")))
    own = envelope.get("deduplication", {}).get("messageIndex") if incoming else result.get("referenceIndex")
    return (path, conversation, "incoming" if incoming else "outgoing", identifier(own),
            identifier(envelope.get("referenceIndex")), int(direct), int(sent), json.dumps(checks))


def invalidate_neighbors(db, rows, exclude=None):
    affected = set()
    for row in rows:
        if not row:
            continue
        # A later reply can admit a formerly skipped question. A duplicate reference
        # index must revoke an earlier match as well; do not re-embed the whole chat.
        candidates = db.execute("SELECT path FROM chat_messages WHERE conversation=? AND direction='incoming' "
                                "AND direct=0 AND (own_index=? OR own_index=? OR reply_index=?)",
                                (row["conversation"], row["own_index"], row["reply_index"], row["own_index"]))
        affected.update(r[0] for r in candidates)
    affected.discard(exclude)
    db.executemany("UPDATE sources SET state='pending',error=NULL WHERE path=?", [(p,) for p in affected])
    db.executemany("UPDATE docs SET active=0 WHERE path=?", [(p,) for p in affected])


def update(db, event, path, checks):
    previous = db.execute("SELECT * FROM chat_messages WHERE path=?", (path,)).fetchone()
    current = facts(event, path, checks)
    if previous and tuple(previous) == current:
        return
    db.execute("INSERT OR REPLACE INTO chat_messages VALUES(?,?,?,?,?,?,?,?)", current)
    row = db.execute("SELECT * FROM chat_messages WHERE path=?", (path,)).fetchone()
    invalidate_neighbors(db, [previous, row], exclude=path)


def remove(db, path):
    previous = db.execute("SELECT * FROM chat_messages WHERE path=?", (path,)).fetchone()
    db.execute("DELETE FROM chat_messages WHERE path=?", (path,))
    invalidate_neighbors(db, [previous], exclude=path)


def evidence(db, path):
    row = db.execute("SELECT * FROM chat_messages WHERE path=?", (path,)).fetchone()
    if not row:
        return None
    if row["direction"] == "outgoing":
        return ("Eve 已发送", []) if row["sent"] else None
    if row["direct"]:
        return "发给 Eve", []
    targets = db.execute("SELECT * FROM chat_messages WHERE conversation=? AND own_index=? LIMIT 2",
                         (row["conversation"], row["reply_index"])).fetchall()
    if len(targets) == 1 and targets[0]["direction"] == "outgoing" and targets[0]["sent"]:
        return "引用 Eve", json.loads(targets[0]["checks"])
    targets = db.execute("SELECT path FROM chat_messages WHERE conversation=? AND own_index=? LIMIT 2",
                         (row["conversation"], row["own_index"])).fetchall()
    if len(targets) == 1:
        reply = db.execute("SELECT checks FROM chat_messages WHERE conversation=? AND reply_index=? "
                           "AND direction='outgoing' AND sent=1 ORDER BY path LIMIT 1",
                           (row["conversation"], row["own_index"])).fetchone()
        if reply:
            return "Eve 回答的问题", json.loads(reply[0])
    return None
