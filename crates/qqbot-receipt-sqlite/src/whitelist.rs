//! 群白名单：只有被放行的群，群里非主人本人的消息才会进入通道。
//! 群号由通道从当前消息的真实群号取得，不由模型填写。
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::QqGroupWhitelist;
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex, time::Duration};

pub struct SqliteGroupWhitelist(Mutex<Connection>);

fn unavailable() -> PluginError {
    PluginError::State("QQ 群白名单数据库不可用；保留原库，不重建或清空".into())
}

/// 群号或用户号只接受平台常见的数字标识；不合法一律拒绝，不写入。
fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl SqliteGroupWhitelist {
    pub fn open(path: &Path) -> PluginResult<Self> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(unavailable());
        }
        // 打开连接会建出 0 字节文件；首次建表提交前中断留下的空文件按未初始化处理。
        let exists = path.metadata().is_ok_and(|m| m.len() > 0);
        let connection = Connection::open(path).map_err(|_| unavailable())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| unavailable())?;
        if !exists {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                    CREATE TABLE groups (group_id TEXT PRIMARY KEY, added_at INTEGER NOT NULL);
                    COMMIT;",
                )
                .map_err(|_| unavailable())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| unavailable())?;
            }
        }
        // 格式版本与本模块一致才继续；未知结构不清空，直接拒绝。
        let version: Option<i64> = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .optional()
            .map_err(|_| unavailable())?;
        if version.unwrap_or(0) > 1 {
            return Err(unavailable());
        }
        let _ = connection.pragma_update(None, "user_version", 1);
        Ok(Self(Mutex::new(connection)))
    }

    fn contains(&self, group_id: &str) -> PluginResult<bool> {
        if !valid_identity(group_id) {
            return Ok(false);
        }
        self.0
            .lock()
            .map_err(|_| unavailable())?
            .query_row(
                "SELECT 1 FROM groups WHERE group_id=?1",
                params![group_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(|_| unavailable())
    }

    fn add(&self, group_id: &str, at_ms: u64) -> PluginResult<bool> {
        if !valid_identity(group_id) {
            return Err(PluginError::State("群号不合法，白名单未修改".into()));
        }
        let at = i64::try_from(at_ms).unwrap_or(i64::MAX);
        self.0
            .lock()
            .map_err(|_| unavailable())?
            .execute(
                "INSERT OR IGNORE INTO groups(group_id,added_at) VALUES (?1,?2)",
                params![group_id, at],
            )
            .map(|n| n == 1)
            .map_err(|_| unavailable())
    }

    fn remove(&self, group_id: &str) -> PluginResult<bool> {
        if !valid_identity(group_id) {
            return Ok(false);
        }
        self.0
            .lock()
            .map_err(|_| unavailable())?
            .execute("DELETE FROM groups WHERE group_id=?1", params![group_id])
            .map(|n| n == 1)
            .map_err(|_| unavailable())
    }

    fn list(&self) -> PluginResult<Vec<String>> {
        let connection = self.0.lock().map_err(|_| unavailable())?;
        let mut statement = connection
            .prepare("SELECT group_id FROM groups ORDER BY added_at, group_id")
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|_| unavailable())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|_| unavailable())
    }
}

impl QqGroupWhitelist for SqliteGroupWhitelist {
    fn contains(&self, group_id: &str) -> PluginResult<bool> {
        Self::contains(self, group_id)
    }
    fn add(&self, group_id: &str, at_ms: u64) -> PluginResult<bool> {
        Self::add(self, group_id, at_ms)
    }
    fn remove(&self, group_id: &str) -> PluginResult<bool> {
        Self::remove(self, group_id)
    }
    fn list(&self) -> PluginResult<Vec<String>> {
        Self::list(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_database_admits_nothing_and_add_remove_persist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("whitelist.sqlite3");
        let db = SqliteGroupWhitelist::open(&path).unwrap();
        assert!(!QqGroupWhitelist::contains(&db, "123").unwrap());
        assert!(QqGroupWhitelist::add(&db, "123", 1).unwrap());
        assert!(!QqGroupWhitelist::add(&db, "123", 2).unwrap());
        assert!(QqGroupWhitelist::contains(&db, "123").unwrap());
        assert_eq!(
            QqGroupWhitelist::list(&db).unwrap(),
            vec!["123".to_string()]
        );
        let reopened = SqliteGroupWhitelist::open(&path).unwrap();
        assert!(QqGroupWhitelist::contains(&reopened, "123").unwrap());
        assert!(QqGroupWhitelist::remove(&reopened, "123").unwrap());
        assert!(!QqGroupWhitelist::contains(&reopened, "123").unwrap());
    }

    #[test]
    fn invalid_identity_is_rejected_without_touching_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("whitelist.sqlite3");
        let db = SqliteGroupWhitelist::open(&path).unwrap();
        for bad in ["", "abc", "12 3", "１２３", "<script>"] {
            assert!(
                QqGroupWhitelist::add(&db, bad, 1).is_err(),
                "{bad} 应被拒绝"
            );
            assert!(!QqGroupWhitelist::contains(&db, bad).unwrap());
        }
        assert!(QqGroupWhitelist::list(&db).unwrap().is_empty());
    }

    #[test]
    fn future_version_is_refused_and_bytes_kept() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("whitelist.sqlite3");
        drop(SqliteGroupWhitelist::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        assert!(SqliteGroupWhitelist::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
