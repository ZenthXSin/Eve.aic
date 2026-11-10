use super::*;
use eve_memory_api::{
    ExperienceHit, ExperienceReadContext, ExperienceRecall, MAX_RECALL_QUERY_BYTES,
    MemoryRecallFactory, MemoryRecallRequest, MemoryRecallResponse, MemoryRecallService,
    MemoryResult, MemoryScope,
};
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

#[test]
fn speech_recall_uses_bounded_original_and_event_topic_without_routing_or_tool_arguments() {
    let mut input = event("foreground-input:current");
    input.facts = json!({"kind":"foreground_input_pending", "activity":"检查探索笔记", "phase":"tools",
        "tool_arguments":{"credential":"not-a-recall-query"},"sender_id":"not-a-recall-query"});
    let query = recall_input(&input, "继续研究昨天的星图");
    assert!(query.text.contains("当前原话：继续研究昨天的星图"));
    assert!(query.text.contains("检查探索笔记"));
    assert!(!query.text.contains("not-a-recall-query"));
    input.id = "foreground-progress:later".into();
    let query = recall_input(&input, &"星图".repeat(4096));
    assert!(query.text.contains("此前最后原话，仅作语境："));
    assert!(query.text.contains("检查探索笔记"));
    assert!(query.text.len() <= 4096);
}

struct SpeechContextRequest {
    input: TurnInput,
    scope: Option<ContextScope>,
    model_session: Option<eve_llm_api::ModelSession>,
}

#[derive(Default)]
struct SpeechRecallBase {
    requests: Mutex<Vec<SpeechContextRequest>>,
}

fn speech_recall_base_snapshot() -> ContextSnapshot {
    ContextSnapshot {
        profile: "原有交流画像".into(),
        memories: vec!["已有蒸馏经验".into()],
        pinned: vec!["固定身份".into()],
        ..Default::default()
    }
}

impl ContextAssembler for SpeechRecallBase {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_with_model_session(input, None, None)
    }

    fn assemble_with_model_session(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
        model_session: Option<eve_llm_api::ModelSession>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        self.requests.lock().unwrap().push(SpeechContextRequest {
            input,
            scope,
            model_session,
        });
        Box::pin(async { Ok(speech_recall_base_snapshot()) })
    }
}

struct SpeechExperienceRequest {
    scope: MemoryScope,
    principal: Option<String>,
    query: String,
    limit: usize,
}

#[derive(Clone)]
struct SpeechRecallProbe {
    scope: MemoryScope,
    recalls: Arc<Mutex<Vec<MemoryRecallRequest>>>,
    experiences: Arc<Mutex<Vec<SpeechExperienceRequest>>>,
}

impl MemoryRecallFactory for SpeechRecallProbe {
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryRecallService>> {
        assert!(scope == self.scope);
        Ok(Arc::new(self.clone()))
    }
}

impl MemoryRecallService for SpeechRecallProbe {
    fn recall(&self, request: &MemoryRecallRequest) -> MemoryResult<MemoryRecallResponse> {
        request.validate()?;
        self.recalls.lock().unwrap().push(request.clone());
        Ok(MemoryRecallResponse {
            scope: self.scope.clone(),
            revision: 1,
            hits: vec![],
        })
    }
}

impl ExperienceRecall for SpeechRecallProbe {
    fn recall(
        &self,
        context: &ExperienceReadContext,
        query: &str,
        limit: usize,
    ) -> MemoryResult<Vec<ExperienceHit>> {
        self.experiences
            .lock()
            .unwrap()
            .push(SpeechExperienceRequest {
                scope: context.scope.clone(),
                principal: context.principal.clone(),
                query: query.into(),
                limit,
            });
        Ok(vec![])
    }
}

