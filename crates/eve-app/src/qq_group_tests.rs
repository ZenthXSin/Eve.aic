use super::*;
use eve_config_api::*;
use eve_control_api::*;
use eve_group_observation_sqlite::SqliteGroupObservations;
use eve_llm_api::{LlmModelResolver, ModelSelection, TurnEvent, TurnEventKind};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::atomic::{AtomicUsize, Ordering},
};

#[test]
fn authored_pagination_crosses_filtered_sources_and_claims_the_exact_later_group() {
    struct Candidates {
        rows: Vec<QqOutreachPush>,
        claims: Mutex<Vec<String>>,
    }
    impl QqOutreach for Candidates {
        fn authored_pushes(
            &self,
            after: Option<&str>,
            limit: usize,
        ) -> PluginResult<eve_qqbot_plugin::QqAuthoredPushPage> {
            let mut rows: Vec<_> = self
                .rows
                .iter()
                .filter(|p| after.is_none_or(|a| p.message.id.as_str() > a))
                .take(limit + 1)
                .cloned()
                .collect();
            let more = rows.len() > limit;
            rows.truncate(limit);
            let next = more.then(|| rows.last().unwrap().message.id.clone());
            Ok(eve_qqbot_plugin::QqAuthoredPushPage { pushes: rows, next })
        }
        fn group_chime(&self, id: &str) -> PluginResult<Option<QqGroupChimeDelivery>> {
            Ok(self
                .rows
                .iter()
                .find(|p| p.message.id == id)
                .map(|p| QqGroupChimeDelivery {
                    session: p.session.clone().unwrap(),
                    speaker_id: "member".into(),
                    anchor: Some("real-platform-anchor".into()),
                    parts: vec![(p.message.text.clone(), 0)],
                }))
        }
        fn claim_authored_push(&self, expected: &QqOutreachPush) -> PluginResult<bool> {
            if !self.rows.contains(expected) {
                return Ok(false);
            }
            self.claims
                .lock()
                .unwrap()
                .push(expected.message.id.clone());
            Ok(true)
        }
        fn due(&self, _: &SessionKey) -> PluginResult<bool> {
            Ok(false)
        }
        fn judge(&self, _: QqOutreachMoment) -> QqOutreachFuture {
            Box::pin(async { Ok(false) })
        }
        fn attach(&self, _: &SessionKey, _: &str) -> PluginResult<Option<QqOutreachMessage>> {
            panic!("不附带旧模板")
        }
        fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
            panic!("不读取旧模板")
        }
        fn delivered(&self, _: &str, _: QqOutreachResult) -> PluginResult<()> {
            panic!("只读筛选没有回执")
        }
    }
    let rows = (0..70)
        .map(|i| QqOutreachPush {
            session: Some(
                SessionKey::new(
                    format!("group-{i}"),
                    if i < 69 { "wrong-scope" } else { USER },
                )
                .unwrap(),
            ),
            user_id: if i < 69 { "wrong-scope" } else { USER }.into(),
            message: QqOutreachMessage {
                id: format!("chime-{i:03}"),
                text: "模型选定的一句".into(),
            },
        })
        .collect();
    let source = Arc::new(Candidates {
        rows,
        claims: Mutex::new(vec![]),
    });
    let combined = Arc::new(crate::qq_clarify::Combined(vec![(
        "chime-",
        source.clone(),
    )]));
    let guarded = crate::qq_access::GuardedOutreach {
        inner: combined,
        admin: crate::qq_admin::Admin::stub(false),
    };
    let first = guarded.authored_pushes(None, 64).unwrap();
    assert!(first.pushes.is_empty());
    assert_eq!(first.next.as_deref(), Some("chime-063"));
    assert!(source.claims.lock().unwrap().is_empty());
    let next = guarded.authored_pushes(first.next.as_deref(), 64).unwrap();
    assert!(next.next.is_none());
    assert_eq!(next.pushes.len(), 1);
    let expected = &next.pushes[0];
    assert_eq!(expected.message.id, "chime-069");
    let mut changed = expected.clone();
    changed.message.text = "被宿主篡改".into();
    assert!(!guarded.claim_authored_push(&changed).unwrap());
    assert!(guarded.claim_authored_push(expected).unwrap());
    assert_eq!(*source.claims.lock().unwrap(), ["chime-069"]);
}

