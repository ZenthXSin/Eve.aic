//! 与表达统计共用同一 SQLite 身份和事务边界的完整交流日志。
use super::{SqliteExpressions, failed, hash, scope_valid};
use eve_llm_api::ContextScope;
use eve_plugin_api::{PluginError, PluginResult};
use eve_training_api::*;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

fn invalid() -> PluginError {
    PluginError::Task("交流记录无效或消息编号内容冲突；原记录保留".into())
}
const MAX_CHECKPOINT_BYTES: usize = 4 * 1024 * 1024;

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn checkpoint_valid(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_CHECKPOINT_BYTES
        && serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|value| value.is_object())
}
fn lesson_valid(lesson: &ConversationLesson) -> bool {
    let text = |value: &str, max: usize| {
        !value.trim().is_empty()
            && value.len() <= max
            && !value
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    };
    identifier(&lesson.id)
        && text(&lesson.situation, 240)
        && text(&lesson.response, 480)
        && lesson.avoid.len() <= 480
        && (2..=4).contains(&lesson.evidence.len())
        && lesson
            .evidence
            .iter()
            .all(|e| identifier(&e.message_id) && text(&e.quote, 240))
        && lesson.evaluated_message_ids.len() <= 64
        && lesson.evaluated_message_ids.iter().all(|id| identifier(id))
        && (!lesson.active
            || lesson.counterexamples == 0 && (lesson.observations >= 2 || lesson.corrections > 0))
}
fn revision_valid(revision: &ConversationLessonRevision) -> bool {
    lesson_valid(&revision.lesson)
        && identifier(&revision.reason)
        && revision.batch_id.as_ref().is_none_or(|id| identifier(id))
}
fn archive_valid(scope: &ContextScope, archive: &ConversationBatchArchive) -> bool {
    let batch = &archive.batch;
    batch.scope == *scope
        && identifier(&batch.id)
        && (1..=3).contains(&batch.attempt)
        && !batch.messages.is_empty()
        && batch.messages.len() <= MAX_CONVERSATION_MESSAGES
        && batch.messages.iter().all(conversation_message_valid)
        && batch
            .messages
            .iter()
            .map(|m| &m.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == batch.messages.len()
        && batch.lessons.len() <= MAX_CONVERSATION_LESSONS
        && batch.lessons.iter().all(lesson_valid)
        && batch.lesson_history.iter().all(revision_valid)
        && matches!(
            archive.outcome.as_str(),
            "completed" | "failed" | "paused" | "reset" | "interrupted"
        )
}
fn insert_archive(
    db: &Connection,
    scope: &ContextScope,
    archive: &ConversationBatchArchive,
) -> PluginResult<()> {
    if !archive_valid(scope, archive) {
        return Err(invalid());
    }
    let bytes = serde_json::to_vec(&archive.batch).map_err(|_| invalid())?;
    db.execute("INSERT INTO conversation_batches(session_id,user_id,batch_id,attempt,outcome,batch_json,recorded_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![scope.session_id,scope.user_id,archive.batch.id,archive.batch.attempt,archive.outcome,bytes,archive.recorded_at_ms]).map_err(|_| failed())?;
    Ok(())
}
fn scope_record_valid(record: &ConversationStoredScope) -> bool {
    scope_valid(&record.scope)
        && record.next_sequence > 0
        && record
            .checkpoint
            .as_ref()
            .is_none_or(|bytes| checkpoint_valid(bytes))
}
fn decode_revision(
    sequence: u64,
    lesson_id: &str,
    bytes: &[u8],
) -> PluginResult<ConversationLessonRevision> {
    let revision: ConversationLessonRevision =
        serde_json::from_slice(bytes).map_err(|_| failed())?;
    if sequence == 0
        || revision.sequence != sequence
        || revision.lesson.id != lesson_id
        || !revision_valid(&revision)
    {
        return Err(failed());
    }
    Ok(revision)
}

pub(super) fn initialize(db: &Connection) -> PluginResult<()> {
    let user_version: u64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| failed())?;
    let count: u64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('conversation_metadata','conversation_scopes','conversation_events','conversation_seen','conversation_revisions','conversation_batches')",
        [], |row| row.get(0),
    ).map_err(|_| failed())?;
    if count == 0 && user_version == 0 {
        db.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE conversation_metadata(version INTEGER NOT NULL, legacy_sha TEXT NOT NULL);
            INSERT INTO conversation_metadata VALUES(1,'');
            CREATE TABLE conversation_scopes(
                session_id TEXT NOT NULL, user_id TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0, next_sequence INTEGER NOT NULL DEFAULT 1,
                checkpoint BLOB, PRIMARY KEY(session_id,user_id));
            CREATE TABLE conversation_events(
                session_id TEXT NOT NULL, user_id TEXT NOT NULL, sequence INTEGER NOT NULL,
                message_id TEXT NOT NULL, message_json BLOB NOT NULL,
                PRIMARY KEY(session_id,user_id,sequence), UNIQUE(session_id,user_id,message_id));
            CREATE TABLE conversation_seen(
                session_id TEXT NOT NULL, user_id TEXT NOT NULL, id_sha TEXT NOT NULL,
                body_sha TEXT NOT NULL, PRIMARY KEY(session_id,user_id,id_sha));
            CREATE TABLE conversation_revisions(
                session_id TEXT NOT NULL, user_id TEXT NOT NULL, sequence INTEGER NOT NULL,
                lesson_id TEXT NOT NULL, revision_json BLOB NOT NULL,
                PRIMARY KEY(session_id,user_id,sequence));
            CREATE INDEX conversation_lesson_history ON conversation_revisions(session_id,user_id,lesson_id,sequence);
            CREATE TABLE conversation_batches(
                sequence INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                user_id TEXT NOT NULL, batch_id TEXT NOT NULL, attempt INTEGER NOT NULL,
                outcome TEXT NOT NULL, batch_json BLOB NOT NULL, recorded_at_ms INTEGER,
                UNIQUE(session_id,user_id,batch_id,attempt,outcome));
            PRAGMA user_version=1;
            COMMIT;").map_err(|_| failed())?;
    } else if count != 6 || user_version != 1 {
        return Err(failed());
    }
    let versions: Vec<(u64, String)> = db
        .prepare("SELECT version,legacy_sha FROM conversation_metadata")
        .map_err(|_| failed())?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|_| failed())?
        .collect::<Result<_, _>>()
        .map_err(|_| failed())?;
    if versions.len() != 1
        || versions[0].0 != 1
        || !versions[0].1.is_empty() && !digest_valid(&versions[0].1)
    {
        return Err(failed());
    }
    for sql in [
        "SELECT revision,next_sequence,checkpoint FROM conversation_scopes LIMIT 0",
        "SELECT sequence,message_id,message_json FROM conversation_events LIMIT 0",
        "SELECT id_sha,body_sha FROM conversation_seen LIMIT 0",
        "SELECT sequence,lesson_id,revision_json FROM conversation_revisions LIMIT 0",
        "SELECT batch_id,attempt,outcome,batch_json,recorded_at_ms FROM conversation_batches LIMIT 0",
    ] {
        db.prepare(sql).map_err(|_| failed())?;
    }
    Ok(())
}

