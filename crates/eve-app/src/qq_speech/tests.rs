use super::*;
use eve_session_api::SessionKey;

fn event(id: &str) -> QqSpeechEvent {
    QqSpeechEvent {
        id: id.into(),
        session: SessionKey {
            session_id: "qq:session".into(),
            user_id: "qq:owner".into(),
        },
        anchor: "input-1".into(),
        facts: json!({"kind":"task_result","result":"实际完成了一步"}),
    }
}

fn speak() -> QqSpeechDecision {
    QqSpeechDecision::Speak {
        text: "这一项已经核实了".into(),
        after_ms: 0,
    }
}

fn ready(store: &Store, event: &QqSpeechEvent) {
    store.enqueue(event).unwrap();
    store.save(event, "ready", Some(&speak()), 0).unwrap();
}

#[test]
fn waiting_and_silence_survive_restart_without_a_delivery_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("speech.sqlite3");
    let store = Store::open(&path).unwrap();
    let waiting = event("wait");
    let quiet = event("quiet");
    store.enqueue(&waiting).unwrap();
    store
        .save(
            &waiting,
            "waiting",
            Some(&QqSpeechDecision::Wait { after_ms: 60_000 }),
            now() + 60_000,
        )
        .unwrap();
    store.enqueue(&quiet).unwrap();
    store
        .save(
            &quiet,
            "silence",
            Some(&QqSpeechDecision::Silence {}),
            now(),
        )
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.pending().unwrap().is_empty());
    assert_eq!(store.state(&waiting).unwrap().0, "waiting");
    assert!(store.dispatched(&waiting).is_err());
    let mut later = quiet.clone();
    later.anchor = "input-2".into();
    assert!(
        store.silenced(&later).unwrap(),
        "来源清理中断后，沉默决定仍可幂等清理"
    );
    assert!(
        store
            .recent(&ContextScope {
                session_id: quiet.session.session_id,
                user_id: quiet.session.user_id
            })
            .unwrap()
            .is_empty()
    );
}

#[test]
fn new_input_invalidates_ready_text_and_only_one_exact_due_decision_is_claimed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("speech.sqlite3")).unwrap();
    let old = event("changed");
    ready(&store, &old);
    let mut new = old.clone();
    new.anchor = "input-2".into();
    store.enqueue(&new).unwrap();
    assert_eq!(store.state(&new).unwrap().0, "pending");
    assert!(store.dispatched(&old).is_err());
    store
        .save(&new, "ready", Some(&speak()), now() + 60_000)
        .unwrap();
    assert!(store.dispatched(&new).is_err());
    store.save(&new, "ready", Some(&speak()), 0).unwrap();
    store.dispatched(&new).unwrap();
    assert!(store.dispatched(&new).is_err());
    assert!(store.save(&old, "ready", Some(&speak()), 0).is_err());
    let mut forged = new;
    forged.facts = json!({"replacement":"cannot reuse identity"});
    assert!(store.enqueue(&forged).is_err());
}

#[test]
fn recovery_reconsiders_unfinished_thinking_and_never_replays_possible_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("speech.sqlite3");
    let store = Store::open(&path).unwrap();
    let thinking = event("thinking");
    let sending = event("sending");
    store.enqueue(&thinking).unwrap();
    store.save(&thinking, "evaluating", None, 0).unwrap();
    ready(&store, &sending);
    store.dispatched(&sending).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.state(&thinking).unwrap().0, "waiting");
    assert!(store.state(&thinking).unwrap().2 > now());
    assert_eq!(store.state(&sending).unwrap().0, "unknown");
    store.enqueue(&sending).unwrap();
    assert!(store.pending().unwrap().is_empty());
    assert!(store.dispatched(&sending).is_err());
}

#[test]
fn platform_receipts_distinguish_confirmed_unknown_and_known_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("speech.sqlite3")).unwrap();
    for (id, result, expected) in [
        (
            "sent",
            QqOutreachResult::Sent {
                platform_message_id: Some("platform-1".into()),
            },
            "sent",
        ),
        (
            "missing",
            QqOutreachResult::Sent {
                platform_message_id: None,
            },
            "unknown",
        ),
        (
            "invalid",
            QqOutreachResult::Sent {
                platform_message_id: Some("\n".into()),
            },
            "unknown",
        ),
        ("unknown", QqOutreachResult::Unknown, "unknown"),
        ("not-sent", QqOutreachResult::NotSent, "waiting"),
    ] {
        let item = event(id);
        ready(&store, &item);
        store.dispatched(&item).unwrap();
        store.delivered(&item, result).unwrap();
        let (state, decision, due) = store.state(&item).unwrap();
        assert_eq!(state, expected);
        if expected == "waiting" {
            assert!(decision.is_none());
            assert!(due > now());
        }
        assert!(store.dispatched(&item).is_err());
    }
}

