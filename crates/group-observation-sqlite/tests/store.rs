use eve_chime_api::*;
use eve_group_observation_sqlite::SqliteGroupObservations;
fn message(group: &str) -> GroupMessage {
    GroupMessage {
        id: "m1".into(),
        conversation: group.into(),
        speaker: "user-1".into(),
        speaker_name: None,
        text: "这句说法挺自然".into(),
        mentions_me: false,
        replies_to_me: false,
        from_me: false,
        at_ms: 1,
    }
}
fn reply(group: &str) -> ChimeReply {
    ChimeReply {
        conversation: group.into(),
        reply_to: "m1".into(),
        text: "我也觉得".into(),
        parts: vec![ChimePart {
            text: "我也觉得".into(),
            pause_before_ms: 0,
        }],
    }
}
#[test]
fn observations_deduplicate_and_restart_never_replays_unknown_sends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let mut m = message("g1");
    assert!(store.append(&m).unwrap());
    m.at_ms = 2;
    assert!(!store.append(&m).unwrap());
    assert!(store.recent("g2", 10).unwrap().is_empty());
    assert!(store.queue("chime-1", &reply("g1"), "other-user").is_err());
    assert!(store.queue("chime-2", &reply("g2"), "user-1").is_err());
    store.queue("chime-1", &reply("g1"), "user-1").unwrap();
    let claimed = store.claim().unwrap().unwrap();
    assert_eq!(claimed.speaker, "user-1");
    assert!(store.claim().unwrap().is_none());
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert!(store.claim().unwrap().is_none());
    assert!(store.outbound("chime-1").unwrap().is_none());
    assert!(store.queue("chime-again", &reply("g1"), "user-1").is_err());
    assert_eq!(store.recent("g1", 10).unwrap().len(), 1);
    m.text = "另一条正文".into();
    assert!(store.append(&m).is_err());
    assert_eq!(store.recent("g1", 10).unwrap()[0].text, "这句说法挺自然");
}
#[test]
fn confirmed_and_abandoned_sends_never_reenter_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteGroupObservations::open(&dir.path().join("group.db")).unwrap();
    store.append(&message("g1")).unwrap();
    store.append(&message("g2")).unwrap();
    store.queue("chime-1", &reply("g1"), "user-1").unwrap();
    store.claim().unwrap().unwrap();
    store.finish("chime-1", true).unwrap();
    assert!(store.outbound("chime-1").unwrap().is_none());
    assert!(store.finish("chime-1", true).is_err());
    store.queue("chime-2", &reply("g2"), "user-1").unwrap();
    store.abandon("chime-2").unwrap();
    assert!(store.claim().unwrap().is_none());
}
#[test]
fn corrupted_or_empty_existing_files_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    for bytes in [b"broken database".as_slice(), b"".as_slice()] {
        let path = dir
            .path()
            .join(if bytes.is_empty() { "empty" } else { "corrupt" });
        std::fs::write(&path, bytes).unwrap();
        assert!(SqliteGroupObservations::open(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn missing_database_and_unknown_version_do_not_silently_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    drop(store);
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute("UPDATE metadata SET version=99", []).unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(SqliteGroupObservations::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::rename(&path, dir.path().join("preserved.db")).unwrap();
    assert!(SqliteGroupObservations::open(&path).is_err());
    assert!(!path.exists());
}

#[test]
fn full_participation_survives_window_and_restart_and_rejects_cross_group_links() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let source = message("g1");
    store.append(&source).unwrap();
    let mut own = source.clone();
    own.id = "eve-reply".into();
    own.speaker = "eve".into();
    own.from_me = true;
    own.text = "完整的原始回复".repeat(300);
    own.at_ms = 2;
    store.append(&own).unwrap();
    store.link_reply("g1", &source.id, &own.id).unwrap();
    for n in 0..120 {
        let mut m = source.clone();
        m.id = format!("noise-{n}");
        store.append(&m).unwrap();
    }
    assert!(
        store
            .recent("g1", 100)
            .unwrap()
            .iter()
            .all(|m| m.id != source.id)
    );
    assert_eq!(store.exchanges("g1").unwrap()[0].reply.text, own.text);
    assert!(store.link_reply("g2", &source.id, &own.id).is_err());
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(store.exchanges("g1").unwrap()[0].source, source);
    assert!(store.exchanges("g2").unwrap().is_empty());
}

#[test]
fn legacy_confirmed_pushes_recover_complete_questions_and_answers_but_not_unknown_sends() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("legacy.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    store.append(&message("g1")).unwrap();
    store.append(&message("g2")).unwrap();
    store
        .queue("chime-confirmed", &reply("g1"), "user-1")
        .unwrap();
    store.claim().unwrap().unwrap();
    store.finish("chime-confirmed", true).unwrap();
    store
        .queue("chime-unknown", &reply("g2"), "user-1")
        .unwrap();
    store.claim().unwrap().unwrap();
    drop(store);
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE exchanges; DELETE FROM observations WHERE id='chime-confirmed:reply'; UPDATE metadata SET version=1;").unwrap();
    }
    let store = SqliteGroupObservations::open(&path).unwrap();
    let recovered = store.exchanges("g1").unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].source, message("g1"));
    assert_eq!(recovered[0].reply.text, reply("g1").text);
    assert!(store.exchanges("g2").unwrap().is_empty());
    assert!(store.claim().unwrap().is_none());
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(store.exchanges("g1").unwrap().len(), 1);
}