fn ensure_scope(db: &Connection, scope: &ContextScope) -> PluginResult<()> {
    db.execute(
        "INSERT OR IGNORE INTO conversation_scopes(session_id,user_id) VALUES(?1,?2)",
        params![scope.session_id, scope.user_id],
    )
    .map_err(|_| failed())?;
    Ok(())
}
fn stored_scope(
    db: &Connection,
    scope: &ContextScope,
) -> PluginResult<Option<ConversationStoredScope>> {
    let record = db.query_row("SELECT revision,next_sequence,checkpoint FROM conversation_scopes WHERE session_id=?1 AND user_id=?2",
        params![scope.session_id,scope.user_id], |row| Ok(ConversationStoredScope {
            scope: scope.clone(), revision: row.get(0)?, next_sequence: row.get(1)?, checkpoint: row.get(2)?,
        })).optional().map_err(|_| failed())?;
    if record
        .as_ref()
        .is_some_and(|record| !scope_record_valid(record))
    {
        return Err(failed());
    }
    Ok(record)
}
fn core_hash(message: &ConversationMessage) -> PluginResult<String> {
    // v1 的正文摘要序列化顺序完全相同。恢复时新增时钟或工具活动不修改第一次观察。
    let mut core = message.clone();
    core.metadata = None;
    Ok(hash(&serde_json::to_vec(&core).map_err(|_| invalid())?))
}
fn insert_event(
    db: &Connection,
    scope: &ContextScope,
    event: &ConversationEvent,
) -> PluginResult<()> {
    let bytes = serde_json::to_vec(&event.message).map_err(|_| invalid())?;
    db.execute("INSERT INTO conversation_events(session_id,user_id,sequence,message_id,message_json) VALUES(?1,?2,?3,?4,?5)",
        params![scope.session_id,scope.user_id,event.sequence,event.message.id,bytes]).map_err(|_| failed())?;
    Ok(())
}
fn insert_revision(
    db: &Connection,
    scope: &ContextScope,
    revision: &ConversationLessonRevision,
) -> PluginResult<()> {
    if !revision_valid(revision) {
        return Err(invalid());
    }
    let sequence: u64 = db.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM conversation_revisions WHERE session_id=?1 AND user_id=?2",
        params![scope.session_id,scope.user_id], |row| row.get(0)).map_err(|_| failed())?;
    let mut revision = revision.clone();
    revision.sequence = sequence;
    let bytes = serde_json::to_vec(&revision).map_err(|_| invalid())?;
    db.execute("INSERT INTO conversation_revisions(session_id,user_id,sequence,lesson_id,revision_json) VALUES(?1,?2,?3,?4,?5)",
        params![scope.session_id,scope.user_id,sequence,revision.lesson.id,bytes]).map_err(|_| failed())?;
    Ok(())
}
fn insert_seen(
    tx: &Transaction<'_>,
    scope: &ContextScope,
    seen: &ConversationSeen,
) -> PluginResult<()> {
    let existing: Option<String> = tx.query_row("SELECT body_sha FROM conversation_seen WHERE session_id=?1 AND user_id=?2 AND id_sha=?3",
        params![scope.session_id,scope.user_id,seen.id], |row| row.get(0)).optional().map_err(|_| failed())?;
    if let Some(existing) = existing {
        if existing != seen.body {
            return Err(invalid());
        }
    } else {
        tx.execute(
            "INSERT INTO conversation_seen(session_id,user_id,id_sha,body_sha) VALUES(?1,?2,?3,?4)",
            params![scope.session_id, scope.user_id, seen.id, seen.body],
        )
        .map_err(|_| failed())?;
    }
    Ok(())
}

