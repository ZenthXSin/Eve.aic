use eve_llm_api::ContextScope;
use eve_plugin_api::PluginError;
use eve_training_api::*;
use eve_training_sqlite::SqliteExpressions;
use rusqlite::{Connection, params};
use std::fs;

fn scope(user: &str) -> ContextScope {
    ContextScope {
        session_id: "room".into(),
        user_id: user.into(),
    }
}
fn event(id: &str, text: &str) -> ConversationMessage {
    ConversationMessage {
        id: id.into(),
        speaker: "person".into(),
        role: ConversationRole::Human,
        text: text.into(),
        metadata: Some(ConversationMetadata {
            observed_at_ms: Some(10),
            source_kind: Some("received".into()),
            ..Default::default()
        }),
    }
}
fn lesson(response: &str) -> ConversationLesson {
    ConversationLesson {
        id: "lesson-one".into(),
        situation: "用户正在补充排查信息时".into(),
        response: response.into(),
        avoid: "对方要求安静时重新判断".into(),
        observations: 2,
        corrections: 0,
        counterexamples: 0,
        active: true,
        updated_at_ms: 10,
        evidence: vec![
            ConversationEvidence {
                message_id: "first".into(),
                quote: "先接一句".into(),
            },
            ConversationEvidence {
                message_id: "second".into(),
                quote: "接着查".into(),
            },
        ],
        evaluated_message_ids: vec!["first".into(), "second".into()],
    }
}
fn revision(response: &str) -> ConversationLessonRevision {
    ConversationLessonRevision {
        sequence: 0,
        lesson: lesson(response),
        reason: "observation".into(),
        batch_id: Some("batch-one".into()),
        recorded_at_ms: Some(10),
    }
}
fn expression() -> ExpressionRecord {
    ExpressionRecord {
        scope: scope("person"),
        id: "a".repeat(64),
        body: "b".repeat(64),
        chars: 8,
        paragraphs: 1,
        question: false,
        formal: false,
        casual: true,
        active: true,
    }
}

#[test]
fn complete_messages_keep_first_metadata_and_pause_dedup_without_touching_expressions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training-expressions.sqlite3");
    let db = SqliteExpressions::open(&path).unwrap();
    ExpressionStore::insert(&db, &expression()).unwrap();
    let marker = fs::read(path.with_extension("required")).unwrap();
    let original = event("first", "长文\n```rs\nlet x = 1;\n```\n> 引文\n尾部");
    assert_eq!(
        db.append(&scope("person"), &original, true)
            .unwrap()
            .sequence,
        Some(1)
    );
    let mut replay = original.clone();
    replay.metadata.as_mut().unwrap().observed_at_ms = Some(1000);
    replay.metadata.as_mut().unwrap().source_kind = Some("recovered".into());
    assert!(!db.append(&scope("person"), &replay, true).unwrap().inserted);
    let saved = db.events(&scope("person"), 1, 16).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].message.text, original.text);
    let metadata = saved[0].message.metadata.as_ref().unwrap();
    assert_eq!(metadata.observed_at_ms, Some(10));
    assert_eq!(metadata.sequence, Some(1));
    assert_eq!(metadata.source_kind.as_deref(), Some("received"));
    assert!(
        db.append(&scope("person"), &event("first", "不同正文"), true)
            .is_err()
    );
    assert!(
        db.append(&scope("person"), &event("paused", "暂停期间收到"), false)
            .unwrap()
            .sequence
            .is_none()
    );
    assert!(
        !db.append(&scope("person"), &event("paused", "暂停期间收到"), true)
            .unwrap()
            .inserted
    );
    assert!(db.events(&scope("other"), 1, 16).unwrap().is_empty());
    assert_eq!(
        ExpressionStore::get(&db, &scope("person"), &expression().id).unwrap(),
        Some(expression())
    );
    drop(db);
    let restored = SqliteExpressions::open(&path).unwrap();
    assert_eq!(restored.events(&scope("person"), 1, 16).unwrap(), saved);
    assert_eq!(fs::read(path.with_extension("required")).unwrap(), marker);
    assert_eq!(
        ExpressionStore::get(&restored, &scope("person"), &expression().id).unwrap(),
        Some(expression())
    );
}

