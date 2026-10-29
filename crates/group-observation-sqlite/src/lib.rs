//! 群消息原话与主动发送账本；重启不重放、不补发，异常保留原记录。
use eve_chime_api::{
    ChimeReply, GroupExchange, GroupMessage, GroupObservationStore, GroupObservationWindow,
    GroupOutbound, ObservedMessage,
};
mod digests;
pub use digests::SqliteGroupDigests;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
const BROKEN: &str = "群消息数据库不可用；原记录保留，停止本次观察和发送";
pub struct SqliteGroupObservations {
    db: Mutex<Connection>,
    healthy: AtomicBool,
}
impl SqliteGroupObservations {
    pub fn open(path: &Path) -> Result<Self, String> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(BROKEN.into());
        }
        let exists = path.exists();
        let marker = path.with_extension("required");
        if marker.exists() && !exists {
            return Err("群消息数据库缺失；保留现场，请恢复原数据库".into());
        }
        let mut db = Connection::open(path).map_err(|_| BROKEN)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| BROKEN)?;
        if !exists {
            db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE metadata(version INTEGER NOT NULL); INSERT INTO metadata VALUES(1); CREATE TABLE observations(seq INTEGER PRIMARY KEY AUTOINCREMENT,conversation TEXT NOT NULL,id TEXT NOT NULL,payload TEXT NOT NULL,UNIQUE(conversation,id)); CREATE INDEX observation_window ON observations(conversation,seq); CREATE TABLE outbound(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,conversation TEXT NOT NULL,reply_to TEXT NOT NULL,payload TEXT NOT NULL,status TEXT NOT NULL,UNIQUE(conversation,reply_to)); COMMIT;").map_err(|_|BROKEN)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| BROKEN)?;
            }
        }
        let versions: Vec<u64> = db
            .prepare("SELECT version FROM metadata")
            .map_err(|_| BROKEN)?
            .query_map([], |r| r.get(0))
            .map_err(|_| BROKEN)?
            .collect::<Result<_, _>>()
            .map_err(|_| BROKEN)?;
        let check: String = db
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|_| BROKEN)?;
        if (versions != [1] && versions != [2]) || check != "ok" {
            return Err(BROKEN.into());
        }
        db.prepare("SELECT seq,conversation,id,payload FROM observations LIMIT 0")
            .map_err(|_| BROKEN)?;
        db.prepare("SELECT seq,id,conversation,reply_to,payload,status FROM outbound LIMIT 0")
            .map_err(|_| BROKEN)?;
        if versions == [1] {
            db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE exchanges(seq INTEGER PRIMARY KEY AUTOINCREMENT,conversation TEXT NOT NULL,source TEXT NOT NULL,reply TEXT NOT NULL,UNIQUE(conversation,reply)); UPDATE metadata SET version=2; COMMIT;").map_err(|_|BROKEN)?;
        }
        db.prepare("SELECT seq,conversation,source,reply FROM exchanges LIMIT 0")
            .map_err(|_| BROKEN)?;
        // 普通回复保存正文后、关联提交前中断的，可由宿主稳定 ID 恢复；不推断缺失原话。
        db.execute("INSERT OR IGNORE INTO exchanges(conversation,source,reply) SELECT a.conversation,a.id,b.id FROM observations a JOIN observations b ON b.conversation=a.conversation AND b.id='sent-'||substr(a.id,4) WHERE a.id LIKE 'qg-%' AND json_extract(a.payload,'$.from_me')=0 AND json_extract(b.payload,'$.from_me')=1",[]).map_err(|_|BROKEN)?;
        db.pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| BROKEN)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| BROKEN)?;
        recover_confirmed(&mut db)?;
        // 上次进程未确认的发送不重试，窗口只供读取，不主动重放旧消息。
        db.execute(
            "UPDATE outbound SET status='unknown' WHERE status IN ('queued','sending')",
            [],
        )
        .map_err(|_| BROKEN)?;
        if !marker.exists() {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&marker)
                .map_err(|_| BROKEN)?;
            file.write_all(b"group-observations-v1\n")
                .and_then(|()| file.sync_all())
                .map_err(|_| BROKEN)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)
                    .and_then(|f| f.sync_all())
                    .map_err(|_| BROKEN)?;
            }
        }
        Ok(Self {
            db: Mutex::new(db),
            healthy: AtomicBool::new(true),
        })
    }
    fn run<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T, String>) -> Result<T, String> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(BROKEN.into());
        }
        let result = self
            .db
            .lock()
            .map_err(|_| BROKEN.to_string())
            .and_then(|mut db| f(&mut db));
        if result.as_ref().is_err_and(|e| e == BROKEN) {
            self.healthy.store(false, Ordering::SeqCst);
        }
        result
    }
}
/// 旧格式已经有成功投递正文和原消息，只补齐可验证问答，不推断未知发送结果。
fn recover_confirmed(db: &mut Connection) -> Result<(), String> {
    let tx = db.transaction().map_err(|_| BROKEN)?;
    let rows: Vec<(String, String, String, String)> = {
        let mut query=tx.prepare("SELECT o.id,o.conversation,o.reply_to,o.payload FROM outbound o WHERE o.status='sent' AND NOT EXISTS(SELECT 1 FROM exchanges x WHERE x.conversation=o.conversation AND x.reply=o.id||':reply')").map_err(|_|BROKEN)?;
        query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|_| BROKEN)?
            .collect::<Result<_, _>>()
            .map_err(|_| BROKEN)?
    };
    for (id, conversation, source, payload) in rows {
        let outbound = outbound(payload)?;
        if outbound.id != id
            || outbound.reply.conversation != conversation
            || outbound.reply.reply_to != source
        {
            return Err(BROKEN.into());
        }
        let original: String = tx
            .query_row(
                "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                params![conversation, source],
                |r| r.get(0),
            )
            .map_err(|_| BROKEN)?;
        let original: GroupMessage = serde_json::from_str(&original).map_err(|_| BROKEN)?;
        if !original.valid() || original.from_me || original.conversation != conversation {
            return Err(BROKEN.into());
        }
        let reply_id = format!("{id}:reply");
        let saved: Option<String> = tx
            .query_row(
                "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                params![conversation, reply_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| BROKEN)?;
        if let Some(saved) = saved {
            let saved: GroupMessage = serde_json::from_str(&saved).map_err(|_| BROKEN)?;
            if !saved.valid()
                || !saved.from_me
                || saved.text != outbound.reply.text
                || saved.conversation != conversation
            {
                return Err(BROKEN.into());
            }
        } else {
            // 这是恢复时观察到的已确认旧回复，记录恢复时刻，不编造原平台发送时间。
            let reply = GroupMessage {
                id: reply_id.clone(),
                conversation: conversation.clone(),
                speaker: "eve-chime".into(),
                speaker_name: Some("Eve".into()),
                text: outbound.reply.text,
                mentions_me: false,
                replies_to_me: false,
                from_me: true,
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(1, |v| v.as_millis() as u64)
                    .max(original.at_ms.saturating_add(1)),
            };
            let payload = serde_json::to_string(&reply).map_err(|_| BROKEN)?;
            tx.execute(
                "INSERT INTO observations(conversation,id,payload) VALUES(?1,?2,?3)",
                params![conversation, reply_id, payload],
            )
            .map_err(|_| BROKEN)?;
        }
        tx.execute(
            "INSERT INTO exchanges(conversation,source,reply) VALUES(?1,?2,?3)",
            params![conversation, source, reply_id],
        )
        .map_err(|_| BROKEN)?;
    }
    tx.commit().map_err(|_| BROKEN)?;
    Ok(())
}
fn outbound(raw: String) -> Result<GroupOutbound, String> {
    let value: GroupOutbound = serde_json::from_str(&raw).map_err(|_| BROKEN)?;
    if value.reply.text.len() > 32768 || value.reply.parts.len() > 3 {
        return Err(BROKEN.into());
    }
    Ok(value)
}
fn checked_outbound(
    raw: String,
    id: &str,
    conversation: &str,
    reply_to: &str,
) -> Result<GroupOutbound, String> {
    let value = outbound(raw)?;
    if value.id != id
        || value.reply.conversation != conversation
        || value.reply.reply_to != reply_to
    {
        return Err(BROKEN.into());
    }
    Ok(value)
}
fn checked_message(raw: &str, conversation: &str, id: &str) -> Result<GroupMessage, String> {
    let message: GroupMessage = serde_json::from_str(raw).map_err(|_| BROKEN)?;
    if !message.valid() || message.conversation != conversation || message.id != id {
        return Err(BROKEN.into());
    }
    Ok(message)
}
impl GroupObservationStore for SqliteGroupObservations {
    fn append(&self, m: &GroupMessage) -> Result<bool, String> {
        if !m.valid() || m.text.len() > 32768 {
            return Err("群消息观察输入无效".into());
        }
        let payload = serde_json::to_string(m).map_err(|_| BROKEN)?;
        self.run(|db| {
            let tx = db.transaction().map_err(|_| BROKEN)?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                    params![m.conversation, m.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| BROKEN)?;
            if let Some(old) = old {
                let mut before: GroupMessage = serde_json::from_str(&old).map_err(|_| BROKEN)?;
                before.at_ms = m.at_ms;
                if before != *m {
                    return Err("群消息 ID 冲突；未覆盖原记录".into());
                }
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO observations(conversation,id,payload) VALUES(?1,?2,?3)",
                params![m.conversation, m.id, payload],
            )
            .map_err(|_| BROKEN)?;
            tx.commit().map_err(|_| BROKEN)?;
            Ok(true)
        })
    }
    fn recent(&self, conversation: &str, limit: usize) -> Result<Vec<GroupMessage>, String> {
        self.run(|db| {
            let mut q = db.prepare("SELECT id,payload FROM observations WHERE conversation=?1 ORDER BY seq DESC LIMIT ?2").map_err(|_| BROKEN)?;
            let rows = q.query_map(params![conversation,limit.min(100)], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(|_| BROKEN)?;
            let mut values = rows.map(|r| {
                let (id, raw) = r.map_err(|_| BROKEN)?;
                checked_message(&raw, conversation, &id)
            }).collect::<Result<Vec<_>,_>>()?;
            values.reverse();
            Ok(values)
        })
    }
    fn window_ending_at(
        &self,
        conversation: &str,
        anchor_ids: &[String],
        limit: usize,
    ) -> Result<GroupObservationWindow, String> {
        let invalid = || "群轮次锚点无效或缺失；未读取最新消息".to_owned();
        let unique: std::collections::BTreeSet<_> = anchor_ids.iter().cloned().collect();
        if anchor_ids.is_empty()
            || anchor_ids.len() > 32
            || unique.len() != anchor_ids.len()
            || !(1..=100).contains(&limit)
            || anchor_ids.len() > limit
        {
            return Err(invalid());
        }
        self.run(|db| {
            let tx = db.transaction().map_err(|_| BROKEN)?;
            let mut anchors = Vec::new();
            for id in anchor_ids {
                let row: Option<(i64,String)> = tx.query_row("SELECT seq,payload FROM observations WHERE conversation=?1 AND id=?2",params![conversation,id],|r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_| BROKEN)?;
                let (seq,payload) = row.ok_or_else(invalid)?;
                let message: GroupMessage = serde_json::from_str(&payload).map_err(|_| BROKEN)?;
                if seq <= 0 || message.conversation != conversation || &message.id != id || !message.valid() { return Err(BROKEN.to_owned()); }
                if message.from_me || anchors.last().is_some_and(|prior: &ObservedMessage| prior.seq >= seq as u64) { return Err(invalid()); }
                anchors.push(ObservedMessage { seq: seq as u64, message });
            }
            let cutoff = anchors.last().ok_or_else(invalid)?.seq;
            let mut result = anchors;
            {
                let mut query = tx.prepare("SELECT seq,payload FROM observations WHERE conversation=?1 AND seq<=?2 ORDER BY seq DESC LIMIT ?3").map_err(|_| BROKEN)?;
                let rows = query.query_map(params![conversation,cutoff as i64,limit as i64],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).map_err(|_| BROKEN)?;
                for row in rows {
                    let (seq,payload) = row.map_err(|_| BROKEN)?;
                    let message: GroupMessage = serde_json::from_str(&payload).map_err(|_| BROKEN)?;
                    if seq <= 0 || message.conversation != conversation || !message.valid() { return Err(BROKEN.to_owned()); }
                    if result.len() < limit && !unique.contains(&message.id) { result.push(ObservedMessage { seq: seq as u64, message }); }
                }
            }
            result.sort_by_key(|row| row.seq);
            tx.commit().map_err(|_| BROKEN)?;
            Ok(GroupObservationWindow { cutoff_seq: cutoff, anchor_ids: anchor_ids.to_vec(), messages: result })
        })
    }
    fn search(
        &self,
        conversation: &str,
        query: &str,
        before_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<ObservedMessage>, String> {
        let before = before_seq
            .map(i64::try_from)
            .transpose()
            .map_err(|_| BROKEN)?;
        self.run(|db| {
            let mut statement = db
                .prepare(
                    "SELECT seq,payload FROM observations WHERE conversation=?1 \
                 AND (?2 IS NULL OR seq<?2) \
                 AND (?3='' OR instr(lower(json_extract(payload,'$.text')),lower(?3))>0) \
                 ORDER BY seq DESC LIMIT ?4",
                )
                .map_err(|_| BROKEN)?;
            let rows = statement
                .query_map(
                    params![conversation, before, query, limit.min(100) as i64],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(|_| BROKEN)?;
            rows.map(|row| {
                let (seq, payload) = row.map_err(|_| BROKEN)?;
                let message: GroupMessage = serde_json::from_str(&payload).map_err(|_| BROKEN)?;
                if seq <= 0 || message.conversation != conversation {
                    return Err(BROKEN.to_string());
                }
                Ok(ObservedMessage {
                    seq: seq as u64,
                    message,
                })
            })
            .collect()
        })
    }
    fn after(
        &self,
        conversation: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<ObservedMessage>, String> {
        self.run(|db| {
            let mut q = db
                .prepare("SELECT seq,payload FROM observations WHERE conversation=?1 AND seq>?2 ORDER BY seq LIMIT ?3")
                .map_err(|_| BROKEN)?;
            let rows = q
                .query_map(
                    params![conversation, after_seq as i64, limit.min(1000) as i64],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .map_err(|_| BROKEN)?;
            rows.map(|row| {
                let (seq, payload) = row.map_err(|_| BROKEN)?;
                let message: GroupMessage = serde_json::from_str(&payload).map_err(|_| BROKEN)?;
                if seq <= 0 || message.conversation != conversation {
                    return Err(BROKEN.to_string());
                }
                Ok(ObservedMessage {
                    seq: seq as u64,
                    message,
                })
            })
            .collect()
        })
    }
    fn exchanges(&self, conversation: &str) -> Result<Vec<GroupExchange>, String> {
        self.run(|db| {
            let mut q = db.prepare("SELECT a.payload,b.payload FROM exchanges x JOIN observations a ON a.conversation=x.conversation AND a.id=x.source JOIN observations b ON b.conversation=x.conversation AND b.id=x.reply WHERE x.conversation=?1 ORDER BY x.seq").map_err(|_|BROKEN)?;
            let rows = q.query_map([conversation], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(|_|BROKEN)?;
            rows.map(|row| {
                let (source,reply)=row.map_err(|_|BROKEN)?;
                let exchange=GroupExchange {source:serde_json::from_str(&source).map_err(|_|BROKEN)?,reply:serde_json::from_str(&reply).map_err(|_|BROKEN)?};
                if !exchange.valid(conversation) { return Err(BROKEN.into()); }
                Ok(exchange)
            }).collect()
        })
    }
    fn message(&self, conversation: &str, id: &str) -> Result<Option<GroupMessage>, String> {
        self.run(|db| {
            let raw: Option<String> = db
                .query_row(
                    "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                    params![conversation, id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| BROKEN)?;
            raw.map(|v| checked_message(&v, conversation, id))
                .transpose()
        })
    }
    fn link_reply(&self, conversation: &str, source: &str, reply: &str) -> Result<(), String> {
        self.run(|db| {
            let tx = db.transaction().map_err(|_| BROKEN)?;
            let get = |id: &str| -> Result<GroupMessage, String> {
                let raw: String = tx
                    .query_row(
                        "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                        params![conversation, id],
                        |r| r.get(0),
                    )
                    .map_err(|_| BROKEN)?;
                checked_message(&raw, conversation, id)
            };
            if !(GroupExchange {
                source: get(source)?,
                reply: get(reply)?,
            })
            .valid(conversation)
            {
                return Err("问答关联来源无效".into());
            }
            let old: Option<String> = tx
                .query_row(
                    "SELECT source FROM exchanges WHERE conversation=?1 AND reply=?2",
                    params![conversation, reply],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| BROKEN)?;
            if old.as_ref().is_some_and(|v| v != source) {
                return Err("问答关联冲突；保留原记录".into());
            }
            tx.execute(
                "INSERT OR IGNORE INTO exchanges(conversation,source,reply) VALUES(?1,?2,?3)",
                params![conversation, source, reply],
            )
            .map_err(|_| BROKEN)?;
            tx.commit().map_err(|_| BROKEN)?;
            Ok(())
        })
    }
    fn queue(&self, id: &str, reply: &ChimeReply, speaker: &str) -> Result<(), String> {
        if id.len() > 256
            || !id.starts_with("chime-")
            || reply.text.trim().is_empty()
            || reply.text.len() > 32768
            || reply.parts.is_empty()
            || reply.parts.len() > 3
            || speaker.is_empty()
            || speaker.len() > 128
            || !speaker
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("群回复参数无效".into());
        }
        let value = GroupOutbound {
            id: id.into(),
            reply: reply.clone(),
            speaker: speaker.into(),
        };
        let payload = serde_json::to_string(&value).map_err(|_| BROKEN)?;
        self.run(|db|{let tx=db.transaction().map_err(|_|BROKEN)?;
   let source:Option<String>=tx.query_row("SELECT payload FROM observations WHERE conversation=?1 AND id=?2",params![reply.conversation,reply.reply_to],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
   let m=checked_message(&source.ok_or("群回复没有原始观察来源")?,&reply.conversation,&reply.reply_to)?;
   if m.speaker!=speaker {return Err("群回复发送者与来源不符".into());}
   if tx.query_row("SELECT count(*) FROM outbound WHERE conversation=?1 AND reply_to=?2",params![reply.conversation,reply.reply_to],|r|r.get::<_,u64>(0)).map_err(|_|BROKEN)?!=0{return Err("这条群消息已有投递记录；不重发".into());}
   tx.execute("INSERT INTO outbound(id,conversation,reply_to,payload,status) VALUES(?1,?2,?3,?4,'queued')",params![id,reply.conversation,reply.reply_to,payload]).map_err(|_|BROKEN)?;tx.commit().map_err(|_|BROKEN)?;Ok(())})
    }
    fn claim(&self) -> Result<Option<GroupOutbound>, String> {
        self.run(|db| {
            let tx = db.transaction().map_err(|_| BROKEN)?;
            let value: Option<(String, String, String, String)> = tx
                .query_row(
                    "SELECT id,conversation,reply_to,payload FROM outbound WHERE status='queued' ORDER BY seq LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(|_| BROKEN)?;
            let Some((id, conversation, reply_to, raw)) = value else {
                return Ok(None);
            };
            let value = checked_outbound(raw, &id, &conversation, &reply_to)?;
            tx.execute(
                "UPDATE outbound SET status='sending' WHERE id=?1 AND status='queued'",
                [id],
            )
            .map_err(|_| BROKEN)?;
            tx.commit().map_err(|_| BROKEN)?;
            Ok(Some(value))
        })
    }
    fn finish(&self, id: &str, sent: bool) -> Result<(), String> {
        self.run(|db| {
            let tx = db.transaction().map_err(|_| BROKEN)?;
            if sent {
                let (conversation, reply_to, raw): (String, String, String) = tx
                    .query_row(
                        "SELECT conversation,reply_to,payload FROM outbound WHERE id=?1 AND status='sending'",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .map_err(|_| "群回复结局无效；原记录保留")?;
                let outbound = checked_outbound(raw, id, &conversation, &reply_to)?;
                let original: String = tx
                    .query_row(
                        "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                        params![outbound.reply.conversation, outbound.reply.reply_to],
                        |r| r.get(0),
                    )
                    .map_err(|_| BROKEN)?;
                let original = checked_message(&original, &conversation, &reply_to)?;
                let own = GroupMessage {
                    id: format!("{id}:reply"),
                    conversation: outbound.reply.conversation.clone(),
                    speaker: "eve-chime".into(),
                    speaker_name: Some("Eve".into()),
                    text: outbound.reply.text,
                    mentions_me: false,
                    replies_to_me: false,
                    from_me: true,
                    at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(1, |v| v.as_millis() as u64)
                        .max(original.at_ms.saturating_add(1)),
                };
                let payload = serde_json::to_string(&own).map_err(|_| BROKEN)?;
                tx.execute(
                    "INSERT INTO observations(conversation,id,payload) VALUES(?1,?2,?3)",
                    params![own.conversation, own.id, payload],
                )
                .map_err(|_| BROKEN)?;
                tx.execute(
                    "INSERT INTO exchanges(conversation,source,reply) VALUES(?1,?2,?3)",
                    params![own.conversation, outbound.reply.reply_to, own.id],
                )
                .map_err(|_| BROKEN)?;
            }
            if tx
                .execute(
                    "UPDATE outbound SET status=?2 WHERE id=?1 AND status='sending'",
                    params![id, if sent { "sent" } else { "failed" }],
                )
                .map_err(|_| BROKEN)?
                != 1
            {
                return Err("群回复结局无效；原记录保留".into());
            }
            tx.commit().map_err(|_| BROKEN)?;
            Ok(())
        })
    }
    fn finish_unknown(&self, id: &str) -> Result<(), String> {
        self.run(|db| {
            let changed = db
                .execute(
                    "UPDATE outbound SET status='unknown' WHERE id=?1 AND status='sending'",
                    [id],
                )
                .map_err(|_| BROKEN)?;
            if changed != 1 {
                return Err("群回复结局无效；原记录保留".into());
            }
            Ok(())
        })
    }
    fn abandon(&self, id: &str) -> Result<(), String> {
        self.run(|db|{db.execute("UPDATE outbound SET status='unknown' WHERE id=?1 AND status IN ('queued','sending')",[id]).map_err(|_|BROKEN)?;Ok(())})
    }
    fn outbound(&self, id: &str) -> Result<Option<GroupOutbound>, String> {
        self.run(|db| {
            db.query_row(
                "SELECT conversation,reply_to,payload FROM outbound WHERE id=?1 AND status='sending'",
                [id],
                |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)),
            )
            .optional()
            .map_err(|_| BROKEN)?
            .map(|(conversation, reply_to, raw)| checked_outbound(raw, id, &conversation, &reply_to))
            .transpose()
        })
    }
}