#[tokio::test]
async fn speech_recall_keeps_event_topic_and_original_in_actual_memory_queries() {
    let markers = [
        "类型主题",
        "行动主题",
        "阶段主题",
        "事件原话主题",
        "状态主题",
    ];
    for id in [
        "foreground-input:current",
        "host-reply:input-1",
        "foreground-progress:later",
    ] {
        let mut input_event = event(id);
        for (field, marker) in ["kind", "activity", "phase", "user_text", "status"]
            .into_iter()
            .zip(markers)
        {
            input_event.facts[field] = json!(format!("{marker}{}", "𐐷".repeat(256)));
        }
        input_event.facts["tool_arguments"] = json!({"credential":"forbidden-recall-value"});
        input_event.facts["sender_id"] = json!("forbidden-recall-value");
        input_event.facts["result"] = json!("forbidden-recall-value");
        let original = format!(
            "原话检索关键词 {} 原话远端匹配词 {}",
            "旧".repeat(570),
            "𐐷".repeat(2048)
        );
        let input = recall_input(&input_event, &original);
        let original_label = if id == "foreground-progress:later" {
            "此前最后原话，仅作语境："
        } else {
            "当前原话："
        };
        assert!(input.text.find(original_label).unwrap() <= 320);
        let messages = decision_messages(
            "Eve".into(),
            speech_recall_base_snapshot(),
            json!({}),
            vec![],
            &input_event,
            original.clone(),
            100,
        );
        let dynamic: Value = serde_json::from_str(
            messages[4]
                .text
                .as_deref()
                .unwrap()
                .split_once('：')
                .unwrap()
                .1,
        )
        .unwrap();
        let decision: Value = serde_json::from_str(messages[6].text.as_deref().unwrap()).unwrap();
        assert_eq!(dynamic["latest_user_message"]["text"], original);
        assert_eq!(decision["host_event"], input_event.facts);
        if id == "foreground-progress:later" {
            assert_eq!(decision["trigger"], "background_event");
            assert!(decision.get("current_user_message").is_none());
        } else {
            assert_eq!(decision["trigger"], "user_input");
            assert_eq!(decision["current_user_message"]["text"], original);
        }
        let scope = ContextScope {
            session_id: input_event.session.session_id.clone(),
            user_id: input_event.session.user_id.clone(),
        };
        let model_session = eve_llm_api::ModelSession::scoped("speech", &scope)
            .unwrap()
            .with_turn(id)
            .unwrap();
        let probe = SpeechRecallProbe {
            scope: MemoryScope {
                channel: "qq".into(),
                session_id: scope.session_id.clone(),
                user_id: scope.user_id.clone(),
            },
            recalls: Arc::default(),
            experiences: Arc::default(),
        };
        let base = Arc::new(SpeechRecallBase::default());
        let memory = eve_memory_plugin::MemoryRecallContext::new(
            "qq",
            Arc::new(probe.clone()),
            base.clone(),
        )
        .unwrap();
        let context = crate::subject_memory::ExperienceContext::new(
            "qq",
            Arc::new(probe.clone()),
            Arc::new(memory),
        )
        .unwrap();
        let snapshot = context
            .assemble_with_model_session(
                input.clone(),
                Some(scope.clone()),
                Some(model_session.clone()),
            )
            .await
            .unwrap();
        assert_eq!(snapshot, speech_recall_base_snapshot());
        let base_requests = base.requests.lock().unwrap();
        assert_eq!(base_requests.len(), 1);
        assert_eq!(base_requests[0].input, input);
        assert_eq!(base_requests[0].scope, Some(scope));
        assert_eq!(base_requests[0].model_session, Some(model_session));
        // TrainingContext 等接收完整 TurnInput，仍可匹配原来 2048 字节内的远端原话。
        assert!(base_requests[0].input.text.contains("原话远端匹配词"));
        let recalls = probe.recalls.lock().unwrap();
        let experiences = probe.experiences.lock().unwrap();
        assert_eq!(recalls.len(), 1);
        assert_eq!(experiences.len(), 1);
        assert!(experiences[0].scope == probe.scope);
        assert!(experiences[0].principal.is_none());
        assert_eq!(recalls[0].limit, 3);
        assert_eq!(experiences[0].limit, 3);
        for query in [&recalls[0].query, &experiences[0].query] {
            assert!(query.len() <= MAX_RECALL_QUERY_BYTES);
            assert!(!query.chars().any(char::is_control));
            assert!(query.contains("原话检索关键词"));
            for marker in markers {
                assert!(query.contains(marker), "{id} 的实际记忆查询丢失 {marker}");
            }
            assert!(!query.contains("forbidden-recall-value"));
            if id == "foreground-progress:later" {
                assert!(query.contains("此前最后原话，仅作语境："));
                assert!(!query.contains("当前原话："));
            } else {
                assert!(query.contains("当前原话："));
                assert!(!query.contains("此前最后原话，仅作语境："));
            }
        }
    }
}

