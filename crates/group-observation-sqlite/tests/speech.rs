use eve_chime_api::*;
use eve_group_observation_sqlite::SqliteGroupObservations;
use rusqlite::Connection;
use std::{path::Path, sync::Arc};

fn message(id: &str, group: &str) -> GroupMessage {
    GroupMessage {
        id: id.into(),
        conversation: group.into(),
        speaker: "member-a".into(),
        speaker_name: None,
        text: "还在说这一件事".into(),
        mentions_me: false,
        replies_to_me: false,
        from_me: false,
        at_ms: 1000,
    }
}
fn stage(store: &SqliteGroupObservations, m: &GroupMessage, at: u64) -> GroupSpeechReview {
    store
        .append_speech(
            m,
            &format!("platform-{}", m.id),
            &format!("chime-{}", m.id),
            at,
        )
        .unwrap();
    store.begin_speech(m, at).unwrap().unwrap()
}
fn speak(text: &str, after_ms: u64) -> ChimeSpeechDecision {
    ChimeSpeechDecision::Speak {
        text: text.into(),
        after_ms,
    }
}
fn status(path: &Path, id: &str) -> (String, u64, Option<String>) {
    Connection::open(path).unwrap().query_row("SELECT s.state,s.revision,o.status FROM speech_reviews s LEFT JOIN outbound o ON o.id=s.outbound_id WHERE s.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap()
}

#[test]
fn confirmed_cadence_survives_window_eviction_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("old", "g1");
    let review = stage(&store, &m, 1000);
    store
        .decide_speech(&review, Some(&speak("已经说过的话", 0)), 1000)
        .unwrap();
    let candidate = store.next_authored(1000).unwrap().unwrap();
    assert!(store.claim_authored(&candidate, 1000).unwrap());
    assert_eq!(
        store.reply_cadence("g1", 1000).unwrap(),
        GroupReplyCadence::default()
    );
    store.finish(&candidate.outbound.id, true).unwrap();
    let confirmed = store.reply_cadence("g1", 1000).unwrap();
    let sent_at = confirmed.last_reply_ms.unwrap();
    assert_eq!(confirmed.replies_last_hour, 1);
    for n in 0..30 {
        store.append(&message(&format!("new-{n}"), "g1")).unwrap();
    }
    assert!(store.recent("g1", 20).unwrap().iter().all(|m| !m.from_me));
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(store.reply_cadence("g1", sent_at + 1).unwrap(), confirmed);
    assert_eq!(
        store.reply_cadence("g1", sent_at + 3_600_000).unwrap(),
        GroupReplyCadence {
            last_reply_ms: Some(sent_at),
            replies_last_hour: 0
        }
    );
    assert_eq!(
        store.reply_cadence("g2", sent_at).unwrap(),
        GroupReplyCadence::default()
    );
}

#[test]
fn wait_and_silence_are_durable_decisions_without_delivery_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("wait", "g1");
    let review = stage(&store, &m, 1000);
    assert!(
        store
            .decide_speech(
                &review,
                Some(&ChimeSpeechDecision::Wait { after_ms: 5000 }),
                1000
            )
            .unwrap()
    );
    assert_eq!(status(&path, &m.id), ("waiting".into(), 1, None));
    assert!(store.next_authored(99_000).unwrap().is_none());
    assert!(store.begin_speech(&m, 5999).unwrap().is_none());
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert!(store.pending_speech(5999, 10).unwrap().is_empty());
    assert_eq!(
        store.pending_speech(6000, 10).unwrap(),
        std::slice::from_ref(&m)
    );
    let next = store.begin_speech(&m, 6000).unwrap().unwrap();
    assert_eq!(next.revision, 2);
    assert!(
        !store
            .decide_speech(&review, Some(&speak("旧模型结果", 0)), 6000)
            .unwrap()
    );
    assert!(
        store
            .decide_speech(&next, Some(&ChimeSpeechDecision::Silence {}), 6000)
            .unwrap()
    );
    store.stage_speech(&m, "chime-wait", 6001).unwrap();
    assert_eq!(status(&path, &m.id), ("silence".into(), 2, None));
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert!(store.pending_speech(99999, 10).unwrap().is_empty());
    assert!(store.next_authored(99999).unwrap().is_none());
    assert!(store.exchanges("g1").unwrap().is_empty());
}

