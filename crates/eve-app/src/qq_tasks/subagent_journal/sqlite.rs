//! 独立 SQLite 冷执行账本；冻结资料只写一次，每步仅事务更新检查点列。
use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, time::Duration};

const DEFAULT_CAPACITY: u64 = 2 * 1024 * 1024 * 1024;
const RESERVE: usize = eve_llm_api::MAX_CHECKPOINT_BYTES + 128 * 1024;
pub(crate) struct SqliteRunStorage {
    connection: Mutex<Connection>,
    capacity: u64,
}
impl SqliteRunStorage {
    pub(crate) fn open(path: &Path) -> Result<Self, LlmError> {
        let capacity = std::env::var("EVE_SUBAGENT_STORAGE_MAX_BYTES")
            .ok()
            .map(|s| s.parse::<u64>().map_err(|_| storage()))
            .transpose()?
            .unwrap_or(DEFAULT_CAPACITY);
        if !(64 * 1024 * 1024..=64 * 1024 * 1024 * 1024).contains(&capacity) {
            return Err(storage());
        }
        if path.symlink_metadata().is_ok_and(|m| !m.is_file()) {
            return Err(storage());
        }
        let exists = path.exists();
        if exists && path.metadata().map_err(|_| storage())?.len() == 0 {
            return Err(storage());
        }
        let connection = Connection::open(path).map_err(|_| storage())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|_| storage())?;
        if exists {
            let version: i64 = connection
                .query_row("SELECT version FROM metadata", [], |r| r.get(0))
                .map_err(|_| storage())?;
            if version != 1 {
                return Err(storage());
            }
            let check: String = connection
                .query_row("PRAGMA quick_check", [], |r| r.get(0))
                .map_err(|_| storage())?;
            if check != "ok" {
                return Err(storage());
            }
        }
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|_| storage())?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|_| storage())?;
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| storage())?;
        let page_size: u64 = connection
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(|_| storage())?;
        connection
            .pragma_update(None, "max_page_count", capacity / page_size)
            .map_err(|_| storage())?;
        if !exists {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE metadata(version INTEGER NOT NULL);
                INSERT INTO metadata VALUES(1);
                CREATE TABLE runs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,session_id TEXT NOT NULL,user_id TEXT NOT NULL,entry BLOB NOT NULL,frozen BLOB NOT NULL);
                CREATE TABLE run_states(id TEXT PRIMARY KEY REFERENCES runs(id),state BLOB NOT NULL,reserved INTEGER NOT NULL);
                CREATE INDEX runs_owner ON runs(session_id,user_id,seq DESC);
                COMMIT;").map_err(|_|storage())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| storage())?;
            }
        }
        connection
            .prepare(
                "SELECT r.seq,r.id,r.session_id,r.user_id,r.entry,r.frozen,s.state,s.reserved FROM runs r JOIN run_states s ON r.id=s.id LIMIT 0",
            )
            .map_err(|_| storage())?;
        let missing:i64=connection.query_row("SELECT (SELECT COUNT(*) FROM runs r LEFT JOIN run_states s ON r.id=s.id WHERE s.id IS NULL)+(SELECT COUNT(*) FROM run_states s LEFT JOIN runs r ON r.id=s.id WHERE r.id IS NULL)",[],|r|r.get(0)).map_err(|_|storage())?;
        if missing != 0 {
            return Err(storage());
        }
        {
            let mut statement = connection
                .prepare("SELECT r.id,r.session_id,r.user_id,r.entry,r.frozen,s.state FROM runs r JOIN run_states s ON r.id=s.id")
                .map_err(|_| storage())?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                })
                .map_err(|_| storage())?;
            for row in rows {
                let (id, session_id, user_id, entry, frozen, state) = row.map_err(|_| storage())?;
                if frozen.len().saturating_add(state.len()) > MAX_RECORD_BYTES {
                    return Err(storage());
                }
                let frozen: FrozenRun = serde_json::from_slice(&frozen).map_err(|_| storage())?;
                let entry: Entry = serde_json::from_slice(&entry).map_err(|_| storage())?;
                if entry.id != id
                    || entry.owner != frozen.owner
                    || frozen.owner.session_id != session_id
                    || frozen.owner.user_id != user_id
                    || run_id(&frozen)? != id
                {
                    return Err(storage());
                }
                let state: SavedState = serde_json::from_slice(&state).map_err(|_| storage())?;
                Record {
                    version: state.version,
                    frozen,
                    checkpoint: state.checkpoint,
                    end: state.end,
                    usage: state.usage,
                    verification: state.verification,
                    finished_at_ms: state.finished_at_ms,
                }
                .validate()?;
            }
        }
        Ok(Self {
            connection: Mutex::new(connection),
            capacity,
        })
    }
}
impl RunStorage for SqliteRunStorage {
    fn create(
        &self,
        id: &str,
        owner: &TaskOwner,
        entry: &[u8],
        frozen: &[u8],
        state: &[u8],
    ) -> Result<bool, LlmError> {
        let mut connection = self.connection.lock().map_err(|_| storage())?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| storage())?;
        if tx
            .query_row("SELECT 1 FROM runs WHERE id=?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .map_err(|_| storage())?
            .is_some()
        {
            return Ok(false);
        }
        let used: u64 = tx
            .query_row(
                "SELECT COALESCE(SUM(reserved),0) FROM run_states",
                [],
                |r| r.get(0),
            )
            .map_err(|_| storage())?;
        let reserved = frozen
            .len()
            .checked_add(entry.len())
            .and_then(|n| n.checked_add(RESERVE + 4096))
            .ok_or_else(storage)?;
        if used.saturating_add(reserved as u64) > self.capacity {
            return Err(LlmError::Context(
                "子代理冷存储容量不足；旧记录保留，未开始新任务".into(),
            ));
        }
        tx.execute(
            "INSERT INTO runs(id,session_id,user_id,entry,frozen) VALUES(?1,?2,?3,?4,?5)",
            params![id, owner.session_id, owner.user_id, entry, frozen],
        )
        .map_err(|_| storage())?;
        tx.execute(
            "INSERT INTO run_states(id,state,reserved) VALUES(?1,?2,?3)",
            params![id, state, reserved as u64],
        )
        .map_err(|_| storage())?;
        tx.commit().map_err(|_| storage())?;
        Ok(true)
    }
    fn read(&self, id: &str) -> Result<Option<StoredRun>, LlmError> {
        self.connection
            .lock()
            .map_err(|_| storage())?
            .query_row(
                "SELECT r.frozen,s.state FROM runs r JOIN run_states s ON r.id=s.id WHERE r.id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| storage())
    }
    fn update(&self, id: &str, state: &[u8]) -> Result<(), LlmError> {
        let saved: SavedState = serde_json::from_slice(state).map_err(|_| storage())?;
        let mut connection = self.connection.lock().map_err(|_| storage())?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| storage())?;
        let count=if saved.end.is_some() {
            tx.execute("UPDATE run_states SET state=?2,reserved=(SELECT length(frozen)+length(entry) FROM runs WHERE id=?1)+length(?2)+4096 WHERE id=?1",params![id,state])
        } else { tx.execute("UPDATE run_states SET state=?2 WHERE id=?1",params![id,state]) }.map_err(|_|storage())?;
        if count != 1 {
            return Err(storage());
        }
        tx.commit().map_err(|_| storage())
    }
    fn list(
        &self,
        owner: &TaskOwner,
        id: Option<&str>,
        offset: usize,
    ) -> Result<Vec<Vec<u8>>, LlmError> {
        let connection = self.connection.lock().map_err(|_| storage())?;
        let mut statement=connection.prepare("SELECT entry FROM runs WHERE session_id=?1 AND user_id=?2 AND (?3 IS NULL OR id=?3) ORDER BY seq DESC LIMIT 20 OFFSET ?4").map_err(|_|storage())?;
        statement
            .query_map(
                params![owner.session_id, owner.user_id, id, offset],
                |row| row.get(0),
            )
            .map_err(|_| storage())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| storage())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkpoint_sql_failure_rolls_back_without_changing_frozen_or_previous_state() {
        let directory = tempfile::tempdir().unwrap();
        let backend = SqliteRunStorage::open(&directory.path().join("runs.sqlite3")).unwrap();
        let owner = TaskOwner {
            session_id: "s".into(),
            user_id: "u".into(),
        };
        let initial = serde_json::to_vec(&SavedState {
            version: 1,
            checkpoint: None,
            end: None,
            usage: None,
            verification: None,
            finished_at_ms: None,
        })
        .unwrap();
        assert!(
            backend
                .create("id", &owner, b"entry", b"frozen", &initial)
                .unwrap()
        );
        backend.connection.lock().unwrap().execute_batch("CREATE TRIGGER fail_update BEFORE UPDATE ON run_states BEGIN SELECT RAISE(ABORT,'test write failure'); END;").unwrap();
        assert!(backend.update("id", &initial).is_err());
        assert_eq!(
            backend.read("id").unwrap().unwrap(),
            (b"frozen".to_vec(), initial)
        );
    }
    #[test]
    fn completed_rows_release_reservation_and_remain_available_after_128_runs() {
        let directory = tempfile::tempdir().unwrap();
        let backend = SqliteRunStorage::open(&directory.path().join("runs.sqlite3")).unwrap();
        let owner = TaskOwner {
            session_id: "s".into(),
            user_id: "u".into(),
        };
        let state = SavedState {
            version: 1,
            checkpoint: None,
            end: None,
            usage: None,
            verification: None,
            finished_at_ms: None,
        };
        let initial = serde_json::to_vec(&state).unwrap();
        let ended = serde_json::to_vec(&SavedState {
            end: Some(End::Failed {
                reason: "test".into(),
            }),
            usage: Some(TaskUsage::default()),
            finished_at_ms: Some(1),
            ..state
        })
        .unwrap();
        for index in 0..140 {
            let id = format!("{index:064x}");
            assert!(
                backend
                    .create(&id, &owner, id.as_bytes(), b"frozen", &initial)
                    .unwrap()
            );
            backend.update(&id, &ended).unwrap();
        }
        assert_eq!(backend.list(&owner, None, 120).unwrap().len(), 20);
        assert_eq!(backend.list(&owner, None, 140).unwrap().len(), 0);
        let used: u64 = backend
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT SUM(reserved) FROM run_states", [], |r| r.get(0))
            .unwrap();
        assert!(used < 1024 * 1024);
    }
}