#[test]
fn speech_recall_omits_empty_and_non_string_event_topics() {
    let mut input = event("foreground-progress:later");
    input.facts = json!({
        "kind":"", "activity":" \n\t ", "phase":null, "user_text":42,
        "status":"仍在核实", "details":"forbidden-recall-value"
    });
    let query = recall_input(&input, "短原话完整保留");
    assert!(
        query
            .text
            .contains("此前最后原话，仅作语境：短原话完整保留")
    );
    assert!(query.text.contains("status: 仍在核实"));
    for omitted in [
        "kind:",
        "activity:",
        "phase:",
        "user_text:",
        "forbidden-recall-value",
    ] {
        assert!(
            !query.text.contains(omitted),
            "空值或非白名单字段进入查询：{omitted}"
        );
    }
    input.facts["status"] = json!("\t\n");
    let query = recall_input(&input, "没有主题时原话完整保留");
    assert_eq!(query.text, "此前最后原话，仅作语境：没有主题时原话完整保留");
}

#[test]
fn new_facts_preserve_rules_pinned_and_history_before_dynamic_speech_context() {
    let context = ContextSnapshot {
        pinned: vec!["固定身份与宿主工具事实".into()],
        profile: "原有交流画像".into(),
        memories: vec!["原有记忆".into()],
        ..Default::default()
    };
    let history = json!({"turns":[{"internal_final":"旧轮次收尾不是本次输出"}]});
    let deliveries = vec![json!({"delivery_status":"unknown","event_id":"earlier"})];
    let first_event = event("host-reply:input-1");
    let first = decision_messages(
        "同一个 Eve".into(),
        context.clone(),
        history.clone(),
        deliveries.clone(),
        &first_event,
        "用户当前完整原话\n第二行".into(),
        100,
    );
    let next_context = ContextSnapshot {
        profile: "更新后的交流画像".into(),
        memories: vec!["新的记忆".into()],
        ..context
    };
    let next = decision_messages(
        "同一个 Eve".into(),
        next_context,
        history.clone(),
        vec![],
        &event("next"),
        "另一句原话".into(),
        200,
    );
    assert_eq!(&first[..4], &next[..4]);
    assert_eq!(first[1].text.as_deref(), Some(RULES));
    assert_ne!(first[4], next[4]);
    let data = |message: &ChatMessage| -> Value {
        serde_json::from_str(message.text.as_deref().unwrap().split_once('：').unwrap().1).unwrap()
    };
    assert_eq!(
        data(&first[2]),
        json!({"pinned":["固定身份与宿主工具事实"]})
    );
    assert_eq!(data(&first[3]), history);
    assert_eq!(
        data(&first[4]),
        json!({
            "profile":"原有交流画像", "memories":["原有记忆"], "recent_deliveries":deliveries,
            "latest_user_message":{"id":first_event.anchor,"text":"用户当前完整原话\n第二行"}
        })
    );
    assert_eq!(first[5].text.as_deref(), Some(FORMAT_GUARD));
    let input: Value = serde_json::from_str(first[6].text.as_deref().unwrap()).unwrap();
    assert_eq!(input["host_event"], first_event.facts);
    assert_eq!(input["now_ms"], 100);
    assert_eq!(
        input["current_user_message"],
        json!({
            "id":first_event.anchor, "text":"用户当前完整原话\n第二行"
        })
    );
    assert_eq!(input["trigger"], "user_input");
    let next_input: Value = serde_json::from_str(next[6].text.as_deref().unwrap()).unwrap();
    assert_eq!(next_input["trigger"], "background_event");
    assert!(next_input.get("current_user_message").is_none());
    assert!(next_input.get("latest_user_message").is_none());
    assert_eq!(data(&next[4])["latest_user_message"]["text"], "另一句原话");
}

