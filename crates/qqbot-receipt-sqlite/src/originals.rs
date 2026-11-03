//! 平台原文独立于加工回执保存；缺口占据真实消息位置，数据库损坏不降级到加工正文。
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqOriginalBinding, QqOriginalMessage};
use eve_session_api::SessionKey;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

const ORIGINALS: &str = "CREATE TABLE qq_originals (
    seq INTEGER PRIMARY KEY,
    app TEXT NOT NULL,
    message_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    session_user_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('c2c','group')),
    target_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    text TEXT,
    UNIQUE(app,message_id)
) STRICT";
const METADATA: &str = "CREATE TABLE qq_original_metadata (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    version INTEGER NOT NULL CHECK(version=1),
    identity TEXT NOT NULL CHECK(length(identity)=32),
    committed_count INTEGER NOT NULL CHECK(committed_count>=0)
) STRICT";
const INDEX: &str =
    "CREATE INDEX qq_originals_session ON qq_originals(app,session_id,session_user_id,seq DESC)";
const NO_UPDATE: &str = "CREATE TRIGGER qq_originals_no_update BEFORE UPDATE ON qq_originals
    BEGIN SELECT RAISE(ABORT,'QQ original messages are immutable'); END";
const NO_DELETE: &str = "CREATE TRIGGER qq_originals_no_delete BEFORE DELETE ON qq_originals
    BEGIN SELECT RAISE(ABORT,'QQ original messages are immutable'); END";
const NO_REPLACE: &str = "CREATE TRIGGER qq_originals_no_replace BEFORE INSERT ON qq_originals
    WHEN EXISTS(SELECT 1 FROM qq_originals WHERE seq=NEW.seq OR (app=NEW.app AND message_id=NEW.message_id))
    BEGIN SELECT RAISE(ABORT,'QQ original messages are immutable'); END";

pub(super) fn unavailable() -> PluginError {
    PluginError::State("QQ 原文归档不可用；保留原库，拒绝使用缺失、冲突或损坏的授权证据".into())
}

/// user_version 只标记这个附加存储；原回执 metadata.version 保持 1。
/// 在一个事务中迁移，避免缺表被误识别成尚未启用的旧版本。
pub(super) fn initialize(connection: &mut Connection) -> PluginResult<()> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| unavailable())?;
    let version: u64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|_| unavailable())?;
    if version == 0 {
        let has_objects: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema
                 WHERE name LIKE 'qq_original%' OR tbl_name LIKE 'qq_original%')",
                [],
                |r| r.get(0),
            )
            .map_err(|_| unavailable())?;
        if has_objects {
            return Err(unavailable());
        }
        let id = receipt_identity(&tx)?;
        for ddl in [ORIGINALS, METADATA, INDEX, NO_UPDATE, NO_DELETE, NO_REPLACE] {
            tx.execute_batch(ddl).map_err(|_| unavailable())?;
        }
        tx.execute("INSERT INTO qq_original_metadata VALUES (1,1,?1,0)", [&id])
            .map_err(|_| unavailable())?;
        tx.pragma_update(None, "user_version", 1)
            .map_err(|_| unavailable())?;
    } else if version != 1 {
        return Err(unavailable());
    }
    identity(&tx)?;
    tx.commit().map_err(|_| unavailable())
}

fn receipt_identity(connection: &Connection) -> PluginResult<String> {
    let mut statement = connection
        .prepare("SELECT version,identity FROM metadata LIMIT 2")
        .map_err(|_| unavailable())?;
    let records = statement
        .query_map([], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)))
        .map_err(|_| unavailable())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unavailable())?;
    if let [(1, id)] = records.as_slice()
        && id.len() == 32
        && id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        Ok(id.clone())
    } else {
        Err(unavailable())
    }
}

