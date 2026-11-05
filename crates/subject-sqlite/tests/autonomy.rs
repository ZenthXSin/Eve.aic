use eve_subject_api::*;
use eve_subject_sqlite::SqliteIdleMemory;

fn scope() -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: "private-one".into(),
        principal_id: "owner".into(),
    }
}

#[test]
fn authorization_and_problem_receipts_survive_restart_and_exceed_hot_capacity() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    assert!(!store.host_authorization_seen(&scope()).unwrap());
    store
        .record_host_authorization(&scope(), "operator-start", &"a".repeat(64), 100)
        .unwrap();
    for n in 0..260 {
        store
            .reserve_problem(
                &scope(),
                &format!("problem-{n}"),
                &format!("auto-1-{n}"),
                101 + n,
            )
            .unwrap();
        store
            .record_task_reconciled(&scope(), &format!("task-{n}"), "completed", 101 + n)
            .unwrap();
    }
    drop(store);
    let store = SqliteIdleMemory::open(&path).unwrap();
    assert!(store.host_authorization_seen(&scope()).unwrap());
    assert_eq!(
        store.record_host_authorization(&scope(), "different-file", &"b".repeat(64), 400),
        Err(SubjectError::InvalidTransition)
    );
    assert_eq!(
        store
            .problem_focus(&scope(), "problem-0")
            .unwrap()
            .as_deref(),
        Some("auto-1-0")
    );
    store
        .reserve_problem(&scope(), "problem-0", "auto-1-0", 400)
        .unwrap();
    assert_eq!(
        store.reserve_problem(&scope(), "problem-0", "auto-2-0", 400),
        Err(SubjectError::InvalidTransition)
    );
    assert!(
        store
            .task_reconciled(&scope(), "task-0", "completed")
            .unwrap()
    );
    assert!(
        !store
            .task_reconciled(&scope(), "task-0", "cancelled")
            .unwrap()
    );
    let other = SubjectScope {
        conversation_id: "another".into(),
        ..scope()
    };
    assert!(!store.host_authorization_seen(&other).unwrap());
    assert_eq!(store.problem_focus(&other, "problem-0").unwrap(), None);
    assert!(
        !store
            .task_reconciled(&other, "task-0", "completed")
            .unwrap()
    );
}

#[test]
fn version_one_migration_is_atomic_and_invalid_old_schema_is_not_rewritten() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    drop(SqliteIdleMemory::open(&path).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE host_authorizations; DROP TABLE autonomy_problems; DROP TABLE reconciled_tasks; UPDATE metadata SET version=1;").unwrap();
    drop(db);
    let migrated = SqliteIdleMemory::open(&path).unwrap();
    migrated
        .reserve_problem(&scope(), "first", "auto-1-0", 100)
        .unwrap();
    drop(migrated);
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT version FROM metadata", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    db.execute_batch("DROP TABLE host_authorizations; DROP TABLE autonomy_problems; DROP TABLE reconciled_tasks; UPDATE metadata SET version=1; DROP TABLE consumed;").unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteIdleMemory::open(&path),
        Err(SubjectError::CorruptState)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn corrupt_authorization_receipt_closes_the_store_without_regranting() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    store
        .record_host_authorization(&scope(), "first", &"a".repeat(64), 100)
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE host_authorizations SET digest='broken'", [])
        .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        store.host_authorization_seen(&scope()),
        Err(SubjectError::CorruptState)
    );
    assert_eq!(
        store.reserve_problem(&scope(), "p", "f", 200),
        Err(SubjectError::Unavailable)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