#[test]
fn checkpoint_revision_and_batch_outcome_commit_together_and_old_versions_stay() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteExpressions::open(&dir.path().join("db")).unwrap();
    db.append(&scope("person"), &event("first", "先接一句"), true)
        .unwrap();
    let batch = ConversationBatch {
        id: "batch-one".into(),
        scope: scope("person"),
        messages: db
            .events(&scope("person"), 1, 16)
            .unwrap()
            .into_iter()
            .map(|e| e.message)
            .collect(),
        lessons: vec![],
        lesson_history: vec![],
        attempt: 1,
    };
    let change = ConversationCommit {
        expected_revision: 0,
        checkpoint: br#"{"version":2,"cursor":2}"#.to_vec(),
        revisions: vec![revision("先接话再行动")],
        batch: Some(ConversationBatchArchive {
            batch,
            outcome: "completed".into(),
            recorded_at_ms: Some(10),
        }),
    };
    assert!(db.commit(&scope("person"), &change).unwrap());
    assert!(!db.commit(&scope("person"), &change).unwrap());
    assert_eq!(db.scope(&scope("person")).unwrap().unwrap().revision, 1);
    let mut updated = change.clone();
    updated.expected_revision = 1;
    updated.revisions = vec![revision("在需要查证时先自然回应，结果后详细说")];
    updated.batch = None;
    assert!(db.commit(&scope("person"), &updated).unwrap());
    let versions = db.revisions(&scope("person"), 0, 16).unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].lesson.response, "先接话再行动");
    assert_eq!(versions[1].lesson.id, versions[0].lesson.id);
    assert_eq!(
        db.latest_revision(&scope("person"), "lesson-one").unwrap(),
        Some(versions[1].clone())
    );
    assert!(
        db.latest_revision(&scope("other"), "lesson-one")
            .unwrap()
            .is_none()
    );
    // 明确不合法的归档不能提交任何 checkpoint 或新经验。
    let before = db.scope(&scope("person")).unwrap();
    updated.expected_revision = 2;
    updated.batch = change.batch;
    updated.batch.as_mut().unwrap().outcome = "pretend_success".into();
    assert!(matches!(
        db.commit(&scope("person"), &updated),
        Err(PluginError::Task(_))
    ));
    assert_eq!(db.scope(&scope("person")).unwrap(), before);
    assert_eq!(db.revisions(&scope("person"), 0, 16).unwrap(), versions);
}

#[test]
fn legacy_import_is_atomic_idempotent_and_source_changes_do_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteExpressions::open(&dir.path().join("db")).unwrap();
    let legacy = ConversationLegacyScope {
        scope: scope("person"),
        events: vec![ConversationEvent {
            sequence: 1,
            message: event("first", "先接一句"),
        }],
        seen: vec![],
        checkpoint: br#"{"version":2,"cursor":1,"lessons":[]}"#.to_vec(),
        revisions: vec![revision("先接话再行动")],
        batches: vec![],
    };
    ConversationStore::import_legacy(&db, &"c".repeat(64), std::slice::from_ref(&legacy)).unwrap();
    ConversationStore::import_legacy(&db, &"c".repeat(64), std::slice::from_ref(&legacy)).unwrap();
    assert_eq!(db.events(&scope("person"), 1, 16).unwrap().len(), 1);
    assert_eq!(db.revisions(&scope("person"), 0, 16).unwrap().len(), 1);
    assert!(ConversationStore::import_legacy(&db, &"d".repeat(64), &[legacy]).is_err());
    assert!(
        matches!(
            db.events(&scope("person"), 1, 16),
            Err(PluginError::State(_))
        ),
        "来源变化后旧实例停止写读"
    );
    drop(db);
    let restored = SqliteExpressions::open(&dir.path().join("db")).unwrap();
    assert_eq!(restored.events(&scope("person"), 1, 16).unwrap().len(), 1);
}

#[test]
fn corrupt_event_identity_closes_store_and_preserves_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = SqliteExpressions::open(&path).unwrap();
    db.append(&scope("person"), &event("first", "原消息"), true)
        .unwrap();
    let tamper = Connection::open(&path).unwrap();
    tamper
        .execute(
            "UPDATE conversation_events SET message_id=?1",
            params!["other-id"],
        )
        .unwrap();
    drop(tamper);
    let bytes = fs::read(&path).unwrap();
    assert!(matches!(
        db.events(&scope("person"), 1, 16),
        Err(PluginError::State(_))
    ));
    assert!(matches!(
        db.append(&scope("person"), &event("second", "新消息"), true),
        Err(PluginError::State(_))
    ));
    drop(db);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn missing_migrated_tables_are_rejected_without_recreating_or_clearing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    drop(SqliteExpressions::open(&path).unwrap());
    let tamper = Connection::open(&path).unwrap();
    tamper
        .execute_batch(
            "DROP TABLE conversation_batches; DROP TABLE conversation_revisions;
        DROP TABLE conversation_seen; DROP TABLE conversation_events;
        DROP TABLE conversation_scopes; DROP TABLE conversation_metadata;",
        )
        .unwrap();
    drop(tamper);
    let bytes = fs::read(&path).unwrap();
    assert!(SqliteExpressions::open(&path).is_err());
    assert_eq!(fs::read(path).unwrap(), bytes);
}
