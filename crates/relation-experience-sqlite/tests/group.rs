use eve_chime_api::*;
use eve_relation_experience_sqlite::SqliteGroupRelations;
fn message(id: &str, group: &str) -> GroupMessage {
    GroupMessage {
        id: id.into(),
        conversation: group.into(),
        speaker: "member".into(),
        speaker_name: None,
        text: "那逻辑处理器要怎么接".into(),
        mentions_me: false,
        replies_to_me: false,
        from_me: false,
        at_ms: 1,
    }
}
fn example(m: &GroupMessage) -> GroupRelationExample {
    GroupRelationExample {
        message: m.text.clone(),
        exchange: None,
        decision: GroupRelationDecision {
            kind: GroupRelationKind::MindustryQuestion,
            confidence: 95,
            needs_reply: true,
            topic: None,
            reason: "后续说明了是 Mindustry 问题".into(),
        },
        evidence: vec!["later-user".into()],
    }
}
#[test]
fn uncertain_batches_and_restart_preserve_pending_without_replaying() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("group.db");
    let store = SqliteGroupRelations::open(&path).unwrap();
    for id in ["a", "b"] {
        let m = message(id, "g");
        store.stage(&m).unwrap();
        store
            .record(&m, &GroupRelationDecision::uncertain("含混"))
            .unwrap();
    }
    assert_eq!(store.claim_active().unwrap().unwrap().id, "b");
    assert!(store.claim_active().unwrap().is_none());
    assert_eq!(store.pending("g", "r1", 4).unwrap().len(), 2);
    store.stage(&message("interrupted", "g")).unwrap();
    store
        .record(
            &message("waiting", "g"),
            &GroupRelationDecision::uncertain("等待主动系统"),
        )
        .unwrap();
    drop(store);
    let store = SqliteGroupRelations::open(&path).unwrap();
    assert!(store.claim_active().unwrap().is_none());
    assert_eq!(store.pending("g", "r1", 10).unwrap().len(), 4);
    store.reviewed(&message("a", "g"), "r1", None).unwrap();
    assert_eq!(store.pending("g", "r1", 10).unwrap().len(), 3);
    assert_eq!(store.pending("g", "r2", 10).unwrap().len(), 4);
}
#[test]
fn verified_experience_is_atomic_searchable_and_group_scoped() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("group.db");
    let store = SqliteGroupRelations::open(&path).unwrap();
    let m = message("a", "g");
    store
        .record(&m, &GroupRelationDecision::uncertain("含混"))
        .unwrap();
    store.reviewed(&m, "r1", Some(&example(&m))).unwrap();
    assert!(store.pending("g", "r2", 4).unwrap().is_empty());
    assert_eq!(store.recall(&message("next", "g")).unwrap().len(), 1);
    assert!(
        store
            .recall(&message("next", "other-group"))
            .unwrap()
            .is_empty()
    );
    // 重复整理不重复加经验。
    store.reviewed(&m, "r1", Some(&example(&m))).unwrap();
    assert_eq!(store.recall(&message("next", "g")).unwrap().len(), 1);
    let bad = message("bad", "g");
    store
        .record(&bad, &GroupRelationDecision::uncertain("含混"))
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse BEFORE INSERT ON experiences BEGIN SELECT RAISE(ABORT,'test'); END;",
    )
    .unwrap();
    assert!(store.reviewed(&bad, "r1", Some(&example(&bad))).is_err());
    assert_eq!(store.pending("g", "r1", 4).unwrap().len(), 1);
    assert_eq!(
        db.query_row("SELECT count(*) FROM experiences", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn confident_and_mentioned_messages_do_not_enter_active_batch() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteGroupRelations::open(&root.path().join("db")).unwrap();
    let m = message("a", "g");
    store.stage(&m).unwrap();
    store.record(&m, &example(&m).decision).unwrap();
    assert!(store.claim_active().unwrap().is_none());
    assert!(store.pending_groups().unwrap().is_empty());
    let m = message("at", "g");
    store.stage(&m).unwrap();
    store
        .record(&m, &GroupRelationDecision::uncertain("含混"))
        .unwrap();
    store.suppress_active(&m).unwrap();
    assert!(store.claim_active().unwrap().is_none());
    assert_eq!(store.pending_groups().unwrap(), ["g"]);
}
#[test]
fn malformed_empty_missing_and_future_database_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("empty", b"".as_slice()),
        ("broken", b"not sqlite".as_slice()),
    ] {
        let path = root.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        assert!(SqliteGroupRelations::open(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let path = root.path().join("future");
    drop(SqliteGroupRelations::open(&path).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE metadata SET version=99", []).unwrap();
    drop(db);
    let bytes = std::fs::read(&path).unwrap();
    assert!(SqliteGroupRelations::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::rename(&path, root.path().join("saved")).unwrap();
    assert!(SqliteGroupRelations::open(&path).is_err());
    assert!(!path.exists());
}