#[test]
fn reading_ready_content_does_not_claim_and_exact_cas_claims_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("ready", "g1");
    let review = stage(&store, &m, 1000);
    store
        .decide_speech(
            &review,
            Some(&speak("第一句。第二句保留在同一条。", 2500)),
            1000,
        )
        .unwrap();
    assert!(store.next_authored(3499).unwrap().is_none());
    let candidate = store.next_authored(3500).unwrap().unwrap();
    assert_eq!(candidate.anchor, "platform-ready");
    assert_eq!(candidate.outbound.reply.parts.len(), 1);
    assert_eq!(candidate.outbound.reply.parts[0].pause_before_ms, 0);
    assert_eq!(store.next_authored(4000).unwrap(), Some(candidate.clone()));
    assert!(
        store.claim().unwrap().is_none(),
        "旧 claim 不能抢占自主候选"
    );
    assert_eq!(
        status(&path, &m.id),
        ("ready".into(), 1, Some("queued".into()))
    );
    drop(store);
    let store = Arc::new(SqliteGroupObservations::open(&path).unwrap());
    assert_eq!(store.next_authored(3500).unwrap(), Some(candidate.clone()));
    assert!(!store.claim_authored(&candidate, 3499).unwrap());
    for n in 0..6 {
        let mut wrong = candidate.clone();
        match n {
            0 => wrong.anchor = "fabricated".into(),
            1 => wrong.revision += 1,
            2 => wrong.outbound.reply.text = "改过的正文".into(),
            3 => wrong.outbound.speaker = "member-b".into(),
            4 => wrong.due_at_ms += 1,
            _ => wrong.outbound.reply.parts[0].pause_before_ms = 12,
        }
        assert!(!store.claim_authored(&wrong, 4000).unwrap());
    }
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let candidate = candidate.clone();
            std::thread::spawn(move || store.claim_authored(&candidate, 4000).unwrap())
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|b| *b)
            .count(),
        1
    );
    assert_eq!(
        status(&path, &m.id),
        ("sending".into(), 1, Some("sending".into()))
    );
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert_eq!(
        status(&path, &m.id),
        ("unknown".into(), 1, Some("unknown".into()))
    );
    assert!(store.next_authored(99999).unwrap().is_none());
    assert!(!store.claim_authored(&candidate, 99999).unwrap());
    assert!(store.exchanges("g1").unwrap().is_empty());
}

#[test]
fn any_later_human_invalidates_old_review_without_fabricating_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("old", "g1");
    let review = stage(&store, &m, 1);
    store
        .decide_speech(&review, Some(&speak("旧决定", 0)), 1)
        .unwrap();
    let candidate = store.next_authored(1).unwrap().unwrap();
    // 时间戳回拨、另一成员、正文完全一样，都不能把后发消息误当原观察。
    let mut newer = message("new", "g1");
    newer.at_ms = 0;
    newer.speaker = "member-b".into();
    let next = stage(&store, &newer, 2);
    assert_eq!(
        status(&path, &m.id),
        ("stale".into(), 1, Some("cancelled".into()))
    );
    assert!(!store.claim_authored(&candidate, 2).unwrap());
    assert!(
        !store
            .decide_speech(&review, Some(&speak("晚到结果", 0)), 2)
            .unwrap()
    );
    store
        .decide_speech(
            &next,
            Some(&ChimeSpeechDecision::Wait { after_ms: 1000 }),
            2,
        )
        .unwrap();
    let mut mine = message("own", "g1");
    mine.from_me = true;
    store.append(&mine).unwrap();
    assert_eq!(store.pending_speech(1002, 4).unwrap(), [newer.clone()]);
    let next = store.begin_speech(&newer, 1002).unwrap().unwrap();
    let other = message("other-group", "g2");
    store.append_anchored(&other, "other-platform").unwrap();
    assert!(
        store
            .decide_speech(&next, Some(&speak("新决定", 0)), 1002)
            .unwrap()
    );
    assert_eq!(
        store
            .next_authored(1002)
            .unwrap()
            .unwrap()
            .outbound
            .reply
            .reply_to,
        "new"
    );
    drop(store);
    SqliteGroupObservations::open(&path).unwrap();
}