#[test]
fn delayed_background_and_rebound_command_never_reopen_the_last_user_message() {
    for id in [
        "outreach:health-42",
        "outreach:wake-1",
        "host-reply:older",
        "unknown-source",
    ] {
        let messages = decision_messages(
            "Eve".into(),
            ContextSnapshot::default(),
            json!({}),
            vec![],
            &event(id),
            "晚安喵，你慢慢来".into(),
            1000,
        );
        let input: Value =
            serde_json::from_str(messages.last().unwrap().text.as_deref().unwrap()).unwrap();
        assert_eq!(input["trigger"], "background_event");
        assert!(input.get("current_user_message").is_none());
        assert!(input.get("latest_user_message").is_none());
        assert!(input.get("current_message_id").is_none());
        assert_eq!(input["event_id"], id);
        assert!(
            !messages
                .last()
                .unwrap()
                .text
                .as_ref()
                .unwrap()
                .contains("晚安喵，你慢慢来")
        );
        let context: Value = serde_json::from_str(
            messages[4]
                .text
                .as_deref()
                .unwrap()
                .split_once('：')
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(context["latest_user_message"]["text"], "晚安喵，你慢慢来");
    }
    let messages = decision_messages(
        "Eve".into(),
        ContextSnapshot::default(),
        json!({}),
        vec![],
        &event("foreground-input:live"),
        "晚安喵，你慢慢来".into(),
        1000,
    );
    let input: Value =
        serde_json::from_str(messages.last().unwrap().text.as_deref().unwrap()).unwrap();
    assert_eq!(input["trigger"], "user_input");
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

#[derive(Clone, Copy)]
enum ModelAnswer {
    Pending,
    Failure,
    Silence,
}

struct DecisionProvider {
    answer: Mutex<ModelAnswer>,
    calls: std::sync::atomic::AtomicUsize,
}
impl eve_llm_api::LlmProvider for DecisionProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(request.tools.is_empty());
            let input: Value =
                serde_json::from_str(request.messages.last().unwrap().text.as_deref().unwrap())
                    .unwrap();
            assert_eq!(input["trigger"], "background_event");
            assert!(input.get("current_user_message").is_none());
            let answer = *self.answer.lock().unwrap();
            match answer {
                ModelAnswer::Pending => std::future::pending().await,
                ModelAnswer::Failure => Err(LlmError::Provider("本地模型故障夹具".into())),
                ModelAnswer::Silence => Ok(ModelResponse::Final {
                    text: r#"{"action":"silence"}"#.into(),
                }),
            }
        })
    }
}
impl DecisionProvider {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}
struct DecisionResolver(Arc<DecisionProvider>);
impl LlmModelResolver for DecisionResolver {
    fn resolve(&self) -> Result<eve_llm_api::ModelSelection, LlmError> {
        Ok(eve_llm_api::ModelSelection {
            provider: self.0.clone(),
            provider_timeout: std::time::Duration::from_secs(59),
        })
    }
}

fn decision_fixture(
    directory: &Path,
    conversations: eve_qqbot_plugin::QqConversations,
    event: &QqSpeechEvent,
    answer: ModelAnswer,
) -> (Speech, Arc<DecisionProvider>) {
    let provider = Arc::new(DecisionProvider {
        answer: Mutex::new(answer),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let resolver = Arc::new(DecisionResolver(provider.clone()));
    (
        Speech {
            store: Store::open(&directory.join("decisions.sqlite3")).unwrap(),
            resolver: resolver.clone(),
            background_resolver: resolver,
            context: crate::services::default_context(),
            sessions: Arc::new(RecentOnly(event.session.clone())),
            host: eve_runtime::LlmHostConfig::default(),
            conversations,
        },
        provider,
    )
}

struct NoConversationExecution;
impl eve_control_api::ControlRunner for NoConversationExecution {
    fn run<'a>(
        &'a self,
        _: eve_session_api::SessionInput,
        _: &'a dyn eve_llm_api::TurnEventSink,
    ) -> eve_control_api::RunFuture<'a> {
        panic!("原话夹具只经过通道持久化，不执行主模型或工具")
    }
}