#[test]
fn observations_are_read_incrementally_by_sequence_within_one_group() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteGroupObservations::open(&dir.path().join("group.db")).unwrap();
    for (index, group) in ["g1", "g2", "g1", "g1"].into_iter().enumerate() {
        let mut m = message(group);
        m.id = format!("m{index}");
        store.append(&m).unwrap();
    }
    let all = store.after("g1", 0, 10).unwrap();
    assert_eq!(
        all.iter()
            .map(|o| o.message.id.as_str())
            .collect::<Vec<_>>(),
        ["m0", "m2", "m3"]
    );
    assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
    let rest = store.after("g1", all[0].seq, 1).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].message.id, "m2");
    assert!(store.after("g1", all[2].seq, 10).unwrap().is_empty());
}

#[test]
fn raw_search_pages_all_saved_messages_without_crossing_groups_or_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    for i in 0..125 {
        let mut m = message("g1");
        m.id = format!("m{i}");
        m.text = format!("服务器地址 MDT.example.com {i} 100%_");
        store.append(&m).unwrap();
    }
    let mut foreign = message("g2");
    foreign.text = "服务器地址 SECRET".into();
    store.append(&foreign).unwrap();
    let first = store.search("g1", "服务器地址", None, 2).unwrap();
    assert_eq!(
        first
            .iter()
            .map(|m| m.message.id.as_str())
            .collect::<Vec<_>>(),
        ["m124", "m123"]
    );
    let next = store
        .search("g1", "mdt.EXAMPLE", Some(first[1].seq), 2)
        .unwrap();
    assert_eq!(next[0].message.id, "m122");
    assert_eq!(next[1].message.id, "m121");
    assert!(store.search("g1", "SECRET", None, 20).unwrap().is_empty());
    assert_eq!(store.search("g1", "100%_", None, 2).unwrap().len(), 2);
    assert!(
        store
            .search("g1", "' OR 1=1 --", None, 2)
            .unwrap()
            .is_empty()
    );
    assert!(store.search("g1", "", Some(1), 2).unwrap().is_empty());
    assert!(store.search("g1", "", Some(u64::MAX), 2).is_err());
    let oldest = store.search("g1", "服务器地址", Some(2), 2).unwrap();
    assert_eq!(oldest[0].message.id, "m0");
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(
        store.search("g1", "服务器地址", Some(2), 2).unwrap()[0],
        oldest[0]
    );
}

