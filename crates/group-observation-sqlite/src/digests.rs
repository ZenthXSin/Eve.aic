//! 群消息摘要库：每段摘要覆盖一段连续的观察序号，追加后不改写。
//! 与群观察库分开保存；损坏、缺失或未知格式拒绝启用并保留现场，不自动清空重建。
use eve_chime_api::{GroupDigest, GroupDigestStore};
use rusqlite::{Connection, params};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const BROKEN: &str = "群消息摘要库不可用；原记录保留，停止本次整理";

pub struct SqliteGroupDigests {
    db: Mutex<Connection>,
    healthy: AtomicBool,
}

impl SqliteGroupDigests {
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
            return Err("群消息摘要库缺失；保留现场，请恢复原数据库".into());
        }
        let db = Connection::open(path).map_err(|_| BROKEN)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|_| BROKEN)?;
        if !exists {
            db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE metadata(version INTEGER NOT NULL); INSERT INTO metadata VALUES(1); CREATE TABLE digests(conversation TEXT NOT NULL,from_seq INTEGER NOT NULL,to_seq INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(conversation,to_seq)); COMMIT;")
                .map_err(|_| BROKEN)?;
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
        if versions != [1] || check != "ok" {
            return Err(BROKEN.into());
        }
        db.prepare("SELECT conversation,from_seq,to_seq,payload FROM digests LIMIT 0")
            .map_err(|_| BROKEN)?;
        db.pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| BROKEN)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| BROKEN)?;
        if !marker.exists() {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&marker)
                .map_err(|_| BROKEN)?;
            file.write_all(b"group-digests-v1\n")
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

fn cursor_of(db: &Connection, conversation: &str) -> Result<u64, String> {
    let max: Option<i64> = db
        .query_row(
            "SELECT MAX(to_seq) FROM digests WHERE conversation=?1",
            [conversation],
            |r| r.get(0),
        )
        .map_err(|_| BROKEN)?;
    Ok(max.unwrap_or(0).max(0) as u64)
}

impl GroupDigestStore for SqliteGroupDigests {
    fn cursor(&self, conversation: &str) -> Result<u64, String> {
        self.run(|db| cursor_of(db, conversation))
    }

    fn latest(&self, conversation: &str, limit: usize) -> Result<Vec<GroupDigest>, String> {
        self.run(|db| {
            let mut q = db
                .prepare("SELECT from_seq,to_seq,payload FROM digests WHERE conversation=?1 ORDER BY to_seq DESC LIMIT ?2")
                .map_err(|_| BROKEN)?;
            let rows = q
                .query_map(params![conversation, limit.min(100) as i64], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
                })
                .map_err(|_| BROKEN)?;
            let mut digests = rows
                .map(|row| {
                    let (from, to, payload) = row.map_err(|_| BROKEN)?;
                    let digest: GroupDigest =
                        serde_json::from_str(&payload).map_err(|_| BROKEN)?;
                    if !digest.valid()
                        || digest.conversation != conversation
                        || digest.from_seq as i64 != from
                        || digest.to_seq as i64 != to
                    {
                        return Err(BROKEN.to_string());
                    }
                    Ok(digest)
                })
                .collect::<Result<Vec<_>, String>>()?;
            digests.reverse();
            Ok(digests)
        })
    }

    fn append(&self, digest: &GroupDigest) -> Result<(), String> {
        if !digest.valid() {
            return Err("摘要格式无效，未保存".into());
        }
        self.run(|db| {
            let tx = db.transaction().map_err(|_| BROKEN)?;
            if digest.from_seq <= cursor_of(&tx, &digest.conversation)? {
                return Err("这段消息已经整理过，未重复保存".into());
            }
            let payload = serde_json::to_string(digest).map_err(|_| BROKEN)?;
            tx.execute(
                "INSERT INTO digests(conversation,from_seq,to_seq,payload) VALUES(?1,?2,?3,?4)",
                params![
                    digest.conversation,
                    digest.from_seq as i64,
                    digest.to_seq as i64,
                    payload
                ],
            )
            .map_err(|_| BROKEN)?;
            tx.commit().map_err(|_| BROKEN.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(from: u64, to: u64, summary: &str) -> GroupDigest {
        GroupDigest {
            conversation: "g".into(),
            from_seq: from,
            to_seq: to,
            messages: (to - from + 1) as u32,
            first_at_ms: 1,
            last_at_ms: 2,
            summary: summary.into(),
        }
    }

    #[test]
    fn digests_append_after_cursor_survive_restart_and_refuse_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("group-digests.sqlite3");
        let store = SqliteGroupDigests::open(&path).unwrap();
        assert_eq!(store.cursor("g").unwrap(), 0);
        store.append(&digest(1, 5, "第一段")).unwrap();
        store.append(&digest(6, 9, "第二段")).unwrap();
        assert!(store.append(&digest(9, 12, "重叠")).is_err());
        assert!(store.append(&digest(10, 12, "")).is_err());
        assert_eq!(store.cursor("other").unwrap(), 0);
        drop(store);
        let store = SqliteGroupDigests::open(&path).unwrap();
        assert_eq!(store.cursor("g").unwrap(), 9);
        let latest = store.latest("g", 1).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].summary, "第二段");
        assert_eq!(store.latest("g", 10).unwrap()[0].summary, "第一段");
        drop(store);
        std::fs::remove_file(&path).unwrap();
        assert!(
            SqliteGroupDigests::open(&path)
                .err()
                .unwrap()
                .contains("缺失")
        );
        std::fs::write(&path, b"not sqlite").unwrap();
        assert!(SqliteGroupDigests::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not sqlite");
    }
}
