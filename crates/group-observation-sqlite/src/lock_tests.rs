use super::*;

fn message() -> GroupMessage {
    GroupMessage {
        id: "original-message".into(),
        conversation: "group-1".into(),
        speaker: "member-1".into(),
        speaker_name: None,
        text: "这条原话在交接前后保持完整".into(),
        mentions_me: false,
        replies_to_me: false,
        from_me: false,
        at_ms: 1,
    }
}

#[test]
fn owner_drop_releases_lock_while_duplicate_descriptor_remains_open() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("group.sqlite3");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let original = message();
    assert!(store.append(&original).unwrap());
    let before = std::fs::read(&path).unwrap();
    // 与 fork 暂留描述符相同，共享原锁；不靠并行测试或等待触发竞态。
    let duplicate = store._exclusive.0.try_clone().unwrap();
    assert_eq!(
        SqliteGroupObservations::open(&path).err().as_deref(),
        Some("群消息数据库已由另一宿主持有")
    );

    drop(store);
    let reopened = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(reopened.recent("group-1", 10).unwrap(), [original.clone()]);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        SqliteGroupObservations::open(&path).err().as_deref(),
        Some("群消息数据库已由另一宿主持有")
    );

    drop(duplicate);
    assert!(SqliteGroupObservations::open(&path).is_err());
    assert!(reopened.claim().unwrap().is_none());
    drop(reopened);
    let restored = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(restored.recent("group-1", 10).unwrap(), [original]);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn failed_restore_releases_lock_with_duplicate_descriptor_and_preserves_database() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("group.sqlite3");
    let marker = path.with_extension("required");
    let bytes = b"broken database must remain unchanged";
    std::fs::write(&path, bytes).unwrap();
    let lock_path = path.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    file.try_lock().unwrap();
    let exclusive = ExclusiveLock(file);
    let duplicate = exclusive.0.try_clone().unwrap();
    assert!(SqliteGroupObservations::open(&path).is_err());

    // 调用生产路径中取得锁后的同一个恢复函数，损坏库在持锁后失败。
    assert_eq!(
        SqliteGroupObservations::open_locked(&path, &marker, true, exclusive)
            .err()
            .as_deref(),
        Some(BROKEN)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!marker.exists());
    let file = std::fs::File::open(&lock_path).unwrap();
    file.try_lock().unwrap();
    let reclaimed = ExclusiveLock(file);
    let contender = std::fs::File::open(&lock_path).unwrap();
    assert!(contender.try_lock().is_err());
    drop(duplicate);
    assert!(contender.try_lock().is_err());
    drop(reclaimed);
    contender.try_lock().unwrap();
    contender.unlock().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