#[test]
fn trusted_ids_anchor_repeated_text_attachment_and_late_messages_by_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteGroupObservations::open(&dir.path().join("group.db")).unwrap();
    let mut first = message("g1");
    first.id = "same-text-first".into();
    first.text = "重复原话".into();
    first.at_ms = 100;
    store.append(&first).unwrap();
    let mut anchor = first.clone();
    anchor.id = "same-text-current".into();
    anchor.at_ms = 1; // 平台时间不作为可靠顺序。
    store.append(&anchor).unwrap();
    let cutoff = store
        .window_ending_at("g1", &[anchor.id.clone()], 20)
        .unwrap()
        .cutoff_seq;
    for n in 0..25 {
        let mut late = first.clone();
        late.id = format!("late-{n}");
        late.speaker = format!("member-{n}");
        late.text = "后发新话题".into();
        store.append(&late).unwrap();
    }
    let frozen = store
        .window_ending_at("g1", &[anchor.id.clone()], 20)
        .unwrap();
    assert_eq!(frozen.cutoff_seq, cutoff);
    assert_eq!(frozen.messages.len(), 2);
    assert_eq!(frozen.messages.last().unwrap().message.id, anchor.id);
    assert!(
        frozen
            .messages
            .iter()
            .all(|row| !row.message.id.starts_with("late-"))
    );
    let mut attachment = first.clone();
    attachment.id = "pure-attachment-current".into();
    attachment.text.clear();
    store.append(&attachment).unwrap();
    for n in 0..25 {
        let mut late = first.clone();
        late.id = format!("post-attachment-{n}");
        store.append(&late).unwrap();
    }
    let frozen = store
        .window_ending_at("g1", &[attachment.id.clone()], 20)
        .unwrap();
    assert_eq!(frozen.messages.last().unwrap().message.id, attachment.id);
    assert!(frozen.messages.last().unwrap().message.text.is_empty());
    assert!(
        frozen
            .messages
            .iter()
            .all(|row| !row.message.id.starts_with("post-attachment"))
    );
}

#[test]
fn merged_batch_keeps_all_anchor_ids_even_when_other_members_fill_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteGroupObservations::open(&dir.path().join("group.db")).unwrap();
    let mut first = message("g1");
    first.id = "batch-first".into();
    store.append(&first).unwrap();
    for n in 0..30 {
        let mut other = first.clone();
        other.id = format!("other-{n}");
        other.speaker = "other-member".into();
        store.append(&other).unwrap();
    }
    let mut last = first.clone();
    last.id = "batch-primary".into();
    store.append(&last).unwrap();
    let ids = vec![first.id.clone(), last.id.clone()];
    let frozen = store.window_ending_at("g1", &ids, 20).unwrap();
    assert_eq!(frozen.messages.len(), 20);
    assert_eq!(frozen.messages.first().unwrap().message.id, first.id);
    assert_eq!(frozen.messages.last().unwrap().message.id, last.id);
    assert!(
        frozen
            .messages
            .iter()
            .any(|row| row.message.speaker == "other-member")
    );
    assert!(store.window_ending_at("g2", &ids, 20).is_err());
    assert!(
        store
            .window_ending_at("g1", &["missing-id".into()], 20)
            .is_err()
    );
    assert!(
        store
            .window_ending_at("g1", &[last.id.clone(), first.id.clone()], 20)
            .is_err()
    );
    assert!(
        store
            .window_ending_at("g1", &[last.id.clone(), last.id], 20)
            .is_err()
    );
    assert!(store.window_ending_at("g1", &[], 20).is_err());
}

#[test]
fn explicit_unknown_delivery_preserves_unknown_without_exchange_or_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    store.append(&message("g1")).unwrap();
    store
        .queue("chime-unknown-send", &reply("g1"), "user-1")
        .unwrap();
    store.claim().unwrap().unwrap();
    store.finish_unknown("chime-unknown-send").unwrap();
    assert!(store.claim().unwrap().is_none());
    assert!(store.exchanges("g1").unwrap().is_empty());
    assert!(store.finish("chime-unknown-send", true).is_err());
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    let status: String = db
        .query_row(
            "SELECT status FROM outbound WHERE id='chime-unknown-send'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "unknown");
    drop(db);
    let reopened = SqliteGroupObservations::open(&path).unwrap();
    assert!(reopened.claim().unwrap().is_none());
    assert!(reopened.exchanges("g1").unwrap().is_empty());
}
