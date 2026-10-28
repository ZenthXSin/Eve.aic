//! 单宿主 SQLite 冷回执库。事务先提交，宿主再缩小活跃账本；重复批次只核对原字节。
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqArchivedReceipt, QqReceiptArchive};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex, time::Duration};

mod whitelist;
pub use whitelist::SqliteGroupWhitelist;

pub struct SqliteReceiptArchive(Mutex<Connection>);
fn unavailable() -> PluginError {
    PluginError::State("QQ 回执归档数据库不可用；保留活跃记录，不重建或清空旧库".into())
}
impl SqliteReceiptArchive {
    pub fn open(path: &Path) -> PluginResult<Self> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(unavailable());
        }
        // 打开连接就会建出 0 字节文件；首次建表提交前中断会留下它。空文件里没有任何记录，
        // 按未初始化处理；已有内容的库照常核对，损坏或身份不符仍拒绝启动。
        let exists = path.metadata().is_ok_and(|m| m.len() > 0);
        let connection = Connection::open(path).map_err(|_| unavailable())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|_| unavailable())?;
        // DELETE 日志提交后只需迁移一个数据库文件；运行中备份仍须停宿主或使用 SQLite backup。
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| unavailable())?;
        if !exists {
            let mut random = [0u8; 16];
            SystemRandom::new()
                .fill(&mut random)
                .map_err(|_| unavailable())?;
            let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE metadata (version INTEGER NOT NULL, identity TEXT NOT NULL);
                CREATE TABLE receipts (seq INTEGER PRIMARY KEY, app TEXT NOT NULL, message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL, failed_reply INTEGER NOT NULL CHECK(failed_reply IN (0,1)),
                    payload BLOB NOT NULL, UNIQUE(app,message_id));
                CREATE INDEX failed_receipts ON receipts(app,session_id,failed_reply,seq DESC);")
                .map_err(|_| unavailable())?;
            connection
                .execute("INSERT INTO metadata VALUES (1,?1)", [&id])
                .map_err(|_| unavailable())?;
            connection
                .execute_batch("COMMIT")
                .map_err(|_| unavailable())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| unavailable())?;
            }
        }
        let check: String = connection
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|_| unavailable())?;
        if check != "ok" {
            return Err(unavailable());
        }
        let archive = Self(Mutex::new(connection));
        archive.identity()?;
        Ok(archive)
    }
}
impl QqReceiptArchive for SqliteReceiptArchive {
    fn identity(&self) -> PluginResult<(String, u64)> {
        let connection = self.0.lock().map_err(|_| unavailable())?;
        let rows: u64 = connection
            .query_row(
                "SELECT count(*) FROM metadata WHERE version=1 AND length(identity)=32",
                [],
                |r| r.get(0),
            )
            .map_err(|_| unavailable())?;
        if rows != 1 {
            return Err(unavailable());
        }
        let id = connection
            .query_row("SELECT identity FROM metadata", [], |r| r.get(0))
            .map_err(|_| unavailable())?;
        let count = connection
            .query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))
            .map_err(|_| unavailable())?;
        Ok((id, count))
    }
    fn store(&self, records: &[QqArchivedReceipt]) -> PluginResult<u64> {
        let mut connection = self.0.lock().map_err(|_| unavailable())?;
        let tx = connection.transaction().map_err(|_| unavailable())?;
        for record in records {
            if record.payload.len() > 512 * 1024 {
                return Err(unavailable());
            }
            tx.execute("INSERT OR IGNORE INTO receipts(app,message_id,session_id,failed_reply,payload) VALUES (?1,?2,?3,?4,?5)",
                params![record.app_id,record.message_id,record.session_id,record.failed_reply,record.payload]).map_err(|_| unavailable())?;
            let same: bool = tx.query_row("SELECT payload=?3 AND session_id=?4 AND failed_reply=?5 FROM receipts WHERE app=?1 AND message_id=?2",
                params![record.app_id,record.message_id,record.payload,record.session_id,record.failed_reply], |r| r.get(0)).map_err(|_| unavailable())?;
            if !same {
                return Err(unavailable());
            }
        }
        let count = tx
            .query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))
            .map_err(|_| unavailable())?;
        tx.commit().map_err(|_| unavailable())?;
        Ok(count)
    }
    fn get(&self, app: &str, message_id: &str) -> PluginResult<Option<Vec<u8>>> {
        self.0
            .lock()
            .map_err(|_| unavailable())?
            .query_row(
                "SELECT payload FROM receipts WHERE app=?1 AND message_id=?2",
                params![app, message_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| unavailable())
    }
    fn latest_failed(&self, app: &str, session_id: &str) -> PluginResult<Option<Vec<u8>>> {
        self.0.lock().map_err(|_| unavailable())?.query_row(
            "SELECT payload FROM receipts WHERE app=?1 AND session_id=?2 AND failed_reply=1 ORDER BY seq DESC LIMIT 1", params![app,session_id], |r| r.get(0))
            .optional().map_err(|_| unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, payload: &[u8]) -> QqArchivedReceipt {
        QqArchivedReceipt {
            app_id: "app".into(),
            message_id: id.into(),
            session_id: "scope".into(),
            failed_reply: true,
            payload: payload.to_vec(),
        }
    }
    #[test]
    fn transaction_is_idempotent_conflicts_roll_back_and_restart_retains_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipts.sqlite3");
        let db = SqliteReceiptArchive::open(&path).unwrap();
        let id = db.identity().unwrap().0;
        assert_eq!(db.store(&[record("one", b"original")]).unwrap(), 1);
        assert_eq!(db.store(&[record("one", b"original")]).unwrap(), 1);
        assert!(
            db.store(&[record("two", b"new"), record("one", b"modified")])
                .is_err()
        );
        assert_eq!(db.get("app", "two").unwrap(), None);
        assert_eq!(db.get("foreign", "one").unwrap(), None);
        assert_eq!(db.latest_failed("app", "foreign").unwrap(), None);
        assert_eq!(
            db.latest_failed("app", "scope").unwrap(),
            Some(b"original".to_vec())
        );
        drop(db);
        let db = SqliteReceiptArchive::open(&path).unwrap();
        assert_eq!(db.identity().unwrap(), (id, 1));
        assert_eq!(db.get("app", "one").unwrap(), Some(b"original".to_vec()));
    }
    #[test]
    fn empty_file_left_before_first_commit_is_initialized() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipts.sqlite3");
        std::fs::write(&path, b"").unwrap();
        let db = SqliteReceiptArchive::open(&path).unwrap();
        assert_eq!(db.identity().unwrap().1, 0);
    }
    #[test]
    fn corrupt_database_is_not_recreated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipts.sqlite3");
        std::fs::write(&path, b"broken database").unwrap();
        assert!(SqliteReceiptArchive::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken database");
    }
}