impl ConversationStore for SqliteExpressions {
    fn import_legacy(&self, digest: &str, scopes: &[ConversationLegacyScope]) -> PluginResult<()> {
        if !digest_valid(digest)
            || scopes.iter().any(|s| {
                !scope_valid(&s.scope)
                    || !checkpoint_valid(&s.checkpoint)
                    || s.seen
                        .iter()
                        .any(|seen| !digest_valid(&seen.id) || !digest_valid(&seen.body))
            })
        {
            return Err(invalid());
        }
        self.run(|db| {
            let tx = db.transaction().map_err(|_| failed())?;
            let previous: String = tx.query_row("SELECT legacy_sha FROM conversation_metadata", [], |row| row.get(0)).map_err(|_| failed())?;
            if previous == digest { return Ok(()); }
            if !previous.is_empty() { return Err(PluginError::State("旧交流来源发生变化；不覆盖已迁移数据库".into())); }
            for scope in scopes {
                if stored_scope(&tx, &scope.scope)?.is_some() { return Err(invalid()); }
                ensure_scope(&tx, &scope.scope)?;
                let mut previous_sequence = 0;
                for event in &scope.events {
                    if event.sequence != previous_sequence + 1 || !conversation_message_valid(&event.message)
                        || event.message.metadata.as_ref().is_some_and(|m| m.fragment.is_some()
                            || m.sequence.is_some_and(|sequence| sequence != event.sequence)) { return Err(invalid()); }
                    previous_sequence = event.sequence;
                    insert_event(&tx, &scope.scope, event)?;
                    insert_seen(&tx, &scope.scope, &ConversationSeen { id: hash(event.message.id.as_bytes()), body: core_hash(&event.message)? })?;
                }
                for seen in &scope.seen { insert_seen(&tx, &scope.scope, seen)?; }
                for revision in &scope.revisions { insert_revision(&tx, &scope.scope, revision)?; }
                for batch in &scope.batches {
                    let mut last_sequence = 0;
                    if batch.outcome != "interrupted" || batch.recorded_at_ms.is_some() || batch.batch.messages.iter().any(|message| {
                        let Some(event) = scope.events.iter().find(|event| event.message == *message) else { return true; };
                        if event.sequence <= last_sequence { return true; }
                        last_sequence = event.sequence;
                        false
                    }) { return Err(invalid()); }
                    insert_archive(&tx, &scope.scope, batch)?;
                }
                let next = previous_sequence.checked_add(1).ok_or_else(invalid)?;
                tx.execute("UPDATE conversation_scopes SET checkpoint=?3,next_sequence=?4,revision=1 WHERE session_id=?1 AND user_id=?2",
                    params![scope.scope.session_id,scope.scope.user_id,scope.checkpoint,next]).map_err(|_| failed())?;
            }
            tx.execute("UPDATE conversation_metadata SET legacy_sha=?1", [digest]).map_err(|_| failed())?;
            tx.commit().map_err(|_| failed())
        })
    }
    fn append(
        &self,
        scope: &ContextScope,
        message: &ConversationMessage,
        enabled: bool,
    ) -> PluginResult<ConversationAppend> {
        if !scope_valid(scope)
            || !conversation_message_valid(message)
            || message
                .metadata
                .as_ref()
                .is_some_and(|m| m.sequence.is_some() || m.fragment.is_some())
        {
            return Err(invalid());
        }
        let id = hash(message.id.as_bytes());
        let body = core_hash(message)?;
        self.run(|db| {
            let tx = db.transaction().map_err(|_| failed())?;
            let previous: Option<String> = tx.query_row("SELECT body_sha FROM conversation_seen WHERE session_id=?1 AND user_id=?2 AND id_sha=?3",
                params![scope.session_id,scope.user_id,id], |row| row.get(0)).optional().map_err(|_| failed())?;
            if let Some(previous) = previous {
                if previous != body { return Err(invalid()); }
                let sequence = tx.query_row("SELECT sequence FROM conversation_events WHERE session_id=?1 AND user_id=?2 AND message_id=?3",
                    params![scope.session_id,scope.user_id,message.id], |row| row.get(0)).optional().map_err(|_| failed())?;
                return Ok(ConversationAppend { inserted: false, sequence });
            }
            ensure_scope(&tx, scope)?;
            insert_seen(&tx, scope, &ConversationSeen { id, body })?;
            let sequence = if enabled {
                let sequence = stored_scope(&tx, scope)?.ok_or_else(failed)?.next_sequence;
                let next = sequence.checked_add(1).ok_or_else(failed)?;
                let mut message = message.clone();
                message.metadata.get_or_insert_with(Default::default).sequence = Some(sequence);
                insert_event(&tx, scope, &ConversationEvent { sequence, message })?;
                tx.execute("UPDATE conversation_scopes SET next_sequence=?3 WHERE session_id=?1 AND user_id=?2",
                    params![scope.session_id,scope.user_id,next]).map_err(|_| failed())?;
                Some(sequence)
            } else { None };
            tx.commit().map_err(|_| failed())?;
            Ok(ConversationAppend { inserted: true, sequence })
        })
    }
    fn scopes(&self) -> PluginResult<Vec<ConversationStoredScope>> {
        self.run(|db| {
            let mut query = db.prepare("SELECT session_id,user_id,revision,next_sequence,checkpoint FROM conversation_scopes ORDER BY session_id,user_id").map_err(|_| failed())?;
            let rows = query.query_map([], |row| Ok(ConversationStoredScope {
                scope: ContextScope { session_id: row.get(0)?, user_id: row.get(1)? },
                revision: row.get(2)?, next_sequence: row.get(3)?, checkpoint: row.get(4)?,
            })).map_err(|_| failed())?;
            let rows: Vec<_> = rows.collect::<Result<_, _>>().map_err(|_| failed())?;
            if rows.iter().any(|row| !scope_record_valid(row)) { return Err(failed()); }
            Ok(rows)
        })
    }
    fn scope(&self, scope: &ContextScope) -> PluginResult<Option<ConversationStoredScope>> {
        if !scope_valid(scope) {
            return Err(invalid());
        }
        self.run(|db| stored_scope(db, scope))
    }
    fn events(
        &self,
        scope: &ContextScope,
        from_sequence: u64,
        limit: usize,
    ) -> PluginResult<Vec<ConversationEvent>> {
        if !scope_valid(scope) || limit > 256 {
            return Err(invalid());
        }
        self.run(|db| {
            let mut query = db.prepare("SELECT sequence,message_id,message_json FROM conversation_events WHERE session_id=?1 AND user_id=?2 AND sequence>=?3 ORDER BY sequence LIMIT ?4").map_err(|_| failed())?;
            let rows: Vec<(u64,String,Vec<u8>)> = query.query_map(params![scope.session_id,scope.user_id,from_sequence,limit], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
                .map_err(|_| failed())?.collect::<Result<_, _>>().map_err(|_| failed())?;
            rows.into_iter().map(|(sequence, message_id, bytes)| {
                let message = serde_json::from_slice(&bytes).map_err(|_| failed())?;
                if sequence == 0 || !conversation_message_valid(&message) || message.id != message_id
                    || message.metadata.as_ref().is_some_and(|m| m.fragment.is_some() || m.sequence.is_some_and(|s| s != sequence)) { return Err(failed()); }
                Ok(ConversationEvent { sequence, message })
            }).collect()
        })
    }
    fn revisions(
        &self,
        scope: &ContextScope,
        after_sequence: u64,
        limit: usize,
    ) -> PluginResult<Vec<ConversationLessonRevision>> {
        if !scope_valid(scope) || limit > 256 {
            return Err(invalid());
        }
        self.run(|db| {
            let mut query = db.prepare("SELECT sequence,lesson_id,revision_json FROM conversation_revisions WHERE session_id=?1 AND user_id=?2 AND sequence>?3 ORDER BY sequence LIMIT ?4").map_err(|_| failed())?;
            let rows: Vec<(u64,String,Vec<u8>)> = query.query_map(params![scope.session_id,scope.user_id,after_sequence,limit], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
                .map_err(|_| failed())?.collect::<Result<_, _>>().map_err(|_| failed())?;
            rows.into_iter().map(|(sequence,id,bytes)| decode_revision(sequence,&id,&bytes)).collect()
        })
    }
    fn latest_revision(
        &self,
        scope: &ContextScope,
        lesson_id: &str,
    ) -> PluginResult<Option<ConversationLessonRevision>> {
        if !scope_valid(scope) || !identifier(lesson_id) {
            return Err(invalid());
        }
        self.run(|db| {
            let record: Option<(u64,Vec<u8>)> = db.query_row("SELECT sequence,revision_json FROM conversation_revisions WHERE session_id=?1 AND user_id=?2 AND lesson_id=?3 ORDER BY sequence DESC LIMIT 1",
                params![scope.session_id,scope.user_id,lesson_id], |row| Ok((row.get(0)?,row.get(1)?))).optional().map_err(|_| failed())?;
            record.map(|(sequence,bytes)| decode_revision(sequence,lesson_id,&bytes)).transpose()
        })
    }
    fn commit(&self, scope: &ContextScope, change: &ConversationCommit) -> PluginResult<bool> {
        if !scope_valid(scope)
            || !checkpoint_valid(&change.checkpoint)
            || change
                .revisions
                .iter()
                .any(|revision| !revision_valid(revision))
            || change
                .batch
                .as_ref()
                .is_some_and(|archive| !archive_valid(scope, archive))
        {
            return Err(invalid());
        }
        self.run(|db| {
            let tx = db.transaction().map_err(|_| failed())?;
            ensure_scope(&tx, scope)?;
            if stored_scope(&tx, scope)?.ok_or_else(failed)?.revision != change.expected_revision { return Ok(false); }
            let next = change.expected_revision.checked_add(1).ok_or_else(failed)?;
            tx.execute("UPDATE conversation_scopes SET revision=?3,checkpoint=?4 WHERE session_id=?1 AND user_id=?2",
                params![scope.session_id,scope.user_id,next,change.checkpoint]).map_err(|_| failed())?;
            for revision in &change.revisions { insert_revision(&tx, scope, revision)?; }
            if let Some(archive) = &change.batch {
                insert_archive(&tx, scope, archive)?;
            }
            tx.commit().map_err(|_| failed())?;
            Ok(true)
        })
    }
}