#[test]
fn commands_advance_trusted_anchor_without_storing_or_replaying_their_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("before-command", "g1");
    let review = stage(&store, &m, 1);
    store
        .decide_speech(&review, Some(&speak("还没发送", 0)), 1)
        .unwrap();
    let candidate = store.next_authored(1).unwrap().unwrap();
    assert!(store.advance_anchor("g1", "command-platform-id").unwrap());
    assert!(!store.claim_authored(&candidate, 2).unwrap());
    assert_eq!(store.recent("g1", 20).unwrap(), std::slice::from_ref(&m));
    assert!(
        !store
            .append_anchored(&m, "platform-before-command")
            .unwrap()
    );
    assert!(!store.advance_anchor("g1", "command-platform-id").unwrap());
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert!(store.next_authored(100000).unwrap().is_none());
    assert_eq!(status(&path, &m.id).2, Some("cancelled".into()));
}

#[test]
fn interrupted_model_review_retries_with_new_revision_and_keeps_payload_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("interrupted", "g1");
    let old = stage(&store, &m, 1);
    assert!(
        SqliteGroupObservations::open(&path).is_err(),
        "同库第二宿主不能恢复第一宿主在审的状态"
    );
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    let due: u64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT due_ms FROM speech_reviews", [], |r| r.get(0))
        .unwrap();
    let next = store.begin_speech(&m, due).unwrap().unwrap();
    assert_eq!(next.revision, old.revision + 1);
    assert!(
        !store
            .decide_speech(&old, Some(&speak("旧结果", 0)), due)
            .unwrap()
    );
    store.decide_speech(&next, None, due).unwrap();
    assert_eq!(status(&path, &m.id), ("waiting".into(), 2, None));
    assert!(store.pending_speech(due + 59999, 10).unwrap().is_empty());
    assert_eq!(store.pending_speech(due + 60000, 10).unwrap(), [m]);
}

#[test]
fn confirmed_delivery_alone_creates_exchange_and_all_terminal_states_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    for (id, group) in [
        ("sent", "g1"),
        ("failed", "g2"),
        ("unknown", "g3"),
        ("abandoned", "g4"),
    ] {
        let m = message(id, group);
        let review = stage(&store, &m, 1);
        store
            .decide_speech(&review, Some(&speak("真实模型决定", 0)), 1)
            .unwrap();
        let candidate = store.authored(&format!("chime-{id}")).unwrap().unwrap();
        if id == "abandoned" {
            store.abandon(&candidate.outbound.id).unwrap();
            continue;
        }
        assert!(store.claim_authored(&candidate, 1).unwrap());
        match id {
            "sent" => store.finish(&candidate.outbound.id, true).unwrap(),
            "failed" => store.finish(&candidate.outbound.id, false).unwrap(),
            _ => store.finish_unknown(&candidate.outbound.id).unwrap(),
        }
    }
    assert_eq!(store.exchanges("g1").unwrap().len(), 1);
    for group in ["g2", "g3", "g4"] {
        assert!(store.exchanges(group).unwrap().is_empty());
    }
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    assert!(store.next_authored(99999).unwrap().is_none());
    assert_eq!(store.exchanges("g1").unwrap().len(), 1);
    assert_eq!(status(&path, "abandoned").2, Some("cancelled".into()));
    assert!(store.finish("chime-unknown", true).is_err());
}