/// 只检查固定数量的 schema 对象，不在启动时解码历史正文。
/// 完整 DDL 同时保护唯一键、严格列类型和不可修改触发器；同列视图不能冒充原表。
fn validate_schema(connection: &Connection) -> PluginResult<()> {
    let version: u64 = connection
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|_| unavailable())?;
    if version != 1 {
        return Err(unavailable());
    }
    let expected = [
        (
            "qq_original_metadata",
            "table",
            "qq_original_metadata",
            Some(METADATA),
        ),
        ("qq_originals", "table", "qq_originals", Some(ORIGINALS)),
        (
            "qq_originals_no_delete",
            "trigger",
            "qq_originals",
            Some(NO_DELETE),
        ),
        (
            "qq_originals_no_replace",
            "trigger",
            "qq_originals",
            Some(NO_REPLACE),
        ),
        (
            "qq_originals_no_update",
            "trigger",
            "qq_originals",
            Some(NO_UPDATE),
        ),
        ("qq_originals_session", "index", "qq_originals", Some(INDEX)),
        (
            "sqlite_autoindex_qq_originals_1",
            "index",
            "qq_originals",
            None,
        ),
    ];
    let mut statement = connection
        .prepare(
            "SELECT name,type,tbl_name,sql FROM sqlite_schema
            WHERE name LIKE 'qq_original%' OR tbl_name LIKE 'qq_original%'
            ORDER BY name LIMIT 8",
        )
        .map_err(|_| unavailable())?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|_| unavailable())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unavailable())?;
    if rows.len() != expected.len()
        || rows.iter().zip(expected).any(|(actual, expected)| {
            (
                actual.0.as_str(),
                actual.1.as_str(),
                actual.2.as_str(),
                actual.3.as_deref(),
            ) != expected
        })
    {
        return Err(unavailable());
    }
    Ok(())
}

pub(super) fn identity(connection: &Connection) -> PluginResult<(String, u64)> {
    validate_schema(connection)?;
    let receipt_id = receipt_identity(connection)?;
    let mut statement = connection
        .prepare(
            "SELECT singleton,version,identity,committed_count FROM qq_original_metadata LIMIT 2",
        )
        .map_err(|_| unavailable())?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, u64>(3)?,
            ))
        })
        .map_err(|_| unavailable())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unavailable())?;
    let [(1, 1, id, committed_count)] = rows.as_slice() else {
        return Err(unavailable());
    };
    let (count, min, max): (u64, Option<u64>, Option<u64>) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM qq_originals),
            (SELECT min(seq) FROM qq_originals),(SELECT max(seq) FROM qq_originals)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| unavailable())?;
    if id != &receipt_id
        || count != *committed_count
        || max.unwrap_or(0) != count
        || (count > 0 && min != Some(1))
    {
        return Err(unavailable());
    }
    Ok((id.clone(), count))
}

struct StoredOriginal {
    seq: u64,
    session_id: String,
    session_user_id: String,
    record: QqOriginalMessage,
}

fn decode(row: &Row<'_>) -> rusqlite::Result<StoredOriginal> {
    Ok(StoredOriginal {
        seq: row.get(0)?,
        session_id: bounded_text(row, 3, 256)?,
        session_user_id: bounded_text(row, 4, 256)?,
        record: QqOriginalMessage {
            app_id: bounded_text(row, 1, 128)?,
            message_id: bounded_text(row, 2, 256)?,
            scope: bounded_text(row, 5, 5)?,
            target_id: bounded_text(row, 6, 128)?,
            user_id: bounded_text(row, 7, 128)?,
            text: match row.get_ref(8)? {
                rusqlite::types::ValueRef::Null => None,
                _ => Some(bounded_text(row, 8, 32768)?),
            },
        },
    })
}

