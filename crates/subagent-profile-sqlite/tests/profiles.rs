use eve_llm_api::ContextScope;
use eve_subagent_profile_sqlite::SqliteSubagentProfiles;
use eve_task_api::{SubagentProfileDraft, SubagentProfileError, SubagentProfiles};
fn scope() -> ContextScope {
    ContextScope {
        session_id: "private:app:u".into(),
        user_id: "u".into(),
    }
}
fn draft() -> SubagentProfileDraft {
    SubagentProfileDraft {
        name: "编码".into(),
        instructions: "修改并验证当前任务的代码".into(),
        tools: Some(vec!["read_file".into(), "write_file".into()]),
    }
}
#[test]
fn scoped_create_restart_conflict_delete_and_ids_never_reused() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("profiles.sqlite3");
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    let first = store.create(&scope(), draft()).unwrap();
    assert_eq!(
        store.get(&scope(), "agent-18446744073709551615"),
        Err(SubagentProfileError::InvalidInput)
    );
    assert_eq!(store.create(&scope(), draft()).unwrap(), first);
    let mut changed = draft();
    changed.instructions = "不同的职责".into();
    assert_eq!(
        store.create(&scope(), changed),
        Err(SubagentProfileError::Conflict)
    );
    drop(store);
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    assert_eq!(store.get(&scope(), &first.id).unwrap(), first);
    let other = ContextScope {
        user_id: "other".into(),
        ..scope()
    };
    assert!(store.list(&other, None).unwrap().is_empty());
    assert_eq!(
        store.get(&other, &first.id),
        Err(SubagentProfileError::NotFound)
    );
    assert!(store.delete(&other, &first.id, 1).is_err());
    let group = ContextScope {
        session_id: "group:app:u".into(),
        ..scope()
    };
    assert!(store.list(&group, None).unwrap().is_empty());
    assert!(store.delete(&scope(), &first.id, 2).is_err());
    store.delete(&scope(), &first.id, 1).unwrap();
    let second = store.create(&scope(), draft()).unwrap();
    assert_ne!(first.id, second.id);
    assert!(store.get(&scope(), &first.id).is_err());
}
#[test]
fn recursive_tools_and_corrupt_existing_database_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("profiles.sqlite3");
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    for name in [
        "run_subagent",
        "dispatch_background_task",
        "create_subagent",
    ] {
        let mut d = draft();
        d.tools = Some(vec![name.into()]);
        assert_eq!(
            store.create(&scope(), d),
            Err(SubagentProfileError::InvalidInput)
        );
    }
    assert!(store.list(&scope(), None).unwrap().is_empty());
    drop(store);
    let path = root.path().join("corrupt");
    std::fs::write(&path, b"broken database").unwrap();
    assert!(SqliteSubagentProfiles::open(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"broken database");
}
#[test]
fn pagination_does_not_hide_or_drop_older_roles() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSubagentProfiles::open(&root.path().join("profiles.sqlite3")).unwrap();
    for n in 0..70 {
        let mut d = draft();
        d.name = format!("role-{n}");
        store.create(&scope(), d).unwrap();
    }
    let first = store.list(&scope(), None).unwrap();
    assert_eq!(first.len(), 64);
    let second = store.list(&scope(), Some(&first[63].id)).unwrap();
    assert_eq!(second.len(), 6);
    assert_ne!(first[0].id, second[0].id);
}
#[test]
fn escaped_valid_instructions_survive_save_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("profiles.sqlite3");
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    let mut d = draft();
    d.instructions = format!("说明{}", "\u{1}".repeat(8186));
    d.validate().unwrap();
    let saved = store.create(&scope(), d).unwrap();
    drop(store);
    let store = SqliteSubagentProfiles::open(&path).unwrap();
    assert_eq!(store.get(&scope(), &saved.id).unwrap(), saved);
    assert!(store.create(&scope(), draft()).is_err());
    assert_eq!(store.list(&scope(), None).unwrap().len(), 1);
}