#[test]
fn recent_speech_is_scoped_before_limiting_and_imports_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("speech.sqlite3");
    let store = Store::open(&path).unwrap();
    let mut spoken = event("expression-1");
    spoken.facts = json!({"kind":"expression","medium":"image"});
    store
        .record_expression(spoken.clone(), "选中的表情描述", QqOutreachResult::Unknown)
        .unwrap();
    store
        .record_expression(
            spoken.clone(),
            "选中的表情描述",
            QqOutreachResult::Sent {
                platform_message_id: Some("image-1".into()),
            },
        )
        .unwrap();
    store
        .record_expression(spoken.clone(), "选中的表情描述", QqOutreachResult::Unknown)
        .unwrap();
    assert!(
        store
            .record_expression(
                spoken.clone(),
                "不同表达不能覆盖",
                QqOutreachResult::Unknown
            )
            .is_err()
    );
    for n in 0..300 {
        let mut other = event(&format!("other-{n}"));
        other.session.user_id = "qq:other".into();
        other.facts = json!({"kind":"expression"});
        store
            .record_expression(
                other,
                "另一个人的对话",
                QqOutreachResult::Sent {
                    platform_message_id: Some(format!("other-platform-{n}")),
                },
            )
            .unwrap();
    }
    drop(store);
    let store = Store::open(&path).unwrap();
    let records = store
        .recent(&ContextScope {
            session_id: spoken.session.session_id,
            user_id: spoken.session.user_id,
        })
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["event_id"], "expression-1");
    assert_eq!(records[0]["delivery_status"], "sent");
    assert_eq!(records[0]["medium"], "image");
}

#[test]
fn unavailable_old_candidates_do_not_starve_later_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("speech.sqlite3")).unwrap();
    for n in 0..70 {
        store.enqueue(&event(&format!("event-{n}"))).unwrap();
    }
    let first = store.pending().unwrap();
    assert_eq!(first.len(), 64);
    for (n, item) in first.iter().enumerate() {
        if n % 2 == 0 {
            store.retire(item).unwrap();
        } else {
            store.postpone(item, 60_000).unwrap();
        }
    }
    let later = store.pending().unwrap();
    assert_eq!(later.len(), 6);
    assert!(later.iter().all(|item| !first.contains(item)));
}

#[test]
fn corrupt_record_is_preserved_before_any_recovery_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("speech.sqlite3");
    let store = Store::open(&path).unwrap();
    let item = event("in-flight");
    ready(&store, &item);
    store.dispatched(&item).unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute("UPDATE speech SET decision='broken-json'", [])
        .unwrap();
    drop(store);
    assert!(Store::open(&path).is_err());
    let db = Connection::open(&path).unwrap();
    let saved: (String, String) = db
        .query_row("SELECT state,decision FROM speech", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(saved, ("sending".into(), "broken-json".into()));
}

#[test]
fn missing_or_foreign_schema_is_not_reinitialized() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sqlite3");
    let db = Connection::open(&missing).unwrap();
    db.execute_batch("PRAGMA user_version=1;").unwrap();
    assert!(Store::open(&missing).is_err());
    assert!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='speech')",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    let foreign = dir.path().join("foreign.sqlite3");
    let db = Connection::open(&foreign).unwrap();
    db.execute_batch("CREATE TABLE original(value TEXT); INSERT INTO original VALUES ('keep');")
        .unwrap();
    assert!(Store::open(&foreign).is_err());
    assert_eq!(
        db.query_row("SELECT value FROM original", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "keep"
    );
}

#[test]
fn malformed_or_empty_model_choices_cannot_become_messages() {
    for invalid in [
        "收到",
        r#"{"action":"speak","text":"hi","after_ms":0,"extra":true}"#,
        r#"{"action":"silence","text":"auto"}"#,
    ] {
        assert!(serde_json::from_str::<QqSpeechDecision>(invalid).is_err());
    }
    assert!(
        validate_decision(&QqSpeechDecision::Speak {
            text: " ".into(),
            after_ms: 0
        })
        .is_err()
    );
    assert!(validate_decision(&QqSpeechDecision::Wait { after_ms: 0 }).is_err());
    assert!(validate_decision(&QqSpeechDecision::Silence {}).is_ok());
}

#[test]
fn another_host_cannot_recover_an_active_send_and_missing_data_is_not_recreated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("speech.sqlite3");
    let store = Store::open(&path).unwrap();
    let item = event("active");
    ready(&store, &item);
    store.dispatched(&item).unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(store.state(&item).unwrap().0, "sending");
    drop(store);
    std::fs::remove_file(&path).unwrap();
    assert!(Store::open(&path).is_err());
    assert!(!path.exists());
}