fn bounded_text(row: &Row<'_>, index: usize, max_bytes: usize) -> rusqlite::Result<String> {
    let value = row.get_ref(index)?.as_str()?;
    if value.len() > max_bytes {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(value.to_owned())
}

impl StoredOriginal {
    fn valid(&self) -> PluginResult<()> {
        if self.seq == 0 || !self.record.valid() {
            return Err(unavailable());
        }
        let key = self.record.session_key()?;
        if key.session_id != self.session_id || key.user_id != self.session_user_id {
            return Err(unavailable());
        }
        Ok(())
    }
}

pub(super) fn store(
    connection: &mut Connection,
    record: &QqOriginalMessage,
    expected: Option<&QqOriginalBinding>,
) -> PluginResult<u64> {
    if !record.valid() {
        return Err(unavailable());
    }
    let session = record.session_key()?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| unavailable())?;
    let (id, count) = identity(&tx)?;
    if expected.is_some_and(|expected| !expected.matches(&id, count)) {
        return Err(unavailable());
    }
    let previous = tx
        .query_row(
            "SELECT seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,text
                    FROM qq_originals WHERE app=?1 AND message_id=?2",
            params![record.app_id, record.message_id],
            decode,
        )
        .optional()
        .map_err(|_| unavailable())?;
    if let Some(previous) = previous {
        previous.valid()?;
        if previous.record != *record {
            return Err(unavailable());
        }
        return Ok(count);
    }
    let next = count.checked_add(1).ok_or_else(unavailable)?;
    tx.execute("INSERT INTO qq_originals(seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,text)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![next,record.app_id,record.message_id,session.session_id,session.user_id,record.scope,record.target_id,record.user_id,record.text])
        .map_err(|_| unavailable())?;
    let changed = tx
        .execute("UPDATE qq_original_metadata SET committed_count=?1 WHERE singleton=1 AND committed_count=?2",
            params![next,count])
        .map_err(|_| unavailable())?;
    if changed != 1 {
        return Err(unavailable());
    }
    tx.commit().map_err(|_| unavailable())?;
    Ok(next)
}

pub(super) fn read(
    connection: &mut Connection,
    app: &str,
    session: &SessionKey,
    through_message_id: &str,
    expected: Option<&QqOriginalBinding>,
) -> PluginResult<Vec<QqOriginalMessage>> {
    session.validate().map_err(|_| unavailable())?;
    let tx = connection.transaction().map_err(|_| unavailable())?;
    let (id, count) = identity(&tx)?;
    if expected.is_some_and(|expected| !expected.matches(&id, count)) {
        return Err(unavailable());
    }
    let anchor = tx
        .query_row("SELECT seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,text
                    FROM qq_originals WHERE app=?1 AND message_id=?2 AND session_id=?3 AND session_user_id=?4",
            params![app,through_message_id,session.session_id,session.user_id], decode)
        .map_err(|_| unavailable())?;
    anchor.valid()?;
    let mut statement = tx
        .prepare("SELECT seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,text
                    FROM qq_originals WHERE app=?1 AND session_id=?2 AND session_user_id=?3 AND seq<=?4
                    ORDER BY seq DESC LIMIT 6")
        .map_err(|_| unavailable())?;
    let mut rows = statement
        .query_map(
            params![app, session.session_id, session.user_id, anchor.seq],
            decode,
        )
        .map_err(|_| unavailable())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unavailable())?;
    if rows.first().is_none_or(|row| row.seq != anchor.seq) {
        return Err(unavailable());
    }
    for row in &rows {
        row.valid()?;
        if row.record.app_id != app
            || row.session_id != session.session_id
            || row.session_user_id != session.user_id
        {
            return Err(unavailable());
        }
    }
    rows.reverse();
    Ok(rows.into_iter().map(|row| row.record).collect())
}

pub(super) fn is_current(
    connection: &mut Connection,
    app: &str,
    session: &SessionKey,
    message_id: &str,
    expected: &QqOriginalBinding,
) -> PluginResult<bool> {
    session.validate().map_err(|_| unavailable())?;
    let tx = connection.transaction().map_err(|_| unavailable())?;
    let (identity, count) = identity(&tx)?;
    if !expected.matches(&identity, count) {
        return Err(unavailable());
    }
    // 最后列恒为 NULL：新鲜度只比较持久化元数据，不读取正文，更不依赖原文是否为空。
    let anchor = tx
        .query_row("SELECT seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,NULL
                    FROM qq_originals WHERE app=?1 AND message_id=?2 AND session_id=?3 AND session_user_id=?4",
            params![app,message_id,session.session_id,session.user_id], decode)
        .map_err(|_| unavailable())?;
    anchor.valid()?;
    let latest = tx
        .query_row(
            "SELECT seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,NULL
                    FROM qq_originals WHERE app=?1 AND session_id=?2 AND session_user_id=?3
                    ORDER BY seq DESC LIMIT 1",
            params![app, session.session_id, session.user_id],
            decode,
        )
        .map_err(|_| unavailable())?;
    latest.valid()?;
    Ok(anchor.seq == latest.seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteReceiptArchive;
    use eve_qqbot_plugin::{QqArchivedReceipt, QqReceiptArchive};

    fn record(id: &str, text: Option<&str>) -> QqOriginalMessage {
        QqOriginalMessage {
            app_id: "app".into(),
            message_id: id.into(),
            scope: "group".into(),
            target_id: "group".into(),
            user_id: "sender".into(),
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn full_body_gaps_and_empty_text_survive_restart_without_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("originals.sqlite3");
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        let large = record("large", Some(&"a".repeat(32768)));
        let gap = record("gap", None);
        let empty = record("empty", Some(""));
        for (index, record) in [&large, &gap, &empty].into_iter().enumerate() {
            assert_eq!(archive.store_original(record).unwrap(), index as u64 + 1);
        }
        let identity = archive.original_identity().unwrap().unwrap();
        assert_eq!(identity, (archive.identity().unwrap().0, 3));
        assert_eq!(archive.identity().unwrap().1, 0);
        drop(archive);
        let reopened = SqliteReceiptArchive::open(&path).unwrap();
        assert_eq!(reopened.original_identity().unwrap(), Some(identity));
        let session = empty.session_key().unwrap();
        assert_eq!(
            reopened.read_originals("app", &session, "empty").unwrap(),
            vec![large, gap, empty],
        );
    }

    #[test]
    fn exact_retries_preserve_sequence_and_conflicts_never_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("originals.sqlite3");
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        let original = record("one", Some("原话\n  不裁剪"));
        let next = record("two", Some("next"));
        assert_eq!(archive.store_original(&original).unwrap(), 1);
        assert_eq!(archive.store_original(&next).unwrap(), 2);
        assert_eq!(archive.store_original(&original).unwrap(), 2);
        let mut variants = Vec::new();
        let mut changed = original.clone();
        changed.text = None;
        variants.push(changed);
        let mut changed = original.clone();
        changed.text = Some("原话\n 不裁剪".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.target_id = "another_group".into();
        variants.push(changed);
        let mut changed = original.clone();
        changed.user_id = "another_sender".into();
        variants.push(changed);
        let mut changed = original.clone();
        changed.scope = "c2c".into();
        changed.target_id = changed.user_id.clone();
        variants.push(changed);
        for changed in variants {
            assert!(archive.store_original(&changed).is_err());
        }
        let gap = record("gap", None);
        archive.store_original(&gap).unwrap();
        assert!(archive.store_original(&record("gap", Some(""))).is_err());
        let session = original.session_key().unwrap();
        assert_eq!(
            archive.read_originals("app", &session, "one").unwrap(),
            vec![original]
        );
        assert_eq!(archive.original_identity().unwrap().unwrap().1, 3);
    }

    #[test]
    fn last_six_are_isolated_and_stop_at_the_exact_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let archive =
            SqliteReceiptArchive::open(&directory.path().join("originals.sqlite3")).unwrap();
        let mut same_session = Vec::new();
        for index in 0..10 {
            let current = record(
                &format!("id{index}"),
                if index == 4 { None } else { Some("原文") },
            );
            archive.store_original(&current).unwrap();
            same_session.push(current.clone());
            let mut other_app = current.clone();
            other_app.app_id = "foreign_app".into();
            archive.store_original(&other_app).unwrap();
            let mut other_user = current.clone();
            other_user.message_id = format!("other_user{index}");
            other_user.user_id = "foreign_sender".into();
            archive.store_original(&other_user).unwrap();
            let mut other_group = current;
            other_group.message_id = format!("other_group{index}");
            other_group.target_id = "foreign_group".into();
            archive.store_original(&other_group).unwrap();
        }
        let session = same_session[0].session_key().unwrap();
        assert_eq!(
            archive.read_originals("app", &session, "id7").unwrap(),
            same_session[2..8]
        );
        assert_eq!(
            archive.read_originals("app", &session, "id0").unwrap(),
            same_session[..1]
        );
        assert!(archive.read_originals("app", &session, "absent").is_err());
        assert!(
            archive
                .read_originals("foreign_app", &session, "id7")
                .is_err()
        );
        assert!(
            archive
                .read_originals("app", &session, "other_user7")
                .is_err()
        );
        let mut wrong_user = session.clone();
        wrong_user.user_id = "foreign_sender".into();
        assert!(archive.read_originals("app", &wrong_user, "id7").is_err());
        let mut wrong_session = session;
        wrong_session.session_id = "foreign_session".into();
        assert!(
            archive
                .read_originals("app", &wrong_session, "id7")
                .is_err()
        );
    }

    #[test]
    fn invalid_records_never_create_an_evidence_position() {
        let directory = tempfile::tempdir().unwrap();
        let archive =
            SqliteReceiptArchive::open(&directory.path().join("originals.sqlite3")).unwrap();
        let mut invalid = record("large", Some(&"a".repeat(32769)));
        assert!(archive.store_original(&invalid).is_err());
        invalid = record("bad", Some("原文"));
        invalid.user_id.clear();
        assert!(archive.store_original(&invalid).is_err());
        invalid = record("bad", Some("原文"));
        invalid.scope = "c2c".into();
        assert!(archive.store_original(&invalid).is_err());
        assert_eq!(archive.original_identity().unwrap().unwrap().1, 0);
    }

    #[test]
    fn legacy_database_migrates_atomically_without_changing_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE metadata (version INTEGER NOT NULL, identity TEXT NOT NULL);
            CREATE TABLE receipts (seq INTEGER PRIMARY KEY, app TEXT NOT NULL, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, failed_reply INTEGER NOT NULL CHECK(failed_reply IN (0,1)),
                payload BLOB NOT NULL, UNIQUE(app,message_id));
            CREATE INDEX failed_receipts ON receipts(app,session_id,failed_reply,seq DESC);
            INSERT INTO metadata VALUES (1,'0123456789abcdef0123456789abcdef');
            INSERT INTO receipts(app,message_id,session_id,failed_reply,payload) VALUES ('app','legacy','scope',1,X'010203');").unwrap();
        drop(connection);
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        assert_eq!(
            archive.identity().unwrap(),
            ("0123456789abcdef0123456789abcdef".into(), 1)
        );
        assert_eq!(archive.get("app", "legacy").unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(
            archive.original_identity().unwrap(),
            Some(("0123456789abcdef0123456789abcdef".into(), 0))
        );
        let first = record("legacy", None);
        archive.store_original(&first).unwrap();
        archive
            .store(&[QqArchivedReceipt {
                app_id: "app".into(),
                message_id: "new".into(),
                session_id: "scope".into(),
                failed_reply: false,
                payload: vec![4, 5],
            }])
            .unwrap();
        drop(archive);
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        assert_eq!(archive.identity().unwrap().1, 2);
        assert_eq!(archive.original_identity().unwrap().unwrap().1, 1);
        assert_eq!(
            archive
                .read_originals("app", &first.session_key().unwrap(), "legacy")
                .unwrap(),
            vec![first]
        );
    }

    #[test]
    fn missing_or_changed_schema_never_gets_rebuilt_on_restart() {
        for corruption in [
            "DROP TABLE qq_originals",
            "DROP TABLE qq_original_metadata",
            "DROP INDEX qq_originals_session",
            "DROP TRIGGER qq_originals_no_update",
            "DROP TRIGGER qq_originals_no_delete",
            "DROP TRIGGER qq_originals_no_replace",
            "DROP TABLE qq_originals; DROP TABLE qq_original_metadata",
            "DELETE FROM qq_original_metadata",
            "UPDATE qq_original_metadata SET identity='ffffffffffffffffffffffffffffffff'",
            "UPDATE qq_original_metadata SET committed_count=0",
            "PRAGMA user_version=0",
            "PRAGMA user_version=2",
            "CREATE TRIGGER unexpected_write AFTER INSERT ON qq_originals BEGIN SELECT 1; END",
            "DROP TABLE qq_originals; CREATE VIEW qq_originals AS SELECT 1 AS seq",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("corrupt.sqlite3");
            let archive = SqliteReceiptArchive::open(&path).unwrap();
            archive
                .store_original(&record("one", Some("original")))
                .unwrap();
            drop(archive);
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(corruption).unwrap();
            drop(connection);
            let before = std::fs::read(&path).unwrap();
            assert!(SqliteReceiptArchive::open(&path).is_err(), "{corruption}");
            assert_eq!(std::fs::read(&path).unwrap(), before, "{corruption}");
        }
    }

    #[test]
    fn malformed_selected_row_is_an_error_and_is_never_skipped() {
        for corruption in [
            "UPDATE qq_originals SET text=printf('%32769s','x') WHERE seq=4",
            "UPDATE qq_originals SET target_id='foreign_group' WHERE seq=4",
            "UPDATE qq_originals SET user_id='foreign_user' WHERE seq=4",
            "UPDATE qq_originals SET app='foreign_app' WHERE seq=4",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("corrupt.sqlite3");
            let archive = SqliteReceiptArchive::open(&path).unwrap();
            for index in 0..8 {
                archive
                    .store_original(&record(&format!("id{index}"), Some("original")))
                    .unwrap();
            }
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("DROP TRIGGER qq_originals_no_update")
                .unwrap();
            connection.execute_batch(corruption).unwrap();
            connection.execute_batch(NO_UPDATE).unwrap();
            drop(connection);
            let session = record("id7", None).session_key().unwrap();
            // 更改 app 会使损坏行离开查询集合；其他字段仍在所取六行内且必须拒绝。
            if !corruption.contains("SET app=") {
                assert!(
                    archive.read_originals("app", &session, "id7").is_err(),
                    "{corruption}"
                );
            } else {
                assert!(
                    archive
                        .read_originals("foreign_app", &session, "id3")
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn sqlite_update_delete_and_replace_cannot_mutate_existing_originals() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("originals.sqlite3");
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        let original = record("one", Some("original"));
        archive.store_original(&original).unwrap();
        let connection = Connection::open(&path).unwrap();
        for mutation in [
            "UPDATE qq_originals SET text='modified' WHERE seq=1",
            "DELETE FROM qq_originals WHERE seq=1",
            "INSERT OR REPLACE INTO qq_originals SELECT seq,app,message_id,session_id,session_user_id,scope,target_id,user_id,'modified' FROM qq_originals WHERE seq=1",
            "INSERT OR REPLACE INTO qq_originals SELECT 2,app,message_id,session_id,session_user_id,scope,target_id,user_id,'modified' FROM qq_originals WHERE seq=1",
        ] {
            assert!(connection.execute_batch(mutation).is_err(), "{mutation}");
        }
        assert_eq!(
            archive
                .read_originals("app", &original.session_key().unwrap(), "one")
                .unwrap(),
            vec![original]
        );
        assert_eq!(archive.original_identity().unwrap().unwrap().1, 1);
    }

    #[test]
    fn bound_operations_reject_stale_or_foreign_databases_before_reading_or_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("originals.sqlite3");
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        let (identity, records) = archive.original_identity().unwrap().unwrap();
        let initial = QqOriginalBinding { identity, records };
        let first = record("first", Some("first"));
        let second = record("second", Some("second"));
        assert_eq!(archive.store_original_bound(&first, &initial).unwrap(), 1);
        let session = first.session_key().unwrap();
        let mut stale = initial.clone();
        stale.records = 2;
        let mut foreign = initial.clone();
        foreign.identity = "ffffffffffffffffffffffffffffffff".into();
        for expected in [&stale, &foreign] {
            assert!(archive.store_original_bound(&second, expected).is_err());
            assert!(
                archive
                    .read_originals_bound("app", &session, "first", expected)
                    .is_err()
            );
            assert_eq!(archive.original_identity().unwrap().unwrap().1, 1);
        }
        // 库已提交、宿主标记尚未提交时，原绑定允许幂等重试与继续写入。
        assert_eq!(archive.store_original_bound(&first, &initial).unwrap(), 1);
        assert_eq!(archive.store_original_bound(&second, &initial).unwrap(), 2);
        assert_eq!(
            archive
                .read_originals_bound("app", &session, "second", &initial)
                .unwrap(),
            vec![first, second]
        );
        // 模拟运行时库及其内部元数据一起换了身份，内部自检仍通过而宿主绑定必须拒绝。
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
            UPDATE metadata SET identity='ffffffffffffffffffffffffffffffff';
            UPDATE qq_original_metadata SET identity='ffffffffffffffffffffffffffffffff';
            COMMIT;",
            )
            .unwrap();
        assert_eq!(
            archive.original_identity().unwrap().unwrap().0,
            foreign.identity
        );
        assert!(
            archive
                .read_originals_bound("app", &session, "second", &initial)
                .is_err()
        );
        assert!(
            archive
                .store_original_bound(&record("third", Some("third")), &initial)
                .is_err()
        );
        assert_eq!(archive.original_identity().unwrap().unwrap().1, 2);
    }

    #[test]
    fn freshness_uses_latest_persisted_identity_even_when_new_original_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("originals.sqlite3");
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        let (identity, records) = archive.original_identity().unwrap().unwrap();
        let binding = QqOriginalBinding { identity, records };
        let first = record("first", Some("old authorization"));
        let session = first.session_key().unwrap();
        archive.store_original_bound(&first, &binding).unwrap();
        assert!(
            archive
                .original_is_current_bound("app", &session, "first", &binding)
                .unwrap()
        );
        for (index, field) in ["app", "user", "group"].iter().enumerate() {
            let mut foreign = record(&format!("foreign{index}"), Some("foreign original"));
            match *field {
                "app" => foreign.app_id = "another_app".into(),
                "user" => foreign.user_id = "another_sender".into(),
                "group" => foreign.target_id = "another_group".into(),
                _ => unreachable!(),
            }
            archive.store_original_bound(&foreign, &binding).unwrap();
            assert!(
                archive
                    .original_is_current_bound("app", &session, "first", &binding)
                    .unwrap()
            );
        }
        let queued = record("queued", None);
        archive.store_original_bound(&queued, &binding).unwrap();
        assert!(
            !archive
                .original_is_current_bound("app", &session, "first", &binding)
                .unwrap()
        );
        assert!(
            archive
                .original_is_current_bound("app", &session, "queued", &binding)
                .unwrap()
        );
        archive.store_original_bound(&first, &binding).unwrap();
        assert!(
            !archive
                .original_is_current_bound("app", &session, "first", &binding)
                .unwrap(),
            "旧消息重试不能重新取得最新序号"
        );
        assert!(
            archive
                .original_is_current_bound("app", &session, "absent", &binding)
                .is_err()
        );
        assert!(
            archive
                .original_is_current_bound("another_app", &session, "queued", &binding)
                .is_err()
        );
        let mut wrong_session = session.clone();
        wrong_session.user_id = "foreign".into();
        assert!(
            archive
                .original_is_current_bound("app", &wrong_session, "queued", &binding)
                .is_err()
        );
        let mut stale = binding.clone();
        stale.records = 6;
        assert!(
            archive
                .original_is_current_bound("app", &session, "queued", &stale)
                .is_err()
        );
        let mut wrong_identity = binding.clone();
        wrong_identity.identity = "ffffffffffffffffffffffffffffffff".into();
        assert!(
            archive
                .original_is_current_bound("app", &session, "queued", &wrong_identity)
                .is_err()
        );
        drop(archive);
        let reopened = SqliteReceiptArchive::open(&path).unwrap();
        assert!(
            !reopened
                .original_is_current_bound("app", &session, "first", &binding)
                .unwrap()
        );
        assert!(
            reopened
                .original_is_current_bound("app", &session, "queued", &binding)
                .unwrap()
        );
    }

    #[test]
    fn freshness_does_not_read_text_but_rejects_corrupt_latest_route() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("originals.sqlite3");
        let archive = SqliteReceiptArchive::open(&path).unwrap();
        let (identity, records) = archive.original_identity().unwrap().unwrap();
        let binding = QqOriginalBinding { identity, records };
        let first = record("first", Some("first"));
        let last = record("last", None);
        let session = first.session_key().unwrap();
        archive.store_original(&first).unwrap();
        archive.store_original(&last).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER qq_originals_no_update;
            UPDATE qq_originals SET text=printf('%1048576s','x') WHERE message_id='last';",
            )
            .unwrap();
        connection.execute_batch(NO_UPDATE).unwrap();
        assert!(
            !archive
                .original_is_current_bound("app", &session, "first", &binding)
                .unwrap()
        );
        assert!(
            archive
                .original_is_current_bound("app", &session, "last", &binding)
                .unwrap()
        );
        assert!(
            archive
                .read_originals_bound("app", &session, "last", &binding)
                .is_err(),
            "新鲜度检查不等于正文证据完整性检查"
        );
        connection
            .execute_batch(
                "DROP TRIGGER qq_originals_no_update;
            UPDATE qq_originals SET target_id='another_group' WHERE message_id='last';",
            )
            .unwrap();
        connection.execute_batch(NO_UPDATE).unwrap();
        assert!(
            archive
                .original_is_current_bound("app", &session, "first", &binding)
                .is_err()
        );
    }
}