#[test]
fn corrupted_decision_is_rejected_before_recovery_writes_and_version_is_guarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let m = message("bad", "g1");
    let review = stage(&store, &m, 1);
    store
        .decide_speech(&review, Some(&speak("原内容", 0)), 1)
        .unwrap();
    drop(store);
    let db = Connection::open(&path).unwrap();
    let version: u64 = db
        .query_row("SELECT version FROM metadata", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4);
    db.execute("UPDATE speech_reviews SET decision='{}'", [])
        .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(SqliteGroupObservations::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(status(&path, &m.id).2, Some("queued".into()));
}

#[test]
fn stale_candidates_do_not_starve_a_different_group_and_invalid_outputs_create_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    for i in 0..70 {
        let m = message(&format!("old-{i}"), "busy-group");
        let review = stage(&store, &m, 1);
        store
            .decide_speech(&review, Some(&speak("稍后再说", 60000)), 1)
            .unwrap();
    }
    store.advance_anchor("busy-group", "final-command").unwrap();
    let m = message("fair", "another-group");
    let review = stage(&store, &m, 2);
    for invalid in [
        speak("", 0),
        speak(" 空白", 0),
        speak("合规", 60001),
        ChimeSpeechDecision::Wait { after_ms: 0 },
    ] {
        assert!(store.decide_speech(&review, Some(&invalid), 2).is_err());
        assert_eq!(status(&path, &m.id).2, None);
    }
    store
        .decide_speech(&review, Some(&speak("我的完整一句", 0)), 2)
        .unwrap();
    assert_eq!(
        store.next_authored(99999).unwrap().unwrap().outbound.id,
        "chime-fair"
    );
}

#[test]
fn authored_pages_remain_read_only_across_restart_and_exactly_claim_a_later_group() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    for i in 0..72 {
        let m = message(&format!("ready-{i:03}"), &format!("group-{i}"));
        let review = stage(&store, &m, 1);
        store
            .decide_speech(&review, Some(&speak("保持模型原句", 0)), 1)
            .unwrap();
    }
    let first = store.authored_after(1, None, 64).unwrap();
    assert_eq!(first.len(), 64);
    assert_eq!(store.authored_after(1, None, 64).unwrap(), first);
    let cursor = first.last().unwrap().outbound.id.clone();
    drop(store);
    let store = SqliteGroupObservations::open(&path).unwrap();
    let next = store.authored_after(1, Some(&cursor), 64).unwrap();
    assert_eq!(next.len(), 8);
    assert_eq!(next[0].outbound.id, "chime-ready-064");
    assert_eq!(status(&path, "ready-000").2.as_deref(), Some("queued"));
    assert!(store.claim_authored(&next[6], 1).unwrap());
    assert!(!store.claim_authored(&next[6], 1).unwrap());
    assert_eq!(store.authored_after(1, Some(&cursor), 64).unwrap().len(), 7);
}

#[test]
fn source_anchor_and_review_are_one_transaction_even_when_registration_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.db");
    let store = SqliteGroupObservations::open(&path).unwrap();
    let old = message("previous", "group");
    let review = stage(&store, &old, 1);
    store
        .decide_speech(&review, Some(&speak("尚未发送的决定", 0)), 1)
        .unwrap();
    let m = message("new", "group");
    assert!(
        store
            .append_speech(&m, "platform-new", "chime-new", u64::MAX)
            .is_err()
    );
    assert!(store.message("group", "new").unwrap().is_none());
    assert_eq!(store.pending_speech(1, 10).unwrap(), []);
    assert_eq!(
        store.next_authored(1).unwrap().unwrap().outbound.id,
        "chime-previous"
    );
    assert!(
        store
            .append_speech(&m, "platform-new", "chime-new", 1)
            .unwrap()
    );
    assert_eq!(
        store.pending_speech(1, 10).unwrap(),
        std::slice::from_ref(&m)
    );
    assert!(
        !store
            .append_speech(&m, "platform-new", "chime-new", 1)
            .unwrap()
    );
    assert!(store.next_authored(1).unwrap().is_none());
}