#[test]
fn oversized_tool_history_is_a_bounded_fact_view_without_claiming_delivery() {
    use eve_llm_api::{ToolCall, ToolResult};
    let large = "旧工具输出\\\"\n内容".repeat(50_000);
    let call = ToolCall {
        id: "call-1".into(),
        name: "read_file".into(),
        arguments: json!({"path":"source.rs"}),
    };
    let messages = vec![
        ChatMessage::text(ChatRole::User, "检查源码"),
        ChatMessage::assistant_tool_calls(vec![call]).unwrap(),
        ChatMessage::tool_results(vec![
            ToolResult::success("call-1", json!({"body":large})).unwrap(),
        ])
        .unwrap(),
        ChatMessage::text(ChatRole::Assistant, "内部草稿，还没有告诉用户"),
    ];
    assert!(serde_json::to_vec(&messages).unwrap().len() > 256 * 1024);
    let turn = SessionTurn {
        id: 1,
        input: "检查源码".into(),
        status: SessionTurnStatus::Completed { messages },
    };
    let original = serde_json::to_vec(&turn).unwrap();
    let view = history_turn(&turn);
    assert!(serde_json::to_vec(&view).unwrap().len() < 4096);
    assert_eq!(view["user_input"]["text"], "检查源码");
    assert_eq!(view["internal_final"]["text"], "内部草稿，还没有告诉用户");
    assert_eq!(view["tool_tail"][0]["outcome"]["status"], "returned");
    assert!(view["tool_tail"][0]["outcome"].get("sent").is_none());
    assert_eq!(serde_json::to_vec(&turn).unwrap(), original);
}

#[test]
fn history_fragments_keep_utf8_and_failed_pending_turns_as_facts() {
    let text = "字\n\"".repeat(2000);
    let fragment = history_excerpt(&text);
    let excerpt = fragment["text"].as_str().unwrap();
    assert!(excerpt.len() <= HISTORY_EXCERPT_BYTES);
    assert!(text.starts_with(excerpt));
    assert_eq!(fragment["original_bytes"], text.len());
    assert_eq!(fragment["truncated"], true);
    for (status, want) in [
        (SessionTurnStatus::Pending, "pending"),
        (SessionTurnStatus::Interrupted, "interrupted"),
        (
            SessionTurnStatus::Failed {
                failure: eve_session_api::SessionFailure {
                    code: eve_session_api::SessionFailureCode::ProviderTimeout,
                    started_tools: Some(2),
                    evidence: None,
                },
            },
            "failed",
        ),
    ] {
        let view = history_turn(&SessionTurn {
            id: 1,
            input: text.clone(),
            status,
        });
        assert_eq!(view["state"], want);
        assert!(view.get("internal_final").is_none());
    }
}

struct RecentOnly(eve_session_api::SessionKey);
impl SessionService for RecentOnly {
    fn recent_turns(
        &self,
        key: &SessionKey,
        before: Option<u64>,
        limit: usize,
    ) -> eve_session_api::SessionResult<Vec<SessionTurn>> {
        assert_eq!(key, &self.0);
        assert_eq!(before, None);
        assert_eq!(limit, HISTORY_TURNS);
        Ok(vec![
            SessionTurn {
                id: 2,
                input: "新原话".into(),
                status: SessionTurnStatus::Pending,
            },
            SessionTurn {
                id: 1,
                input: "旧原话".into(),
                status: SessionTurnStatus::Interrupted,
            },
        ])
    }
    fn snapshot(
        &self,
        _: &SessionKey,
    ) -> eve_session_api::SessionResult<Option<eve_session_api::SessionSnapshot>> {
        panic!("发言不应克隆全历史")
    }
    fn begin(
        &self,
        _: eve_session_api::SessionInput,
    ) -> eve_session_api::SessionResult<eve_session_api::StartedTurn> {
        unreachable!()
    }
    fn complete(
        &self,
        _: &eve_session_api::TurnLease,
        _: Vec<ChatMessage>,
    ) -> eve_session_api::SessionResult<()> {
        unreachable!()
    }
    fn fail(
        &self,
        _: &eve_session_api::TurnLease,
        _: eve_session_api::SessionFailure,
    ) -> eve_session_api::SessionResult<()> {
        unreachable!()
    }
}

#[test]
fn speech_history_uses_bounded_recent_page_in_chronological_order() {
    let key = event("recent").session;
    let view = recent_history(&RecentOnly(key.clone()), &key).unwrap();
    assert_eq!(view["turns"][0]["turn_id"], 1);
    assert_eq!(view["turns"][1]["turn_id"], 2);
    assert_eq!(view["turns"][1]["state"], "pending");
}