struct TestSettings(Mutex<String>);
impl ConfigService for TestSettings {
    fn snapshot(&self, namespace: &str, schema_version: u32) -> ConfigResult<ConfigSnapshot> {
        let values = chime::schema()
            .fields
            .into_iter()
            .filter_map(|(name, field)| field.default.map(|value| (name, value)))
            .collect::<BTreeMap<_, _>>();
        let mut snapshot = ConfigSnapshot {
            namespace: namespace.into(),
            schema_version,
            revision: 1,
            values,
        };
        snapshot
            .values
            .insert("judge".into(), serde_json::json!(*self.0.lock().unwrap()));
        snapshot
            .values
            .insert("cooldown_ms".into(), serde_json::json!(0));
        Ok(snapshot)
    }
    fn begin_request(&self, _: &str, _: u32) -> ConfigResult<ConfigRequest> {
        Err(ConfigError::Unavailable)
    }
    fn read_request(&self, _: &ConfigRequest) -> ConfigResult<ConfigSnapshot> {
        Err(ConfigError::Unavailable)
    }
}
#[derive(Default)]
struct NoTraining(AtomicUsize);
impl TrainingService for NoTraining {
    fn enabled(&self, _: &ContextScope) -> PluginResult<bool> {
        Ok(false)
    }
    fn set_enabled(&self, _: &ContextScope, _: bool) -> PluginResult<()> {
        Ok(())
    }
    fn observe_conversation(
        &self,
        _: &ContextScope,
        _: eve_training_api::ConversationMessage,
    ) -> PluginResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
struct NoJudge;
impl LlmModelResolver for NoJudge {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        panic!("群交流不能先调用独立 yes/no 模型")
    }
}
struct Submitted {
    input: ControlInput,
    sink: Arc<dyn ControlEventSink>,
    text: String,
}
struct TestControl {
    submitted: Mutex<Vec<Submitted>>,
    decisions: Mutex<VecDeque<String>>,
    requested: Arc<AtomicUsize>,
}
impl TestControl {
    fn new(decisions: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            submitted: Mutex::new(vec![]),
            decisions: Mutex::new(decisions.iter().map(|s| s.to_string()).collect()),
            requested: Arc::new(AtomicUsize::new(0)),
        })
    }
}
impl ControlService for TestControl {
    fn submit(
        &self,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        input.validate()?;
        let mut submitted = self.submitted.lock().unwrap();
        let key = GenerationKey {
            session: input.session.key.clone(),
            task_id: input.task_id.clone(),
            controller_epoch: [1; 16],
            generation: submitted.len() as u64 + 1,
        };
        let text = self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| r#"{"action":"silence"}"#.into());
        submitted.push(Submitted { input, sink, text });
        Ok(key)
    }
    fn submit_if_current(
        &self,
        _: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.submit(input, sink)
    }
    fn cancel(&self, _: &GenerationKey) -> ControlResult<CancelDisposition> {
        Ok(CancelDisposition::AlreadyFinished)
    }
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        let key = key.clone();
        let (sink, text) = {
            let submitted = self.submitted.lock().unwrap();
            let entry = &submitted[key.generation as usize - 1];
            (entry.sink.clone(), entry.text.clone())
        };
        let requested = self.requested.clone();
        Box::pin(async move {
            let result = sink
                .emit(ControlEvent {
                    key: key.clone(),
                    event: TurnEvent {
                        turn_id: Some(key.generation),
                        kind: TurnEventKind::ProviderStarted { request: 1 },
                    },
                })
                .await;
            let (commit, text, failure) = match result {
                Ok(()) => {
                    requested.fetch_add(1, Ordering::SeqCst);
                    (CommitState::Completed, Some(text), None)
                }
                Err(error) => (
                    CommitState::Failed,
                    None,
                    Some(RunFailure::Execution(error)),
                ),
            };
            Ok(ControlReport {
                key,
                cancel_requested: false,
                run: RunReport {
                    turn_id: Some(1),
                    commit,
                    text,
                    transcript: None,
                    started_tools: Some(0),
                    tool_results: vec![],
                    failure,
                },
            })
        })
    }
    fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        Ok(None)
    }
    fn accepts(&self, _: &ControlEvent) -> bool {
        true
    }
}
fn platform(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        scope: "group".into(),
        target_id: "group-42".into(),
        user_id: "member-7".into(),
        text: text.into(),
        attachments: vec![],
        quoted: None,
    }
}
fn make_group(
    path: &std::path::Path,
    settings: Arc<TestSettings>,
    training: Arc<NoTraining>,
) -> Arc<Group> {
    let group = Group::new(
        "app-1".into(),
        Arc::new(SqliteGroupObservations::open(path).unwrap()),
        training,
    );
    assert!(group.policy.settings.set(settings).is_ok());
    group
}
async fn state(path: &std::path::Path, expected: &str) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let value: Option<String> = rusqlite::Connection::open(path)
                .unwrap()
                .query_row(
                    "SELECT state FROM speech_reviews ORDER BY rowid DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            if value.as_deref() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("没有进入 {expected}"));
}

