use eve_subject_api::*;
use eve_subject_sqlite::SqliteIdleMemory;
use ring::digest::{SHA256, digest};
use rusqlite::Connection;

fn scope() -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: "private-one".into(),
        principal_id: "owner".into(),
    }
}

fn sha256(text: &str) -> String {
    digest(&SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn excerpt(text: &str) -> ExplorationExcerpt {
    let mut end = text.len().min(MAX_EXPLORATION_EXCERPT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    ExplorationExcerpt {
        text: text[..end].into(),
        original_bytes: text.len() as u64,
        sha256: sha256(text),
        truncated: end < text.len(),
    }
}

fn experience(session: &str) -> ExplorationExperience {
    ExplorationExperience {
        session_id: session.into(),
        scope: scope(),
        started_at_ms: 100,
        ended_at_ms: 200,
        end_reason: ExplorationEndReason::Natural,
        tools: vec![ExplorationToolExperience {
            call_id: format!("call-{session}"),
            tool: "read_file".into(),
            arguments: excerpt(r#"{"path":"ideas/random.txt"}"#),
            output: Some(excerpt("实际读取：蓝色方格，试着看看相邻的随机资料")),
            outcome: ExplorationToolOutcome::Succeeded,
            error: None,
        }],
        total_tool_calls: 1,
        tool_offset: 0,
        tools_truncated: false,
        subjective_notes: Some(excerpt("这些颜色让人想到海边，还想随便看看。")),
    }
}

fn observation(store: &SqliteIdleMemory) -> IdleObservation {
    let mind = store.snapshot(&scope()).unwrap();
    IdleObservation {
        scope: scope(),
        revision: mind.revision,
        interests: vec!["自由探索".into()],
        self_interests: mind.self_interests,
        interest_consumed: Default::default(),
        evidence: vec![],
        judgments: mind.judgments,
    }
}

fn exploration() -> IdleReflection {
    IdleReflection {
        choice: IdleChoice::Explore,
        thought: "想逛逛".into(),
        reason: "有点好奇".into(),
        judgments: vec![],
        interest_updates: vec![],
        action: None,
        exploration: None,
        revisit_after_minutes: 30,
    }
}

fn evidence(experience: &ExplorationExperience) -> IdleEvidence {
    IdleEvidence {
        id: experience.session_id.clone(),
        kind: if experience.has_external_evidence() {
            "exploration_experience"
        } else {
            "exploration_reflection"
        }
        .into(),
        text: serde_json::to_string(experience).unwrap(),
        excerpt: experience.tools_truncated,
    }
}

fn downgrade_to_two(path: &std::path::Path) {
    Connection::open(path)
        .unwrap()
        .execute_batch("DROP TABLE exploration_sources; DROP TABLE exploration_experiences; UPDATE metadata SET version=2;")
        .unwrap();
}

#[test]
fn experiences_survive_restart_are_globally_idempotent_and_keep_raw_receipts() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let first = experience("session-one");
    store.record_exploration(&first).unwrap();
    store.record_exploration(&first).unwrap();
    let mut changed = first.clone();
    changed.subjective_notes = Some(excerpt("另一种感想"));
    assert_eq!(
        store.record_exploration(&changed),
        Err(SubjectError::InvalidTransition)
    );
    changed = first.clone();
    changed.scope.conversation_id = "private-other".into();
    assert_eq!(
        store.record_exploration(&changed),
        Err(SubjectError::InvalidTransition)
    );
    assert_eq!(
        store
            .exploration(&changed.scope, &first.session_id)
            .unwrap(),
        None
    );
    drop(store);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    let recovered = reopened
        .exploration(&scope(), &first.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered, first);
    assert!(
        recovered.tools[0]
            .output
            .as_ref()
            .unwrap()
            .text
            .contains("蓝色方格")
    );
    assert_ne!(recovered.tools[0].output, recovered.subjective_notes);
    assert_eq!(reopened.explorations(&scope(), None, 16).unwrap().total, 1);
}

#[test]
fn recent_pagination_keeps_same_millisecond_and_out_of_order_clock_records() {
    let temporary = tempfile::tempdir().unwrap();
    let store = SqliteIdleMemory::open(&temporary.path().join("mind.sqlite3")).unwrap();
    for n in 0..37 {
        let mut value = experience(&format!("session-{n:03}"));
        if n == 36 {
            value.started_at_ms = 1;
            value.ended_at_ms = 2;
        }
        store.record_exploration(&value).unwrap();
    }
    let first = store.explorations(&scope(), None, 5).unwrap();
    assert_eq!(first.items[0].session_id, "session-036");
    let mut ids = first
        .items
        .into_iter()
        .map(|v| v.session_id)
        .collect::<Vec<_>>();
    let mut before = first.next_before;
    store
        .record_exploration(&experience("later-insert"))
        .unwrap();
    while before.is_some() {
        let page = store.explorations(&scope(), before, 5).unwrap();
        assert_eq!(page.total, 38);
        ids.extend(page.items.into_iter().map(|v| v.session_id));
        before = page.next_before;
    }
    assert_eq!(
        ids,
        (0..37)
            .rev()
            .map(|n| format!("session-{n:03}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        store.explorations(&scope(), None, 1).unwrap().items[0].session_id,
        "later-insert"
    );
}

#[test]
fn bounded_long_session_keeps_count_offsets_hashes_and_unknown_receipts() {
    let temporary = tempfile::tempdir().unwrap();
    let store = SqliteIdleMemory::open(&temporary.path().join("mind.sqlite3")).unwrap();
    let mut value = experience("long-session");
    value.end_reason = ExplorationEndReason::Unknown;
    value.tools = (486..550)
        .map(|n| ExplorationToolExperience {
            call_id: format!("call-{n}"),
            tool: "read_file".into(),
            arguments: excerpt(&format!("{{\"path\":\"{n}.txt\"}}")),
            output: if n == 549 {
                None
            } else {
                Some(excerpt(&"真实多字节资料".repeat(550)))
            },
            outcome: if n == 549 {
                ExplorationToolOutcome::Unknown
            } else {
                ExplorationToolOutcome::Succeeded
            },
            error: (n == 549).then(|| excerpt("进程中断，没有确认回执")),
        })
        .collect();
    value.total_tool_calls = 550;
    value.tool_offset = 486;
    value.tools_truncated = true;
    store.record_exploration(&value).unwrap();
    assert_eq!(
        store.exploration(&scope(), &value.session_id).unwrap(),
        Some(value.clone())
    );
    assert!(value.tools[0].output.as_ref().unwrap().truncated);
    assert!(value.tools[0].output.as_ref().unwrap().text.len() <= 2048);
    assert!(value.tools[63].output.is_none());
    let mut wrong = value.clone();
    wrong.tools_truncated = false;
    assert!(store.record_exploration(&wrong).is_err());
    wrong = value.clone();
    wrong.tool_offset = 500;
    assert!(store.record_exploration(&wrong).is_err());
    wrong = value.clone();
    wrong.end_reason = ExplorationEndReason::Natural;
    assert!(store.record_exploration(&wrong).is_err());
    wrong = value.clone();
    wrong.tools[0].arguments.sha256 = "0".repeat(64);
    assert!(store.record_exploration(&wrong).is_err());
    wrong = value;
    wrong.tools[63].output = Some(excerpt("猜想输出"));
    assert!(store.record_exploration(&wrong).is_err());
}

#[test]
fn observation_uses_saved_scope_and_exact_tool_projection_not_invented_facts() {
    let temporary = tempfile::tempdir().unwrap();
    let store = SqliteIdleMemory::open(&temporary.path().join("mind.sqlite3")).unwrap();
    let saved = experience("session-one");
    let mut input = observation(&store);
    input.evidence.push(evidence(&saved));
    assert!(
        store.prepare_observation(&mut input).is_err(),
        "不存在的经历不能进入回顾"
    );
    store.record_exploration(&saved).unwrap();
    store.prepare_observation(&mut input).unwrap();
    let mut forged = saved.clone();
    forged.tools[0].output = Some(excerpt("从未读到的文字"));
    input.evidence[0] = evidence(&forged);
    assert!(store.prepare_observation(&mut input).is_err());
    forged = saved.clone();
    forged.subjective_notes = Some(excerpt("替换感想"));
    input.evidence[0] = evidence(&forged);
    assert!(store.prepare_observation(&mut input).is_err());
    input.evidence[0] = evidence(&saved);
    input.scope.conversation_id = "another-user".into();
    assert!(store.prepare_observation(&mut input).is_err());
    input.scope = scope();
    let mut view = saved.clone();
    view.tools.clear();
    view.tool_offset = 1;
    view.tools_truncated = true;
    input.evidence[0] = evidence(&view);
    store.prepare_observation(&mut input).unwrap();
    input.evidence[0].kind = "exploration_experience".into();
    assert!(
        store.prepare_observation(&mut input).is_err(),
        "没有展示回执的片段不是新外界证据"
    );
    let mut thought = experience("thought-only");
    thought.tools.clear();
    thought.total_tool_calls = 0;
    store.record_exploration(&thought).unwrap();
    input.evidence[0] = evidence(&thought);
    store.prepare_observation(&mut input).unwrap();
}

#[test]
fn one_committed_reflection_reserves_one_session_and_new_curiosity_is_not_deduplicated_by_interest()
{
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    assert!(
        store
            .reserve_exploration_source(&scope(), "missing", "focus-one", 100)
            .is_err()
    );
    let mut wait = exploration();
    wait.choice = IdleChoice::Wait;
    store
        .commit(&observation(&store), &wait, "wait-review", 100)
        .unwrap();
    assert!(
        store
            .reserve_exploration_source(&scope(), "wait-review", "focus-one", 100)
            .is_err()
    );
    store
        .commit(&observation(&store), &exploration(), "review-one", 110)
        .unwrap();
    store
        .reserve_exploration_source(&scope(), "review-one", "focus-one", 120)
        .unwrap();
    store
        .reserve_exploration_source(&scope(), "review-one", "focus-one", 200)
        .unwrap();
    assert_eq!(
        store.reserve_exploration_source(&scope(), "review-one", "focus-two", 200),
        Err(SubjectError::InvalidTransition)
    );
    store
        .commit(&observation(&store), &exploration(), "review-two", 220)
        .unwrap();
    assert_eq!(
        store.reserve_exploration_source(&scope(), "review-two", "focus-one", 230),
        Err(SubjectError::InvalidTransition)
    );
    store
        .reserve_exploration_source(&scope(), "review-two", "focus-two", 230)
        .unwrap();
    drop(store);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    assert_eq!(
        reopened
            .exploration_source(&scope(), "review-one")
            .unwrap()
            .as_deref(),
        Some("focus-one")
    );
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT at_ms FROM exploration_sources WHERE reflection_id='review-one'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        120
    );
}

#[test]
fn version_two_upgrade_preserves_legacy_fingerprints_and_partial_failure_rolls_back() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let input = observation(&store);
    let mut old = exploration();
    old.choice = IdleChoice::Wait;
    let prior = store.commit(&input, &old, "old-review", 100).unwrap();
    store
        .record_host_authorization(&scope(), "operator", &"a".repeat(64), 100)
        .unwrap();
    drop(store);
    downgrade_to_two(&path);
    let db = Connection::open(&path).unwrap();
    let before: (String, String, String) = db
        .query_row(
            "SELECT fingerprint,observation,result FROM reflections",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    db.execute_batch("CREATE TRIGGER reject_upgrade BEFORE UPDATE ON metadata BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    drop(db);
    assert!(matches!(
        SqliteIdleMemory::open(&path),
        Err(SubjectError::Storage)
    ));
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT version FROM metadata", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'exploration_%'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER reject_upgrade").unwrap();
    drop(db);
    let migrated = SqliteIdleMemory::open(&path).unwrap();
    assert!(migrated.host_authorization_seen(&scope()).unwrap());
    let replay = migrated.commit(&input, &old, "old-review", 900).unwrap();
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(prior).unwrap()
    );
    let db = Connection::open(&path).unwrap();
    let after: (String, String, String) = db
        .query_row(
            "SELECT fingerprint,observation,result FROM reflections",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(before, after);
    assert!(!after.2.contains("exploration"));
    assert_eq!(
        db.query_row("SELECT version FROM metadata", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn corrupt_legacy_payload_or_receipt_is_never_rewritten_by_migration() {
    for corrupt_sql in [
        "UPDATE minds SET payload='broken'",
        "UPDATE reflections SET fingerprint='forged'",
        "UPDATE host_authorizations SET digest='forged'",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("mind.sqlite3");
        let store = SqliteIdleMemory::open(&path).unwrap();
        let mut old = exploration();
        old.choice = IdleChoice::Wait;
        store
            .commit(&observation(&store), &old, "old-review", 100)
            .unwrap();
        store
            .record_host_authorization(&scope(), "operator", &"a".repeat(64), 100)
            .unwrap();
        drop(store);
        downgrade_to_two(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(corrupt_sql)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            matches!(
                SqliteIdleMemory::open(&path),
                Err(SubjectError::CorruptState)
            ),
            "{corrupt_sql}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn failed_experience_write_does_not_publish_or_poison_the_saved_session() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_experience BEFORE INSERT ON exploration_experiences BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let value = experience("retry-only-storage");
    assert_eq!(store.record_exploration(&value), Err(SubjectError::Storage));
    assert_eq!(
        store.record_exploration(&value),
        Err(SubjectError::Unavailable)
    );
    drop(store);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM exploration_experiences", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER reject_experience").unwrap();
    drop(db);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    reopened.record_exploration(&value).unwrap();
    assert_eq!(
        reopened.exploration(&scope(), &value.session_id).unwrap(),
        Some(value)
    );
}

#[test]
fn historical_interest_can_explore_and_rest_without_forgetting_consumed_sources() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("mind.sqlite3");
    let store = SqliteIdleMemory::open(&path).unwrap();
    for n in 0..12 {
        let mut input = observation(&store);
        input.evidence.push(IdleEvidence {
            id: format!("conversation-{n}"),
            kind: "completed_conversation".into(),
            text: "真实讨论过的图案".into(),
            excerpt: false,
        });
        let mut result = exploration();
        result.choice = IdleChoice::Reflect;
        result.interest_updates.push(SelfInterest {
            key: format!("interest-{n:03}"),
            topic: format!("图案 {n}"),
            phase: SelfInterestPhase::Curious,
            strength: 50,
            reason: "想多看看".into(),
            want_to_try: "继续阅读相关资料".into(),
            evidence: vec![format!("conversation-{n}")],
        });
        store
            .commit(&input, &result, &format!("formation-{n}"), 100 + n)
            .unwrap();
    }
    let input = observation(&store);
    assert!(
        !input
            .self_interests
            .iter()
            .any(|record| record.current.key == "interest-000")
    );
    let mut result = exploration();
    result.exploration = Some(IdleExploration {
        cue: None,
        interest_key: Some("interest-000".into()),
    });
    store
        .commit(&input, &result, "historical-explore", 200)
        .unwrap();
    let mut resting = store.interests(&scope(), None, 1).unwrap().items[0]
        .current
        .clone();
    assert_eq!(resting.key, "interest-000");
    resting.phase = SelfInterestPhase::Resting;
    resting.strength = 20;
    resting.evidence.clear();
    resting.want_to_try.clear();
    resting.reason = "今天不想继续，先休息".into();
    result.interest_updates = vec![resting];
    store
        .commit(&observation(&store), &result, "subjective-rest", 300)
        .unwrap();
    let history = store.history(&scope(), "interest-000", 0, 16).unwrap();
    assert_eq!(history.items.len(), 2);
    assert_eq!(history.items[1].consumed_evidence, ["conversation-0"]);
    let mut replay = history.items[1].current.clone();
    replay.phase = SelfInterestPhase::Active;
    replay.strength = 90;
    replay.want_to_try = "再试试".into();
    replay.evidence = vec!["conversation-0".into()];
    let mut input = observation(&store);
    input.evidence.push(IdleEvidence {
        id: "conversation-0".into(),
        kind: "completed_conversation".into(),
        text: "同一条旧讨论".into(),
        excerpt: false,
    });
    result.interest_updates = vec![replay];
    assert!(
        store
            .commit(&input, &result, "old-evidence-replay", 400)
            .is_err()
    );
    drop(store);
    let reopened = SqliteIdleMemory::open(&path).unwrap();
    assert_eq!(
        reopened
            .history(&scope(), "interest-000", 0, 16)
            .unwrap()
            .items,
        history.items
    );
}

#[test]
fn exploration_projection_cannot_change_hashes_indices_or_receipt_outcomes() {
    let temporary = tempfile::tempdir().unwrap();
    let store = SqliteIdleMemory::open(&temporary.path().join("mind.sqlite3")).unwrap();
    let mut saved = experience("truncated-original");
    saved.tools[0].output = Some(excerpt(&"从真实回执保存的较长资料".repeat(1000)));
    store.record_exploration(&saved).unwrap();
    let mut input = observation(&store);
    let mut correct = evidence(&saved);
    correct.excerpt = true;
    input.evidence.push(correct.clone());
    store.prepare_observation(&mut input).unwrap();
    for index in 0..4 {
        let mut changed = saved.clone();
        match index {
            0 => changed.tools[0].output.as_mut().unwrap().sha256 = "0".repeat(64),
            1 => changed.tools[0].call_id = "some-other-call".into(),
            2 => changed.tools[0].output.as_mut().unwrap().original_bytes += 1,
            _ => changed.tools[0].outcome = ExplorationToolOutcome::Failed,
        }
        input.evidence[0] = evidence(&changed);
        input.evidence[0].excerpt = true;
        assert!(store.prepare_observation(&mut input).is_err());
    }
    correct.excerpt = false;
    input.evidence[0] = correct;
    assert!(store.prepare_observation(&mut input).is_err());
}

#[test]
fn damaged_experience_index_or_payload_closes_store_and_preserves_evidence() {
    for update in [
        "UPDATE exploration_experiences SET ended_at=999",
        "UPDATE exploration_experiences SET digest='forged'",
        "UPDATE exploration_experiences SET payload='broken'",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("mind.sqlite3");
        let store = SqliteIdleMemory::open(&path).unwrap();
        let saved = experience("real-session");
        store.record_exploration(&saved).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch(update)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            store.exploration(&scope(), &saved.session_id),
            Err(SubjectError::CorruptState)
        );
        assert_eq!(
            store.record_exploration(&saved),
            Err(SubjectError::Unavailable)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