/// 通过真实入站路径建立原文绑定，不为测试开放修改 QqConversations 私有状态的接口。
async fn bound_conversations(
    directory: &Path,
) -> (eve_qqbot_plugin::QqConversations, QqSpeechEvent) {
    use eve_plugin_api::{PluginId, ServiceId};
    use eve_qqbot_plugin::{QqBotConfig, QqBotPlugin, QqBotStatusHandle};
    let message = eve_qqbot_plugin::Message {
        id: "input-1".into(),
        scope: "c2c".into(),
        target_id: "owner".into(),
        user_id: "owner".into(),
        text: "/help".into(),
        attachments: vec![],
        quoted: None,
    };
    let mut frame = serde_json::to_value(&message).unwrap();
    frame["type"] = json!("restriction_only");
    frame["version"] = json!(1);
    frame["original_text"] = json!(message.text);
    let script = directory.join("original-ingress.mjs");
    std::fs::write(
        &script,
        [
            "import readline from 'node:readline';\nconst frames = ",
            &json!([{"type":"ready","version":1}, frame]).to_string(),
            ";\nfor (const frame of frames) process.stdout.write(JSON.stringify(frame) + '\\n');\n\
             const input = readline.createInterface({input:process.stdin});\n\
             input.on('line', line => {\n\
               const command = JSON.parse(line);\n\
               if (command.type === 'finish' || command.type === 'stop')\n\
                 process.stdout.end(() => process.exit(0));\n\
             });\n",
        ]
        .concat(),
    )
    .unwrap();
    let services = eve_kernel::KernelServices::default();
    let registry = services.registry.clone();
    let kernel = eve_kernel::Kernel::with_services(services);
    kernel
        .register(Box::new(
            eve_control_plugin::ControlPlugin::new(Arc::new(NoConversationExecution), vec![])
                .unwrap(),
        ))
        .unwrap();
    kernel
        .register(Box::new(
            eve_config_plugin::ConfigPlugin::new(eve_config_plugin::ConfigBootstrap::new(
                directory,
                vec![eve_message_api::message_schema()],
            ))
            .unwrap(),
        ))
        .unwrap();
    kernel
        .register(Box::new(
            eve_message_plugin::RelationPlugin::rules().unwrap(),
        ))
        .unwrap();
    kernel
        .register(Box::new(
            eve_message_plugin::MessageRouterPlugin::builtin().unwrap(),
        ))
        .unwrap();
    let plugin = QqBotPlugin::new(QqBotConfig {
        node_program: "node".into(),
        bridge_script: script,
        bridge_args: vec![],
        app_id: "speech-test".into(),
        app_secret: "local-fixture".into(),
        sandbox: true,
        markdown: false,
    })
    .unwrap()
    .with_receipt_archive(Arc::new(
        eve_qqbot_receipt_sqlite::SqliteReceiptArchive::open(&directory.join("originals.sqlite3"))
            .unwrap(),
    ));
    let conversations = plugin.conversations();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(eve_qqbot_plugin::QQBOT_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let status = registry
        .get(&ServiceId::new(eve_qqbot_plugin::QQBOT_STATUS_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<QqBotStatusHandle>()
        .unwrap();
    let mut changes = status.status.clone();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !changes.borrow_and_update().closed {
            changes.changed().await.unwrap();
        }
    })
    .await;
    kernel.stop_all().await.unwrap();
    closed.expect("原话夹具没有完成持久化并关闭通道");
    assert!(!status.status.borrow().terminal_error);
    assert_eq!(status.status.borrow().received, 1);
    let event = QqSpeechEvent {
        session: message.session_key("speech-test").unwrap(),
        anchor: message.id,
        ..event("after-channel-exit")
    };
    assert_eq!(
        conversations
            .original_message_at(&event.session, &event.anchor)
            .unwrap(),
        "/help"
    );
    assert_eq!(
        conversations
            .latest_message_id(&event.session)
            .unwrap()
            .as_deref(),
        Some(event.anchor.as_str())
    );
    (conversations, event)
}

fn make_retry_due(store: &Store, event: &QqSpeechEvent) {
    // 只推进时钟条件；不改变状态或重开账本，证明清理本身恢复了可调度性。
    assert_eq!(
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE speech SET due=0 WHERE id=?1 AND state='waiting'",
                [key(event).unwrap()],
            )
            .unwrap(),
        1
    );
    assert_eq!(store.pending().unwrap(), std::slice::from_ref(event));
}