#[tokio::test]
async fn background_wait_survives_restart_then_only_exact_claim_can_send_single_authored_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.sqlite3");
    let settings = Arc::new(TestSettings(Mutex::new("model".into())));
    let training = Arc::new(NoTraining::default());
    let group = make_group(&path, settings.clone(), training.clone());
    let input = platform("real-platform-input", "等我讲完这件事");
    group.observe(&input, None, false, now_ms()).unwrap();
    let control = TestControl::new(&[r#"{"action":"wait","after_ms":1000}"#]);
    let background = Background::start(
        group.clone(),
        control.clone(),
        Arc::new(NoJudge),
        None,
        Default::default(),
    )
    .unwrap();
    assert!(group.next_authored_push().unwrap().is_none());
    assert_eq!(control.requested.load(Ordering::SeqCst), 0);
    background.activate();
    state(&path, "waiting").await;
    assert!(group.next_authored_push().unwrap().is_none());
    assert!(group.next_push().unwrap().is_none());
    let first_id = control.submitted.lock().unwrap()[0].input.task_id.clone();
    background.stop().await.unwrap();
    drop(group);
    let group = make_group(&path, settings, training.clone());
    let control = TestControl::new(&[
        r#"{"action":"speak","text":"我看完了。第二句仍是同一条。","after_ms":0}"#,
    ]);
    let background = Background::start(
        group.clone(),
        control.clone(),
        Arc::new(NoJudge),
        None,
        Default::default(),
    )
    .unwrap();
    background.activate();
    state(&path, "ready").await;
    let push = group.next_authored_push().unwrap().unwrap();
    assert_eq!(push.message.text, "我看完了。第二句仍是同一条。");
    let meta = group.group_chime(&push.message.id).unwrap().unwrap();
    assert_eq!(meta.anchor.as_deref(), Some("real-platform-input"));
    assert_eq!(meta.parts, [(push.message.text.clone(), 0)]);
    assert_eq!(meta.speaker_id, "member-7");
    assert_ne!(control.submitted.lock().unwrap()[0].input.task_id, first_id);
    assert_eq!(group.next_authored_push().unwrap(), Some(push.clone()));
    let mut modified = push.clone();
    modified.message.text = "宿主不应代写".into();
    assert!(!group.claim_authored_push(&modified).unwrap());
    assert!(group.claim_authored_push(&push).unwrap());
    assert!(!group.claim_authored_push(&push).unwrap());
    group
        .delivered(
            &push.message.id,
            QqOutreachResult::Sent {
                platform_message_id: Some("actual-platform-receipt".into()),
            },
        )
        .unwrap();
    let conversation = input.conversation_key("app-1").unwrap();
    assert_eq!(group.store.exchanges(&conversation).unwrap().len(), 1);
    assert_eq!(training.0.load(Ordering::SeqCst), 1, "只学真实发出去的正文");
    assert!(group.next_authored_push().unwrap().is_none());
    background.stop().await.unwrap();
}

#[tokio::test]
async fn quiet_keeps_facts_and_mention_ordinary_and_command_cannot_create_extra_speech() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.sqlite3");
    let settings = Arc::new(TestSettings(Mutex::new("quiet".into())));
    let training = Arc::new(NoTraining::default());
    let group = make_group(&path, settings.clone(), training.clone());
    let input = platform("plain", "这里有一件事");
    group.observe(&input, None, false, now_ms()).unwrap();
    assert_eq!(
        group.store.pending_speech(now_ms(), 10).unwrap().len(),
        1,
        "quiet 不消费事实"
    );
    let ordinary = platform("mentioned", "Eve 我直接叫你");
    group.observe(&ordinary, None, true, now_ms()).unwrap();
    assert!(
        group.store.pending_speech(now_ms(), 10).unwrap().is_empty(),
        "普通对话没有第二条插话，旧事实被替代"
    );
    let regular = platform("regular", "这个讨论可以再看看");
    group.observe(&regular, None, false, now_ms()).unwrap();
    let control = TestControl::new(&[r#"{"action":"silence"}"#]);
    let background = Background::start(
        group.clone(),
        control.clone(),
        Arc::new(NoJudge),
        None,
        Default::default(),
    )
    .unwrap();
    background.activate();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(control.requested.load(Ordering::SeqCst), 0);
    assert!(group.next_authored_push().unwrap().is_none());
    *settings.0.lock().unwrap() = "model".into();
    state(&path, "silence").await;
    assert_eq!(control.requested.load(Ordering::SeqCst), 1);
    assert!(group.next_authored_push().unwrap().is_none());
    let command = platform("command", "/bind sensitive-value-not-to-store");
    group.observe(&command, None, false, now_ms()).unwrap();
    let conversation = input.conversation_key("app-1").unwrap();
    assert!(
        group
            .store
            .recent(&conversation, 100)
            .unwrap()
            .iter()
            .all(|m| !m.text.contains("sensitive-value"))
    );
    assert_eq!(training.0.load(Ordering::SeqCst), 0);
    background.stop().await.unwrap();
}

#[tokio::test]
async fn provider_budget_is_reserved_before_actual_request_and_error_does_not_author_a_reply() {
    use eve_budget_api::{AutonomyBudget, BudgetPolicy};
    let kernel = eve_kernel::Kernel::with_services(Default::default());
    let plugin = eve_budget_plugin::BudgetPlugin::new(BudgetPolicy {
        window_ms: 60_000,
        limit: 1,
    })
    .unwrap();
    let budget = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.sqlite3");
    let settings = Arc::new(TestSettings(Mutex::new("model".into())));
    let group = make_group(&path, settings, Arc::new(NoTraining::default()));
    let control = TestControl::new(&[
        r#"{"action":"silence"}"#,
        r#"{"action":"speak","text":"预算不足不能生成这句","after_ms":0}"#,
    ]);
    let engine = Arc::new(
        ChimeEngine::autonomous(control.clone(), group.policy.clone(), "Eve", USER).with_events(
            Arc::new(GroupBudgetEvents(crate::qq_budget::Gate::new(Some(
                budget.clone(),
            )))),
        ),
    );
    for (id, text) in [("first", "第一轮"), ("second", "第二轮")] {
        group
            .observe(&platform(id, text), None, false, now_ms())
            .unwrap();
        let message = group.store.pending_speech(now_ms(), 1).unwrap().remove(0);
        let review = group
            .store
            .begin_speech(&message, now_ms())
            .unwrap()
            .unwrap();
        review_speech(group.clone(), engine.clone(), None, review).await;
    }
    assert_eq!(
        control.requested.load(Ordering::SeqCst),
        1,
        "第二次 ProviderStarted 在请求前被预算拒绝"
    );
    assert_eq!(
        budget
            .snapshot(now_ms())
            .unwrap()
            .current
            .by_category
            .get("chime"),
        Some(&1)
    );
    assert!(group.next_authored_push().unwrap().is_none());
    let attempts: u64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM outbound", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 0);
    state(&path, "waiting").await;
}

#[tokio::test]
async fn missing_platform_receipt_is_unknown_and_never_learned_as_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.sqlite3");
    let settings = Arc::new(TestSettings(Mutex::new("model".into())));
    let training = Arc::new(NoTraining::default());
    let group = make_group(&path, settings, training.clone());
    let input = platform("unknown", "事实");
    group.observe(&input, None, false, now_ms()).unwrap();
    let message = group.store.pending_speech(now_ms(), 1).unwrap().remove(0);
    let review = group
        .store
        .begin_speech(&message, now_ms())
        .unwrap()
        .unwrap();
    group
        .store
        .decide_speech(
            &review,
            Some(&ChimeSpeechDecision::Speak {
                text: "待确认正文".into(),
                after_ms: 0,
            }),
            now_ms(),
        )
        .unwrap();
    let push = group.next_authored_push().unwrap().unwrap();
    assert!(group.claim_authored_push(&push).unwrap());
    group
        .delivered(
            &push.message.id,
            QqOutreachResult::Sent {
                platform_message_id: None,
            },
        )
        .unwrap();
    state(&path, "unknown").await;
    assert!(group.next_authored_push().unwrap().is_none());
    assert!(
        group
            .store
            .exchanges(&input.conversation_key("app-1").unwrap())
            .unwrap()
            .is_empty()
    );
    assert_eq!(training.0.load(Ordering::SeqCst), 0);
}
