use eve_control_api::{ControlPhase, GenerationKey};
use eve_message_api::*;
use eve_relation_experience_sqlite::SqliteRelationExperience;
use eve_session_api::SessionKey;
use rusqlite::Connection;

fn input(id: &str, text: &str) -> RelationInput {
    RelationInput {
        message: IncomingMessage {
            message_id: id.into(),
            target: GenerationKey {
                session: SessionKey {
                    session_id: "qq:app:private:user".into(),
                    user_id: "user".into(),
                },
                task_id: "task".into(),
                controller_epoch: [3; 16],
                generation: 1,
            },
            text: text.into(),
            reply_to: None,
        },
        phase: ControlPhase::Generating,
        task_text: "让日常回复更自然".into(),
        cancel_requested: false,
        started_tools: Some(0),
        clarification: None,
    }
}
fn decision(input: &RelationInput, intent: MessageIntent) -> RelationDecision {
    RelationDecision {
        target: input.message.target.clone(),
        message_id: input.message.message_id.clone(),
        parts: vec![IntentPart {
            intent,
            confidence: 95,
            span: matches!(
                intent,
                MessageIntent::Supplement | MessageIntent::Correction | MessageIntent::NewTask
            )
            .then_some(TextSpan {
                start: 0,
                end: input.message.text.len(),
            }),
        }],
        explanation: "当前任务的措辞补充".into(),
    }
}
#[test]
fn durable_source_dedup_scoped_search_and_old_relevant_examples() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jev.sqlite3");
    let store = SqliteRelationExperience::open(&path).unwrap();
    let first = input("original", "然后记得去除末尾句号");
    let d = decision(&first, MessageIntent::Supplement);
    store
        .remember(&first, &d, RelationExperienceSource::PrimaryModel)
        .unwrap();
    store
        .remember(&first, &d, RelationExperienceSource::PrimaryModel)
        .unwrap();
    for n in 0..150 {
        let filler = input(&format!("filler-{n}"), &format!("构建参数编号{n}的记录"));
        store
            .remember(
                &filler,
                &decision(&filler, MessageIntent::Supplement),
                RelationExperienceSource::PrimaryModel,
            )
            .unwrap();
    }
    drop(store);
    let store = SqliteRelationExperience::open(&path).unwrap();
    let mut query = input("next", "然后记得省略末尾句号");
    let rows = store.recall(&query).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message, first.message.text);
    assert!(
        !serde_json::to_string(&rows)
            .unwrap()
            .contains("qq:app:private:user")
    );
    query.message.target.session.user_id = "someone-else".into();
    assert!(store.recall(&query).unwrap().is_empty());
    query.message.target.session = SessionKey {
        session_id: "qq:app:group:user".into(),
        user_id: "user".into(),
    };
    assert!(store.recall(&query).unwrap().is_empty());
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM examples", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        151
    );
}
#[test]
fn explicit_user_resolution_wins_over_newer_conflicting_model_guesses() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteRelationExperience::open(&root.path().join("jev.sqlite3")).unwrap();
    for (id, intent, source) in [
        (
            "guess1",
            MessageIntent::NewTask,
            RelationExperienceSource::PrimaryModel,
        ),
        (
            "human",
            MessageIntent::Supplement,
            RelationExperienceSource::UserClarification,
        ),
        (
            "guess2",
            MessageIntent::NewTask,
            RelationExperienceSource::PrimaryModel,
        ),
    ] {
        let i = input(id, "然后记得去除末尾句号");
        store.remember(&i, &decision(&i, intent), source).unwrap();
    }
    let rows = store
        .recall(&input("query", "然后记得去除末尾句号"))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, RelationExperienceSource::UserClarification);
    assert_eq!(rows[0].parts[0].intent, MessageIntent::Supplement);
}
#[test]
fn conflict_and_transaction_failure_preserve_original_rows_and_disable_only_experience() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jev.sqlite3");
    let store = SqliteRelationExperience::open(&path).unwrap();
    let first = input("same", "然后记得去除末尾句号");
    store
        .remember(
            &first,
            &decision(&first, MessageIntent::Supplement),
            RelationExperienceSource::PrimaryModel,
        )
        .unwrap();
    assert!(
        store
            .remember(
                &first,
                &decision(&first, MessageIntent::NewTask),
                RelationExperienceSource::PrimaryModel
            )
            .is_err()
    );
    assert!(store.recall(&first).is_err());
    drop(store);
    let store = SqliteRelationExperience::open(&path).unwrap();
    assert_eq!(
        store.recall(&first).unwrap()[0].parts[0].intent,
        MessageIntent::Supplement
    );
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER refuse_new BEFORE INSERT ON examples BEGIN SELECT RAISE(ABORT,'write refused'); END;").unwrap();
    let next = input("next", "然后记得省略末尾句号");
    assert!(
        store
            .remember(
                &next,
                &decision(&next, MessageIntent::Supplement),
                RelationExperienceSource::PrimaryModel
            )
            .is_err()
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM examples", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn cancelled_ambiguous_oversized_or_invalid_source_is_not_learned() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jev.sqlite3");
    let store = SqliteRelationExperience::open(&path).unwrap();
    for intent in [MessageIntent::Cancel, MessageIntent::Ambiguous] {
        let i = input("unlearnable", "然后记得去除末尾句号");
        store
            .remember(
                &i,
                &decision(&i, intent),
                RelationExperienceSource::PrimaryModel,
            )
            .unwrap();
    }
    let long = input("large", &"要求".repeat(2000));
    store
        .remember(
            &long,
            &decision(&long, MessageIntent::Supplement),
            RelationExperienceSource::PrimaryModel,
        )
        .unwrap();
    let i = input("invalid", "然后记得去除末尾句号");
    let mut bad = decision(&i, MessageIntent::Supplement);
    bad.message_id = "foreign".into();
    assert!(
        store
            .remember(&i, &bad, RelationExperienceSource::PrimaryModel)
            .is_err()
    );
    assert!(store.recall(&i).unwrap().is_empty());
}
#[test]
fn corrupt_future_and_empty_existing_databases_are_refused_without_replacement() {
    let root = tempfile::tempdir().unwrap();
    for (name, raw) in [
        ("corrupt", b"not sqlite".as_slice()),
        ("empty", b"".as_slice()),
    ] {
        let path = root.path().join(name);
        std::fs::write(&path, raw).unwrap();
        assert!(SqliteRelationExperience::open(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), raw);
    }
    let path = root.path().join("future");
    drop(SqliteRelationExperience::open(&path).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute("UPDATE metadata SET version=2", [])
        .unwrap();
    drop(connection);
    let before = std::fs::read(&path).unwrap();
    assert!(SqliteRelationExperience::open(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}