#[tokio::test]
async fn original_preflight_error_postpones_without_calling_model_or_spinning() {
    let directory = tempfile::tempdir().unwrap();
    let event = event("missing-original");
    let (speech, provider) = decision_fixture(
        directory.path(),
        eve_qqbot_plugin::QqConversations::default(),
        &event,
        ModelAnswer::Silence,
    );
    let started = now();
    assert!(speech.consider(event.clone()).await.is_err());
    let (state, decision, due) = speech.store.state(&event).unwrap();
    assert_eq!(state, "pending");
    assert!(decision.is_none());
    assert!(due >= started + RETRY_MS);
    assert!(speech.store.pending().unwrap().is_empty());
    assert!(speech.consider(event).await.unwrap().is_none());
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn provider_failure_returns_to_waiting_and_same_store_can_retry() {
    let directory = tempfile::tempdir().unwrap();
    let (conversations, event) = bound_conversations(directory.path()).await;
    let (speech, provider) = decision_fixture(
        directory.path(),
        conversations,
        &event,
        ModelAnswer::Failure,
    );
    let started = now();
    assert!(matches!(
        speech.consider(event.clone()).await,
        Err(LlmError::Provider(_))
    ));
    let (state, decision, due) = speech.store.state(&event).unwrap();
    assert_eq!(state, "waiting");
    assert!(decision.is_none());
    assert!(due >= started + RETRY_MS);
    assert!(speech.consider(event.clone()).await.unwrap().is_none());
    assert_eq!(provider.calls(), 1);
    *provider.answer.lock().unwrap() = ModelAnswer::Silence;
    make_retry_due(&speech.store, &event);
    assert!(matches!(
        speech.consider(event.clone()).await.unwrap(),
        Some(QqSpeechDecision::Silence {})
    ));
    assert_eq!(speech.store.state(&event).unwrap().0, "silence");
    assert_eq!(provider.calls(), 2);
}

#[tokio::test]
async fn cancelled_consideration_retries_without_reopening_the_live_store() {
    let directory = tempfile::tempdir().unwrap();
    let (conversations, event) = bound_conversations(directory.path()).await;
    let (speech, provider) = decision_fixture(
        directory.path(),
        conversations,
        &event,
        ModelAnswer::Pending,
    );
    let mut future = speech.consider(event.clone());
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(future.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(speech.store.state(&event).unwrap().0, "evaluating");
    assert_eq!(provider.calls(), 1);
    let cancelled = now();
    drop(future);
    let (state, decision, due) = speech.store.state(&event).unwrap();
    assert_eq!(state, "waiting", "丢弃 future 后不能留在 evaluating");
    assert!(decision.is_none());
    assert!(due >= cancelled + RETRY_MS);
    assert!(speech.consider(event.clone()).await.unwrap().is_none());
    assert_eq!(provider.calls(), 1);
    *provider.answer.lock().unwrap() = ModelAnswer::Silence;
    make_retry_due(&speech.store, &event);
    assert!(matches!(
        speech.consider(event.clone()).await.unwrap(),
        Some(QqSpeechDecision::Silence {})
    ));
    assert_eq!(provider.calls(), 2);
    assert_eq!(speech.store.state(&event).unwrap().0, "silence");
}

#[tokio::test]
async fn cancellation_cleanup_preserves_retired_dispatched_and_replaced_events() {
    let directory = tempfile::tempdir().unwrap();
    let (conversations, original) = bound_conversations(directory.path()).await;
    let (speech, _) = decision_fixture(
        directory.path(),
        conversations,
        &original,
        ModelAnswer::Pending,
    );
    for state in ["stale", "sending", "sent", "replacement"] {
        let mut event = original.clone();
        event.id = format!("preserve-{state}");
        let mut future = speech.consider(event.clone());
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(future.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(speech.store.state(&event).unwrap().0, "evaluating");
        match state {
            "stale" => speech.store.retire(&event).unwrap(),
            "replacement" => {
                event.anchor = "replacement-input".into();
                speech.store.enqueue(&event).unwrap();
            }
            _ => {
                speech
                    .store
                    .save(&event, "ready", Some(&speak()), 0)
                    .unwrap();
                speech.store.dispatched(&event).unwrap();
                if state == "sent" {
                    speech
                        .store
                        .delivered(
                            &event,
                            QqOutreachResult::Sent {
                                platform_message_id: Some("confirmed-local-receipt".into()),
                            },
                        )
                        .unwrap();
                }
            }
        }
        let saved = speech.store.state(&event).unwrap();
        drop(future);
        assert_eq!(speech.store.state(&event).unwrap(), saved);
    }
}
