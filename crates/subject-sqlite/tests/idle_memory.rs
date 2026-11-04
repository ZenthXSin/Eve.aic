use eve_subject_api::*;
use eve_subject_sqlite::SqliteIdleMemory;

fn scope() -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: "one".into(),
        principal_id: "owner".into(),
    }
}
fn observation(store: &SqliteIdleMemory, evidence: &str) -> IdleObservation {
    let mind = store.snapshot(&scope()).unwrap();
    let mut observation = IdleObservation {
        scope: scope(),
        revision: mind.revision,
        interests: vec!["自由探索".into()],
        self_interests: mind.self_interests,
        interest_consumed: Default::default(),
        judgments: mind.judgments,
        evidence: vec![IdleEvidence {
            id: evidence.into(),
            kind: "completed_conversation".into(),
            text: "实际讨论了不同的图案与规律".into(),
            excerpt: false,
        }],
    };
    store.prepare_observation(&mut observation).unwrap();
    observation
}
fn reflection(key: &str, evidence: &str, n: u64) -> IdleReflection {
    IdleReflection {
        choice: IdleChoice::Reflect,
        thought: "还想试一试不同规则".into(),
        reason: "这次有不同的经历".into(),
        judgments: vec![],
        action: None,
        revisit_after_minutes: 30,
        interest_updates: vec![SelfInterest {
            key: key.into(),
            topic: format!("规则-{key}"),
            phase: if n.is_multiple_of(2) {
                SelfInterestPhase::Active
            } else {
                SelfInterestPhase::Resting
            },
            strength: 60,
            reason: format!("第{n}次实际比较改变了看法"),
            want_to_try: if n.is_multiple_of(2) {
                "比较另一种规则".into()
            } else {
                String::new()
            },
            evidence: vec![evidence.into()],
        }],
    }
}

#[test]
fn persistent_interests_outgrow_context_and_history_outgrows_old_lifetime_limit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    for n in 0..80 {
        let evidence = format!("conversation-{n}");
        let input = observation(&store, &evidence);
        store
            .commit(
                &input,
                &reflection(&format!("topic-{n:03}"), &evidence, n),
                &format!("task-{n}"),
                100 + n,
            )
            .unwrap();
    }
    let mut after = None;
    let mut items = vec![];
    loop {
        let page = store.interests(&scope(), after.as_deref(), 13).unwrap();
        assert_eq!(page.total, 80);
        assert_eq!(page.revision, 80);
        items.extend(page.items);
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(items.len(), 80);
    assert!(store.snapshot(&scope()).unwrap().self_interests.len() <= MAX_SELF_INTERESTS);
    for n in 0..80 {
        let evidence = format!("new-{n}");
        let input = observation(&store, &evidence);
        store
            .commit(
                &input,
                &reflection("topic-000", &evidence, n),
                &format!("change-{n}"),
                1000 + n,
            )
            .unwrap();
    }
    drop(store);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    assert_eq!(reopened.snapshot(&scope()).unwrap().revision, 160);
    let mut after = 0;
    let mut history = vec![];
    loop {
        let page = reopened.history(&scope(), "topic-000", after, 16).unwrap();
        history.extend(page.items);
        if let Some(next) = page.next_after {
            after = next;
        } else {
            break;
        }
    }
    assert_eq!(history.len(), 81);
    assert_eq!(history.first().unwrap().revision, 1);
    assert_eq!(history.last().unwrap().revision, 81);
    assert!(history.iter().all(|r| r.first_recorded_at_ms == 100));
    let input = observation(&reopened, "conversation-0");
    let before = reopened.snapshot(&scope()).unwrap().revision;
    assert!(
        reopened
            .commit(
                &input,
                &reflection("topic-000", "conversation-0", 999),
                "reuse-old",
                2000
            )
            .is_err()
    );
    assert_eq!(reopened.snapshot(&scope()).unwrap().revision, before);
    let other = SubjectScope {
        conversation_id: "other".into(),
        ..scope()
    };
    assert_eq!(reopened.interests(&other, None, 16).unwrap().total, 0);
    assert!(
        reopened
            .history(&other, "topic-000", 0, 16)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn replay_is_idempotent_and_partial_write_failure_keeps_original_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let input = observation(&store, "first");
    let output = reflection("patterns", "first", 0);
    let first = store.commit(&input, &output, "first-review", 100).unwrap();
    let again = store
        .commit(&input, &output, "first-review", 10000)
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(again).unwrap()
    );
    assert!(
        store
            .commit(
                &input,
                &reflection("patterns", "first", 1),
                "first-review",
                10100
            )
            .is_err()
    );
    let admin = rusqlite::Connection::open(&path).unwrap();
    admin.execute_batch("CREATE TRIGGER fail_interest_history BEFORE INSERT ON interest_history BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let next = observation(&store, "second");
    assert!(matches!(
        store.commit(
            &next,
            &reflection("patterns", "second", 1),
            "second-review",
            10200
        ),
        Err(SubjectError::Storage)
    ));
    assert!(
        store.snapshot(&scope()).is_err(),
        "失败后不能继续用旧缓存写入"
    );
    drop(store);
    admin
        .execute_batch("DROP TRIGGER fail_interest_history")
        .unwrap();
    drop(admin);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    assert_eq!(reopened.snapshot(&scope()).unwrap().revision, 1);
    assert_eq!(
        reopened
            .history(&scope(), "patterns", 0, 16)
            .unwrap()
            .items
            .len(),
        1
    );
    reopened
        .commit(
            &next,
            &reflection("patterns", "second", 1),
            "second-review",
            10200,
        )
        .unwrap();
    assert_eq!(reopened.snapshot(&scope()).unwrap().revision, 2);
}

#[test]
fn old_topic_alias_and_reflection_only_updates_do_not_invent_experiences() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let input = observation(&store, "first");
    store
        .commit(&input, &reflection("patterns", "first", 0), "review-1", 100)
        .unwrap();
    let mut input = observation(&store, "second");
    let mut output = reflection("alias", "second", 2);
    output.interest_updates[0].topic = " 规则-patterns ".into();
    assert!(store.commit(&input, &output, "alias", 200).is_err());
    output = reflection("patterns", "second", 2);
    input.evidence[0].kind = "self_reflection".into();
    assert!(store.commit(&input, &output, "self-repeat", 200).is_err());
    input = observation(&store, "second");
    output = reflection("patterns", "second", 1);
    output.choice = IdleChoice::Act;
    output.action=Some(serde_json::from_value(serde_json::json!({"title":"看图案","goal":"生成一次图案","interest_key":"patterns","assessment":{"problem_key":"one","expected_gain":"体验规则","verification":"读取图案文件","reuse_plan":"之后比较","benefit":80,"confidence":80,"cost":10,"risk":0,"evidence":["second"]}})).unwrap());
    assert!(
        store
            .commit(&input, &output, "resting-action", 200)
            .is_err()
    );
    assert_eq!(store.snapshot(&scope()).unwrap().revision, 1);
}

