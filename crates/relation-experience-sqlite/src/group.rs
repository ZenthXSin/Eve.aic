//! Jev 群话题经验和待判定池；不与在途任务分类共享样例。
use eve_chime_api::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex, time::Duration};
const ERROR: &str = "Jev 群话题数据库不可用；保留记录，不补发历史消息";
pub struct SqliteGroupRelations {
    db: Mutex<Connection>,
}
impl SqliteGroupRelations {
    pub fn open(path: &Path) -> Result<Self, String> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(ERROR.into());
        }
        let existed = path.exists();
        let marker = path.with_extension("required");
        if marker.exists() && !existed {
            return Err(ERROR.into());
        }
        let db = Connection::open(path).map_err(|_| ERROR)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| ERROR)?;
        if !existed {
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE metadata(version INTEGER NOT NULL); INSERT INTO metadata VALUES(1);
                CREATE TABLE judgments(seq INTEGER PRIMARY KEY,conversation TEXT NOT NULL,message_id TEXT NOT NULL,message TEXT NOT NULL,decision TEXT NOT NULL,status TEXT NOT NULL,active TEXT NOT NULL,reviewed_revision TEXT NOT NULL DEFAULT '',UNIQUE(conversation,message_id));
                CREATE INDEX pending_group ON judgments(status,conversation,seq);
                CREATE TABLE experiences(seq INTEGER PRIMARY KEY,conversation TEXT NOT NULL,speaker TEXT NOT NULL,message_id TEXT NOT NULL,payload TEXT NOT NULL,UNIQUE(conversation,message_id));
                CREATE VIRTUAL TABLE group_search USING fts5(terms);
                COMMIT;").map_err(|_|ERROR)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| ERROR)?;
            }
        }
        let version: Vec<u64> = db
            .prepare("SELECT version FROM metadata")
            .map_err(|_| ERROR)?
            .query_map([], |r| r.get(0))
            .map_err(|_| ERROR)?
            .collect::<Result<_, _>>()
            .map_err(|_| ERROR)?;
        let check: String = db
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|_| ERROR)?;
        if version != [1] || check != "ok" {
            return Err(ERROR.into());
        }
        for sql in [
            "SELECT seq,conversation,message_id,message,decision,status,active,reviewed_revision FROM judgments LIMIT 0",
            "SELECT seq,conversation,speaker,message_id,payload FROM experiences LIMIT 0",
            "SELECT rowid,terms FROM group_search LIMIT 0",
        ] {
            db.prepare(sql).map_err(|_| ERROR)?;
        }
        db.pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| ERROR)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| ERROR)?;
        // 保留待整理记录；启动不能因积压消息自动补发。
        db.execute("UPDATE judgments SET status='pending',active='archived' WHERE status='judging' OR active='waiting'",[]).map_err(|_|ERROR)?;
        if !marker.exists() {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker)
                .map_err(|_| ERROR)?;
            f.write_all(b"jev-group-experience-v1\n")
                .and_then(|()| f.sync_all())
                .map_err(|_| ERROR)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)
                    .and_then(|f| f.sync_all())
                    .map_err(|_| ERROR)?;
            }
        }
        Ok(Self { db: Mutex::new(db) })
    }
}
fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, String> {
    serde_json::from_str(raw).map_err(|_| ERROR.into())
}
impl GroupRelationStore for SqliteGroupRelations {
    fn suppress_active(&self, m: &GroupMessage) -> Result<(), String> {
        self.db
            .lock()
            .map_err(|_| ERROR)?
            .execute(
                "UPDATE judgments SET active='none' WHERE conversation=?1 AND message_id=?2",
                params![m.conversation, m.id],
            )
            .map_err(|_| ERROR)?;
        Ok(())
    }
    fn stage(&self, m: &GroupMessage) -> Result<(), String> {
        if !m.valid() || m.from_me || m.text.len() > 32768 {
            return Err("群关系输入无效".into());
        }
        let raw = serde_json::to_string(m).map_err(|_| ERROR)?;
        let decision = serde_json::to_string(&GroupRelationDecision::uncertain("判断排队"))
            .map_err(|_| ERROR)?;
        let db = self.db.lock().map_err(|_| ERROR)?;
        let old: Option<String> = db
            .query_row(
                "SELECT message FROM judgments WHERE conversation=?1 AND message_id=?2",
                params![m.conversation, m.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| ERROR)?;
        if old.as_ref().is_some_and(|v| v != &raw) {
            return Err("群关系消息冲突；保留原记录".into());
        }
        db.execute("INSERT OR IGNORE INTO judgments(conversation,message_id,message,decision,status,active) VALUES(?1,?2,?3,?4,'judging','none')",params![m.conversation,m.id,raw,decision]).map_err(|_|ERROR)?;
        db.execute("UPDATE judgments SET status='judging',active='none' WHERE conversation=?1 AND message_id=?2 AND status='pending'",params![m.conversation,m.id]).map_err(|_|ERROR)?;
        Ok(())
    }
    fn record(&self, m: &GroupMessage, d: &GroupRelationDecision) -> Result<(), String> {
        if !m.valid() || m.from_me || m.text.len() > 32768 || d.confidence > 100 {
            return Err("群关系输入无效".into());
        }
        let raw = serde_json::to_string(m).map_err(|_| ERROR)?;
        let decision = serde_json::to_string(d).map_err(|_| ERROR)?;
        let db = self.db.lock().map_err(|_| ERROR)?;
        let old: Option<String> = db
            .query_row(
                "SELECT message FROM judgments WHERE conversation=?1 AND message_id=?2",
                params![m.conversation, m.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| ERROR)?;
        let (status, active) = if d.certain() {
            ("resolved", "none")
        } else {
            ("pending", "waiting")
        };
        if let Some(old) = old {
            if old != raw {
                return Err("群关系消息冲突；保留原记录".into());
            }
            db.execute("UPDATE judgments SET decision=?3,status=?4,active=?5 WHERE conversation=?1 AND message_id=?2 AND status IN ('judging','pending')",params![m.conversation,m.id,decision,status,active]).map_err(|_|ERROR)?;
            return Ok(());
        }
        db.execute("INSERT INTO judgments(conversation,message_id,message,decision,status,active) VALUES(?1,?2,?3,?4,?5,?6)",params![m.conversation,m.id,raw,decision,status,active]).map_err(|_|ERROR)?;
        Ok(())
    }
    fn recall(&self, m: &GroupMessage) -> Result<Vec<GroupRelationExample>, String> {
        let grams = super::grams(&m.text);
        if grams.is_empty() {
            return Ok(vec![]);
        }
        let query = grams
            .iter()
            .take(128)
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let db = self.db.lock().map_err(|_| ERROR)?;
        let mut q=db.prepare("SELECT e.payload FROM group_search JOIN experiences e ON e.seq=group_search.rowid WHERE group_search MATCH ?1 AND e.conversation=?2 ORDER BY bm25(group_search),e.seq DESC LIMIT 4").map_err(|_|ERROR)?;
        q.query_map(params![query, m.conversation], |r| r.get::<_, String>(0))
            .map_err(|_| ERROR)?
            .map(|row| {
                let e: GroupRelationExample = decode(&row.map_err(|_| ERROR)?)?;
                if e.exchange
                    .as_ref()
                    .is_some_and(|x| !x.valid(&m.conversation))
                {
                    return Err(ERROR.into());
                }
                Ok(e)
            })
            .collect()
    }
    fn claim_active(&self) -> Result<Option<GroupMessage>, String> {
        let mut db = self.db.lock().map_err(|_| ERROR)?;
        let tx = db.transaction().map_err(|_| ERROR)?;
        // 同群合成一批，只考虑最后一条；其余仍留在经验待整理池。
        let row:Option<(u64,String,String)>=tx.query_row("SELECT seq,conversation,message FROM judgments WHERE active='waiting' ORDER BY seq LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_|ERROR)?;
        let Some((_, conversation, _)) = row else {
            return Ok(None);
        };
        let (seq,raw):(u64,String)=tx.query_row("SELECT seq,message FROM judgments WHERE conversation=?1 AND active='waiting' ORDER BY seq DESC LIMIT 1",[&conversation],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|ERROR)?;
        let m: GroupMessage = decode(&raw)?;
        if !m.valid() || m.conversation != conversation {
            return Err(ERROR.into());
        }
        tx.execute("UPDATE judgments SET active='considered' WHERE conversation=?1 AND active='waiting' AND seq<=?2",params![conversation,seq]).map_err(|_|ERROR)?;
        tx.commit().map_err(|_| ERROR)?;
        Ok(Some(m))
    }
    fn pending_groups(&self) -> Result<Vec<String>, String> {
        let db = self.db.lock().map_err(|_| ERROR)?;
        let mut q=db.prepare("SELECT conversation FROM judgments WHERE status='pending' GROUP BY conversation ORDER BY MIN(seq) LIMIT 256").map_err(|_|ERROR)?;
        q.query_map([], |r| r.get(0))
            .map_err(|_| ERROR)?
            .collect::<Result<_, _>>()
            .map_err(|_| ERROR.into())
    }
    fn pending(
        &self,
        conversation: &str,
        revision: &str,
        limit: usize,
    ) -> Result<Vec<PendingGroupRelation>, String> {
        let db = self.db.lock().map_err(|_| ERROR)?;
        let mut q=db.prepare("SELECT message,active FROM judgments WHERE status='pending' AND conversation=?1 AND reviewed_revision!=?2 ORDER BY seq LIMIT ?3").map_err(|_|ERROR)?;
        q.query_map(params![conversation, revision, limit.min(16)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|_| ERROR)?
        .map(|row| {
            let (raw, active) = row.map_err(|_| ERROR)?;
            Ok(PendingGroupRelation {
                message: decode(&raw)?,
                active_considered: active != "waiting",
            })
        })
        .collect()
    }
    fn reviewed(
        &self,
        m: &GroupMessage,
        revision: &str,
        example: Option<&GroupRelationExample>,
    ) -> Result<(), String> {
        let mut db = self.db.lock().map_err(|_| ERROR)?;
        let tx = db.transaction().map_err(|_| ERROR)?;
        let raw:Option<String>=tx.query_row("SELECT message FROM judgments WHERE conversation=?1 AND message_id=?2 AND status='pending'",params![m.conversation,m.id],|r|r.get(0)).optional().map_err(|_|ERROR)?;
        let Some(raw) = raw else {
            return Ok(());
        };
        if decode::<GroupMessage>(&raw)? != *m {
            return Err(ERROR.into());
        }
        if let Some(e) = example {
            if e.message != m.text
                || !e.decision.certain()
                || e.decision.confidence < 85
                || e.evidence.is_empty()
                || e.exchange
                    .as_ref()
                    .is_some_and(|x| !x.valid(&m.conversation))
            {
                return Err("群关系经验无效".into());
            }
            let payload = serde_json::to_string(e).map_err(|_| ERROR)?;
            tx.execute("INSERT INTO experiences(conversation,speaker,message_id,payload) VALUES(?1,?2,?3,?4)",params![m.conversation,m.speaker,m.id,payload]).map_err(|_|ERROR)?;
            let seq = tx.last_insert_rowid();
            let terms = super::grams(&m.text)
                .into_iter()
                .collect::<Vec<_>>()
                .join(" ");
            tx.execute(
                "INSERT INTO group_search(rowid,terms) VALUES(?1,?2)",
                params![seq, terms],
            )
            .map_err(|_| ERROR)?;
        }
        tx.execute("UPDATE judgments SET reviewed_revision=?3,status=?4 WHERE conversation=?1 AND message_id=?2",params![m.conversation,m.id,revision,if example.is_some(){"resolved"}else{"pending"}]).map_err(|_|ERROR)?;
        tx.commit().map_err(|_| ERROR)?;
        Ok(())
    }
}
