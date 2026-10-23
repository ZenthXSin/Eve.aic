use eve_llm_api::ContextScope;
use eve_subagent_profile_sqlite::SqliteSubagentProfiles;
use eve_task_api::{SubagentProfileDraft, SubagentProfileError, SubagentProfiles};

fn scope() -> ContextScope {
    ContextScope {
        session_id: "session".into(),
        user_id: "owner".into(),
    }
}
fn draft() -> SubagentProfileDraft {
    SubagentProfileDraft {
        name: "reviewer".into(),
        instructions: "检查当前任务结果".into(),
        tools: Some(vec!["read_file".into()]),
    }
}

#[test]
fn creation_watermark_survives_empty_table_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profiles.sqlite3");
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    assert_eq!(store.revision().unwrap(), 0);
    let first = store.create(&scope(), draft()).unwrap();
    let first_revision = store.revision().unwrap();
    assert!(first_revision > 0);
    store.delete(&scope(), &first.id, first.revision).unwrap();
    assert!(store.list(&scope(), None).unwrap().is_empty());
    assert_eq!(store.revision().unwrap(), first_revision);
    drop(store);
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    assert_eq!(store.revision().unwrap(), first_revision);
    assert!(store.list(&scope(), None).unwrap().is_empty());
    let second = store.create(&scope(), draft()).unwrap();
    assert_ne!(first.id, second.id);
    let second_revision = store.revision().unwrap();
    assert!(second_revision > first_revision);
    store.delete(&scope(), &second.id, second.revision).unwrap();
    assert!(store.list(&scope(), None).unwrap().is_empty());
    drop(store);
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    assert_eq!(store.revision().unwrap(), second_revision);
}

#[test]
fn idempotent_create_and_rejected_mutations_do_not_advance_watermark() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteSubagentProfiles::open(&directory.path().join("profiles.sqlite3")).unwrap();
    let first = store.create(&scope(), draft()).unwrap();
    let revision = store.revision().unwrap();
    assert_eq!(store.create(&scope(), draft()).unwrap(), first);
    assert_eq!(store.revision().unwrap(), revision);
    let mut changed = draft();
    changed.instructions = "不同角色".into();
    assert_eq!(
        store.create(&scope(), changed),
        Err(SubagentProfileError::Conflict)
    );
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(
        store.delete(&scope(), &first.id, first.revision + 1),
        Err(SubagentProfileError::Conflict)
    );
    assert_eq!(store.revision().unwrap(), revision);
    let other = ContextScope {
        user_id: "other".into(),
        ..scope()
    };
    assert_eq!(
        store.delete(&other, &first.id, first.revision),
        Err(SubagentProfileError::NotFound)
    );
    assert_eq!(store.revision().unwrap(), revision);
    store.create(&other, draft()).unwrap();
    assert!(store.revision().unwrap() > revision);
    assert_eq!(store.list(&scope(), None).unwrap(), vec![first]);
}