#[test]
fn legacy_import_preserves_timestamps_and_corruption_is_never_reset() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let input = reflection("patterns", "first", 0);
    let legacy =
        eve_subject_plugin::revise_idle_mind(IdleMind::default(), &input, "old-review", 100)
            .unwrap();
    store.import_legacy(&scope(), &legacy).unwrap();
    let current = store.snapshot(&scope()).unwrap();
    assert!(current.archived);
    assert_eq!(current.next_at_ms, legacy.next_at_ms);
    assert_eq!(current.self_interests, legacy.self_interests);
    assert!(store.import_legacy(&scope(), &legacy).is_err());
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE minds SET payload='broken'", []).unwrap();
    drop(db);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    assert!(reopened.snapshot(&scope()).is_err());
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT payload FROM minds", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "broken"
    );
}

#[test]
fn missing_database_and_competing_host_cannot_start_with_an_empty_history() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let input = observation(&store, "first");
    store
        .commit(&input, &reflection("patterns", "first", 0), "review", 100)
        .unwrap();
    assert!(matches!(
        SqliteIdleMemory::open(&path),
        Err(SubjectError::Unavailable)
    ));
    drop(store);
    let preserved = temp.path().join("preserved.sqlite3");
    std::fs::rename(&path, &preserved).unwrap();
    assert!(matches!(
        SqliteIdleMemory::open(&path),
        Err(SubjectError::CorruptState)
    ));
    assert!(!path.exists(), "缺库不能自动建空库");
    std::fs::rename(preserved, &path).unwrap();
    assert_eq!(
        SqliteIdleMemory::open(&path)
            .unwrap()
            .snapshot(&scope())
            .unwrap()
            .revision,
        1
    );
}

#[test]
fn forged_observation_and_corrupt_index_do_not_overwrite_retained_interests() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let input = observation(&store, "first");
    store
        .commit(&input, &reflection("patterns", "first", 0), "review", 100)
        .unwrap();
    let mut forged = observation(&store, "second");
    forged.self_interests[0].first_recorded_at_ms += 1;
    forged.self_interests[0].updated_at_ms += 1;
    assert!(matches!(
        store.prepare_observation(&mut forged),
        Err(SubjectError::InvalidTransition)
    ));
    assert_eq!(store.snapshot(&scope()).unwrap().revision, 1);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE interests SET revision=revision+1", [])
        .unwrap();
    assert!(matches!(
        store.snapshot(&scope()),
        Err(SubjectError::CorruptState)
    ));
    assert_eq!(
        db.query_row("SELECT revision FROM interests", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn bounded_context_with_many_judgments_never_stops_durable_recording() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    for n in 0..12 {
        let evidence = format!("experience-{n}");
        let input = observation(&store, &evidence);
        let mut output = reflection(&format!("pattern-{n}"), &evidence, n);
        output.thought = "x".repeat(1024);
        output.reason = "x".repeat(768);
        output.interest_updates[0].reason = "x".repeat(384);
        output.judgments = vec![IdleJudgment {
            key: format!("judgment-{n}"),
            statement: "x".repeat(512),
            confidence: 50,
            evidence: vec![evidence],
            revision_reason: "x".repeat(512),
        }];
        let mind = store
            .commit(&input, &output, &format!("review-{n}"), 100 + n)
            .unwrap();
        assert!(serde_json::to_vec(&mind).unwrap().len() <= MAX_PAYLOAD_BYTES);
    }
    assert_eq!(store.interests(&scope(), None, 16).unwrap().total, 12);
}
