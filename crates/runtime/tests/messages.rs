#[path = "support/session.rs"]
mod fixture;
use eve_config_api::*;
use eve_config_plugin::{ConfigBootstrap, ConfigController, ConfigPlugin};
use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_llm_api::*;
use eve_message_api::*;
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin, RulesJudge};
use eve_plugin_api::*;
use eve_runtime::{LlmHostConfig, LlmRelationJudge, SessionControlRunner};
use eve_session_api::*;
use fixture::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio::sync::Notify;

fn discard() -> Arc<dyn ControlEventSink> {
    Arc::new(DiscardControlEvents)
}
fn message(key: &GenerationKey, id: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        target: key.clone(),
        message_id: id.into(),
        text: text.into(),
        reply_to: None,
    }
}
fn request(session: &str, text: &str) -> ControlInput {
    ControlInput {
        session: input(session, text),
        task_id: "task".into(),
    }
}
struct Setup {
    control: Arc<dyn ControlService>,
    messages: Arc<dyn MessageService>,
    admin: ConfigController,
    _directory: tempfile::TempDir,
}
async fn install(rig: &Rig, judge: Arc<dyn RelationJudge>) -> Setup {
    install_using(rig, judge, None).await
}
async fn install_using(
    rig: &Rig,
    judge: Arc<dyn RelationJudge>,
    control: Option<Arc<dyn ControlService>>,
) -> Setup {
    install_plugin(rig, RelationPlugin::new(judge).unwrap(), control).await
}
async fn install_plugin(
    rig: &Rig,
    relation: RelationPlugin,
    control: Option<Arc<dyn ControlService>>,
) -> Setup {
    let directory = tempfile::tempdir().unwrap();
    let config = ConfigPlugin::new(
        ConfigBootstrap::new(directory.path(), vec![message_schema()])
            .with_environment(BTreeMap::new()),
    )
    .unwrap();
    let admin = config.controller();
    rig.kernel.register(Box::new(config)).unwrap();
    let control_plugin: Box<dyn Plugin> = match control {
        Some(service) => Box::new(InjectedControl {
            manifest: PluginManifest::new(CONTROL_PLUGIN_ID, "0.1.0").unwrap(),
            service,
        }),
        None => Box::new(
            ControlPlugin::new(
                Arc::new(SessionControlRunner::new(rig.host.clone())),
                [OWNER, SESSION_PLUGIN_ID]
                    .into_iter()
                    .map(|o| PluginDependency {
                        id: id(o),
                        requirement: Some("^0.1".into()),
                    })
                    .collect(),
            )
            .unwrap(),
        ),
    };
    rig.kernel.register(control_plugin).unwrap();
    rig.kernel.register(Box::new(relation)).unwrap();
    rig.kernel
        .register(Box::new(MessageRouterPlugin::builtin().unwrap()))
        .unwrap();
    rig.kernel.start(&id(ROUTER_PLUGIN_ID)).await.unwrap();
    let control = rig
        .registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone();
    let messages = rig
        .registry
        .get(&ServiceId::new(ROUTER_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<MessageServiceHandle>()
        .unwrap()
        .0
        .clone();
    Setup {
        control,
        messages,
        admin,
        _directory: directory,
    }
}
async fn route(s: &Setup, m: IncomingMessage) -> RouteReport {
    let ticket = s.messages.submit(m, discard()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), s.messages.wait(&ticket))
        .await
        .unwrap()
        .unwrap()
}
async fn done(s: &Setup, k: &GenerationKey) -> ControlReport {
    tokio::time::timeout(Duration::from_secs(3), s.control.wait(k))
        .await
        .unwrap()
        .unwrap()
}
async fn cancel(s: &Setup, k: &GenerationKey) {
    s.control.cancel(k).unwrap();
    done(s, k).await;
}
fn config(s: &Setup, name: &str, value: u64, mode: ApplyMode) {
    let current = s.admin.current().unwrap();
    let mut overrides = current.namespaces;
    overrides
        .entry(MESSAGE_NAMESPACE.into())
        .or_insert_with(|| NamespaceValues {
            schema_version: 1,
            values: BTreeMap::new(),
        })
        .values
        .insert(name.into(), json!(value));
    s.admin.replace(current.revision, overrides, mode).unwrap();
}
#[derive(Clone, Copy)]
enum Behavior {
    Rules,
    Confidence(u8),
    Error,
    PanicCreate,
    PanicPoll,
    Forged,
    InvalidSpan,
    Overlap,
}
struct Judge {
    behavior: Behavior,
    gate: Option<Arc<Notify>>,
    entered: Notify,
    inputs: Mutex<Vec<RelationInput>>,
}
impl Judge {
    fn new(behavior: Behavior, gate: Option<Arc<Notify>>) -> Arc<Self> {
        Arc::new(Self {
            behavior,
            gate,
            entered: Notify::new(),
            inputs: Mutex::new(vec![]),
        })
    }
}
impl RelationJudge for Judge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        self.inputs.lock().unwrap().push(input.clone());
        if matches!(self.behavior, Behavior::PanicCreate) {
            panic!("判断创建异常");
        }
        Box::pin(async move {
            self.entered.notify_one();
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            if matches!(self.behavior, Behavior::PanicPoll) {
                panic!("判断轮询异常");
            }
            if matches!(self.behavior, Behavior::Error) {
                return Err(RelationError::Unavailable);
            }
            let mut d = RulesJudge.judge(input).await?;
            match self.behavior {
                Behavior::Confidence(c) => d.parts[0].confidence = c,
                Behavior::Forged => d.target.generation += 1,
                Behavior::InvalidSpan => d.parts[0].span = Some(TextSpan { start: 6, end: 7 }),
                Behavior::Overlap => d.parts.push(d.parts[0].clone()),
                _ => {}
            }
            Ok(d)
        })
    }
}
fn reason(report: &RouteReport) -> ClarifyReason {
    match report.outcome {
        RouteOutcome::Clarify { reason, .. } => reason,
        ref other => panic!("预期澄清：{other:?}"),
    }
}

#[tokio::test]
async fn mixed_revision_preserves_utf8_sources_and_failed_input_is_not_history() {
    let p = Provider::new(vec![
        Step::blocked(Arc::new(Notify::new())),
        Step::new(final_response("按新要求完成")),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let judge = Judge::new(Behavior::Rules, None);
    let s = install(&rig, judge.clone()).await;
    let old = s
        .control
        .submit(request("s", "原要求：三页中文报告"), discard())
        .unwrap();
    p.wait_requests(1).await;
    let report = route(
        &s,
        message(
            &old,
            "mixed",
            "  /add 包含预算 📚\r\n/correct 改为两页  \r\n/continue",
        ),
    )
    .await;
    let RouteOutcome::Replaced { generation, prior } = report.outcome else {
        panic!("未重规划");
    };
    assert_eq!(prior.run.commit, CommitState::Failed);
    assert_eq!(prior.run.started_tools, Some(0));
    assert_eq!(generation.task_id, old.task_id);
    assert_eq!(generation.generation, 2);
    assert_eq!(
        done(&s, &generation).await.run.commit,
        CommitState::Completed
    );
    let text = s
        .control
        .snapshot(&generation.session)
        .unwrap()
        .unwrap()
        .input_text;
    let revision: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(revision["base_request"], "原要求：三页中文报告");
    assert_eq!(
        revision["changes"],
        json!([{"kind":"supplement","text":"包含预算 📚"},{"kind":"correction","text":"改为两页"}])
    );
    {
        let seen = judge.inputs.lock().unwrap();
        assert_eq!(seen[0].phase, ControlPhase::Generating);
        assert_eq!(seen[0].task_text, "原要求：三页中文报告");
    }
    assert_eq!(
        rig.snapshot("s").history(),
        vec![
            ChatMessage::text(ChatRole::User, text),
            ChatMessage::text(ChatRole::Assistant, "按新要求完成")
        ]
    );
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    rig.stop().await;
}

#[tokio::test]
async fn explicit_new_does_not_copy_old_request_and_duplicate_delivery_executes_once() {
    let p = Provider::new(vec![
        Step::blocked(Arc::new(Notify::new())),
        Step::new(final_response("新完成")),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install(&rig, Arc::new(RulesJudge)).await;
    let old = s
        .control
        .submit(request("s", "旧敏感要求"), discard())
        .unwrap();
    p.wait_requests(1).await;
    let m = message(&old, "new", "/cancel\n/new 独立任务");
    let ticket = s.messages.submit(m.clone(), discard()).unwrap();
    drop(s.messages.wait(&ticket));
    let duplicate = s.messages.submit(m.clone(), discard()).unwrap();
    assert_eq!(ticket, duplicate);
    let report = s.messages.wait(&duplicate).await.unwrap();
    assert_eq!(s.messages.wait(&ticket).await.unwrap(), report);
    let RouteOutcome::Replaced { generation, .. } = report.outcome else {
        panic!("未开启新任务");
    };
    done(&s, &generation).await;
    assert_eq!(generation.task_id, "new");
    assert_eq!(
        s.control
            .snapshot(&generation.session)
            .unwrap()
            .unwrap()
            .input_text,
        "独立任务"
    );
    let mut conflicting = m;
    conflicting.text = "/cancel".into();
    assert_eq!(
        s.messages.submit(conflicting, discard()),
        Err(MessageError::Conflict)
    );
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    rig.stop().await;
}

#[tokio::test]
async fn keep_conflict_unknown_and_pause_do_not_cancel_active_task() {
    let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install(&rig, Arc::new(RulesJudge)).await;
    let old = s.control.submit(request("s", "任务"), discard()).unwrap();
    p.wait_requests(1).await;
    for (index, text, expected) in [
        (0, "/continue", None),
        (1, "/unrelated", None),
        (
            2,
            "/new 新任务\n/add 旧任务要求",
            Some(ClarifyReason::Conflict),
        ),
        (3, "/add 要求\n/unrelated", Some(ClarifyReason::Conflict)),
        (4, "/pause", Some(ClarifyReason::Unsupported)),
        (5, "/resume", Some(ClarifyReason::Unsupported)),
        (
            6,
            "这是引用的 /cancel，不是命令",
            Some(ClarifyReason::LowConfidence),
        ),
        (
            7,
            "/cancel 后面有未解释文字",
            Some(ClarifyReason::LowConfidence),
        ),
    ] {
        let r = route(&s, message(&old, &format!("m-{index}"), text)).await;
        if let Some(expected) = expected {
            assert_eq!(reason(&r), expected);
        } else {
            assert_eq!(r.outcome, RouteOutcome::Unchanged);
        }
        assert!(
            !s.control
                .snapshot(&old.session)
                .unwrap()
                .unwrap()
                .cancel_requested
        );
    }
    cancel(&s, &old).await;
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    rig.stop().await;
}

#[tokio::test]
async fn clarification_answer_requires_latest_same_generation_question() {
    let p = Provider::new(vec![
        Step::blocked(Arc::new(Notify::new())),
        Step::new(final_response("答复已应用")),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let judge = Judge::new(Behavior::Rules, None);
    let s = install(&rig, judge.clone()).await;
    let old = s.control.submit(request("s", "报告"), discard()).unwrap();
    p.wait_requests(1).await;
    assert_eq!(
        reason(&route(&s, message(&old, "q1", "我不确定要怎么改")).await),
        ClarifyReason::LowConfidence
    );
    let missing = route(&s, message(&old, "q2", "/answer 两页")).await;
    assert_eq!(reason(&missing), ClarifyReason::MissingQuestion);
    let mut wrong = message(&old, "q3", "/answer 两页");
    wrong.reply_to = Some("q1".into());
    assert_eq!(
        reason(&route(&s, wrong).await),
        ClarifyReason::MissingQuestion
    );
    let mut answer = message(&old, "answer", "/answer 两页");
    answer.reply_to = Some("q3".into());
    let RouteOutcome::Replaced { generation, .. } = route(&s, answer).await.outcome else {
        panic!("答复未应用");
    };
    done(&s, &generation).await;
    {
        let inputs = judge.inputs.lock().unwrap();
        let context = inputs.last().unwrap().clarification.as_ref().unwrap();
        assert_eq!(context.question_id, "q3");
        assert_eq!(context.source_text, "/answer 两页");
        assert!(!context.prompt.is_empty());
    }
    let revision: serde_json::Value = serde_json::from_str(
        &s.control
            .snapshot(&generation.session)
            .unwrap()
            .unwrap()
            .input_text,
    )
    .unwrap();
    assert_eq!(revision["clarification_source"], "/answer 两页");
    assert_eq!(revision["changes"][0]["kind"], "answer");
    assert!(matches!(
        route(&s, message(&old, "late", "/cancel")).await.outcome,
        RouteOutcome::Stale { prior: None }
    ));
    rig.stop().await;
}

#[tokio::test]
async fn revised_task_with_existing_tools_returns_report_and_never_replays_them() {
    let p = Provider::new(vec![
        Step::new(calls()),
        Step::blocked(Arc::new(Notify::new())),
        Step::new(final_response("新的独立任务")),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install(&rig, Arc::new(RulesJudge)).await;
    let old = s
        .control
        .submit(request("s", "生成回执"), discard())
        .unwrap();
    p.wait_requests(2).await;
    let r = route(&s, message(&old, "revise", "/correct 修改回执")).await;
    assert_eq!(reason(&r), ClarifyReason::SideEffects);
    let RouteOutcome::Clarify {
        prior: Some(prior), ..
    } = r.outcome
    else {
        panic!("副作用报告丢失");
    };
    assert_eq!(prior.run.started_tools, Some(1));
    assert_eq!(prior.run.tool_results.len(), 2);
    assert_eq!(prior.run.commit, CommitState::Failed);
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    let RouteOutcome::Replaced { generation, .. } =
        route(&s, message(&old, "new", "/new 新的独立任务"))
            .await
            .outcome
    else {
        panic!("新任务未开启");
    };
    done(&s, &generation).await;
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    rig.stop().await;
}

#[tokio::test]
async fn pending_commit_blocks_new_task_and_preserves_transcript() {
    let store = Arc::new(FaultStore::default());
    let p = Provider::new(vec![Step {
        fail_commit: Some(store.clone()),
        ..Step::new(final_response("已生成"))
    }]);
    let rig = Rig::new(p.clone(), store.clone(), LlmHostConfig::default()).await;
    let s = install(&rig, Arc::new(RulesJudge)).await;
    let old = s.control.submit(request("s", "问题"), discard()).unwrap();
    assert_eq!(done(&s, &old).await.run.commit, CommitState::Pending);
    let RouteOutcome::Blocked { prior: Some(prior) } =
        route(&s, message(&old, "new", "/new 再试")).await.outcome
    else {
        panic!("未阻断");
    };
    assert_eq!(prior.run.text.as_deref(), Some("已生成"));
    assert!(prior.run.transcript.is_some());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    store.fail.store(false, Ordering::SeqCst);
    rig.stop().await;
}

#[tokio::test]
async fn slow_classification_and_failure_cannot_cancel_replacement_generation() {
    for behavior in [Behavior::Rules, Behavior::Error] {
        let gate = Arc::new(Notify::new());
        let judge = Judge::new(behavior, Some(gate.clone()));
        let p = Provider::new(vec![
            Step::new(final_response("旧完成")),
            Step::blocked(Arc::new(Notify::new())),
        ]);
        let rig = Rig::new(
            p.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install(&rig, judge.clone()).await;
        let old = s.control.submit(request("s", "旧任务"), discard()).unwrap();
        done(&s, &old).await;
        let ticket = s
            .messages
            .submit(message(&old, "late", "/cancel"), discard())
            .unwrap();
        judge.entered.notified().await;
        let new = s.control.submit(request("s", "新任务"), discard()).unwrap();
        p.wait_requests(2).await;
        gate.notify_one();
        assert!(matches!(
            s.messages.wait(&ticket).await.unwrap().outcome,
            RouteOutcome::Stale { prior: None }
        ));
        assert!(
            !s.control
                .snapshot(&new.session)
                .unwrap()
                .unwrap()
                .cancel_requested
        );
        assert_eq!(
            s.control
                .submit_if_current(&old, request("s", "错任务"), discard()),
            Err(ControlError::StaleGeneration)
        );
        cancel(&s, &new).await;
        rig.stop().await;
    }
}

#[tokio::test]
async fn timeouts_panics_and_malformed_decisions_fall_back_without_cancel() {
    for (behavior, expected, text) in [
        (Behavior::Error, RelationError::Unavailable, "/cancel"),
        (Behavior::PanicCreate, RelationError::Panicked, "/cancel"),
        (Behavior::PanicPoll, RelationError::Panicked, "/cancel"),
        (Behavior::Forged, RelationError::Protocol, "/cancel"),
        (Behavior::InvalidSpan, RelationError::Protocol, "/add 中文"),
        (Behavior::Overlap, RelationError::Protocol, "/add 中文"),
    ] {
        let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
        let rig = Rig::new(
            p.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install(&rig, Judge::new(behavior, None)).await;
        let old = s.control.submit(request("s", "任务"), discard()).unwrap();
        p.wait_requests(1).await;
        assert_eq!(
            reason(&route(&s, message(&old, "m", text)).await),
            ClarifyReason::Judge(expected)
        );
        assert!(
            !s.control
                .snapshot(&old.session)
                .unwrap()
                .unwrap()
                .cancel_requested
        );
        cancel(&s, &old).await;
        rig.stop().await;
    }
    let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install(
        &rig,
        Judge::new(Behavior::Rules, Some(Arc::new(Notify::new()))),
    )
    .await;
    config(&s, "judge_timeout_ms", 10, ApplyMode::Immediate);
    let old = s.control.submit(request("s", "任务"), discard()).unwrap();
    p.wait_requests(1).await;
    assert_eq!(
        reason(&route(&s, message(&old, "timeout", "/cancel")).await),
        ClarifyReason::Judge(RelationError::Timeout)
    );
    cancel(&s, &old).await;
    rig.stop().await;
}

#[tokio::test]
async fn confidence_updates_obey_immediate_and_new_request_modes() {
    for mode in [ApplyMode::Immediate, ApplyMode::NewRequests] {
        let gate = Arc::new(Notify::new());
        let judge = Judge::new(Behavior::Confidence(85), Some(gate.clone()));
        let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
        let rig = Rig::new(
            p.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install(&rig, judge.clone()).await;
        let old = s.control.submit(request("s", "任务"), discard()).unwrap();
        p.wait_requests(1).await;
        let ticket = s
            .messages
            .submit(message(&old, "m", "/cancel"), discard())
            .unwrap();
        judge.entered.notified().await;
        config(&s, "confidence_threshold", 90, mode);
        gate.notify_one();
        let r = s.messages.wait(&ticket).await.unwrap();
        if mode == ApplyMode::Immediate {
            assert_eq!(reason(&r), ClarifyReason::LowConfidence);
            cancel(&s, &old).await;
        } else {
            assert!(matches!(r.outcome, RouteOutcome::Cancelled { .. }));
        }
        rig.stop().await;
    }
}

#[tokio::test]
async fn input_record_limits_and_composed_size_are_enforced_before_cancel() {
    let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install(&rig, Arc::new(RulesJudge)).await;
    config(&s, "max_input_bytes", 64, ApplyMode::Immediate);
    config(&s, "max_tracked_messages", 1, ApplyMode::Immediate);
    let old = s
        .control
        .submit(request("s", "a long original user request"), discard())
        .unwrap();
    p.wait_requests(1).await;
    assert_eq!(
        s.messages
            .submit(message(&old, "large", &"x".repeat(65)), discard()),
        Err(MessageError::LimitReached)
    );
    let m = message(&old, "one", "/add 补充");
    let ticket = s.messages.submit(m.clone(), discard()).unwrap();
    assert_eq!(
        reason(&s.messages.wait(&ticket).await.unwrap()),
        ClarifyReason::TooLarge
    );
    assert_eq!(s.messages.submit(m, discard()).unwrap(), ticket);
    assert_eq!(
        s.messages
            .submit(message(&old, "two", "/cancel"), discard()),
        Err(MessageError::LimitReached)
    );
    assert!(
        !s.control
            .snapshot(&old.session)
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    cancel(&s, &old).await;
    rig.stop().await;
}

#[tokio::test]
async fn wrong_user_and_other_session_messages_cannot_cancel_task() {
    let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install(&rig, Arc::new(RulesJudge)).await;
    let old = s.control.submit(request("s", "任务"), discard()).unwrap();
    p.wait_requests(1).await;
    let mut wrong = message(&old, "wrong", "/cancel");
    wrong.target.session.user_id = "用户二".into();
    let ticket = s.messages.submit(wrong, discard()).unwrap();
    assert_eq!(
        s.messages.wait(&ticket).await,
        Err(MessageError::InvalidInput)
    );
    let mut other = message(&old, "other", "/cancel");
    other.target.session.session_id = "other".into();
    assert!(matches!(
        route(&s, other).await.outcome,
        RouteOutcome::Stale { prior: None }
    ));
    assert!(
        !s.control
            .snapshot(&old.session)
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    cancel(&s, &old).await;
    rig.stop().await;
}

#[tokio::test]
async fn stopping_router_drains_owned_classification_and_invalidates_old_handle() {
    let judge = Judge::new(Behavior::Rules, Some(Arc::new(Notify::new())));
    let p = Provider::new(vec![Step::new(final_response("完成"))]);
    let rig = Rig::new(p, Arc::new(FaultStore::default()), LlmHostConfig::default()).await;
    let s = install(&rig, judge.clone()).await;
    let old = s.control.submit(request("s", "任务"), discard()).unwrap();
    done(&s, &old).await;
    let ticket = s
        .messages
        .submit(message(&old, "m", "/new 新任务"), discard())
        .unwrap();
    let waiting = s.messages.wait(&ticket);
    judge.entered.notified().await;
    rig.kernel.stop(&id(ROUTER_PLUGIN_ID)).await.unwrap();
    assert!(matches!(
        waiting.await.unwrap().outcome,
        RouteOutcome::Stopped { prior: None }
    ));
    assert_eq!(
        s.messages
            .submit(message(&old, "next", "/cancel"), discard()),
        Err(MessageError::Unavailable)
    );
    assert_eq!(
        s.messages.wait(&ticket).await,
        Err(MessageError::Unavailable)
    );
    assert_eq!(s.control.snapshot(&old.session).unwrap().unwrap().key, old);
    rig.stop().await;
}

struct InjectedControl {
    manifest: PluginManifest,
    service: Arc<dyn ControlService>,
}
impl Plugin for InjectedControl {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let service = self.service.clone();
        Box::pin(async move {
            ctx.provide_service(
                ServiceId::new(CONTROL_SERVICE_ID)?,
                ControlServiceHandle(service),
            )?;
            Ok(None)
        })
    }
}
#[derive(Clone, Copy)]
enum Race {
    AfterWait,
    AtSubmit,
    SubmitFails,
}
struct RacingControl {
    snapshot: Arc<Mutex<ControlSnapshot>>,
    prior: ControlReport,
    race: Race,
}
impl RacingControl {
    fn switch(snapshot: &Mutex<ControlSnapshot>) {
        let mut s = snapshot.lock().unwrap();
        s.key.generation += 1;
        let key = s.key.clone();
        if let Some(report) = &mut s.report {
            report.key = key;
        }
    }
}
impl ControlService for RacingControl {
    fn submit(
        &self,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        panic!("路由必须使用有条件提交");
    }
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        assert_eq!(expected, &self.prior.key);
        match self.race {
            Race::AtSubmit => {
                Self::switch(&self.snapshot);
                Err(ControlError::StaleGeneration)
            }
            Race::SubmitFails => Err(ControlError::Unavailable),
            Race::AfterWait => panic!("收尾后已切换，不得提交"),
        }
    }
    fn cancel(&self, k: &GenerationKey) -> ControlResult<CancelDisposition> {
        assert_eq!(k, &self.prior.key);
        Ok(CancelDisposition::AlreadyFinished)
    }
    fn wait(&self, _: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        let prior = self.prior.clone();
        let state = self.snapshot.clone();
        let race = self.race;
        Box::pin(async move {
            if matches!(race, Race::AfterWait) {
                Self::switch(&state);
            }
            Ok(prior)
        })
    }
    fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        Ok(Some(self.snapshot.lock().unwrap().clone()))
    }
    fn accepts(&self, _: &ControlEvent) -> bool {
        false
    }
}
#[tokio::test]
async fn races_and_submit_failure_after_cancel_preserve_actual_side_effect_report() {
    for race in [Race::AfterWait, Race::AtSubmit, Race::SubmitFails] {
        let key = GenerationKey {
            session: key("s"),
            task_id: "task".into(),
            controller_epoch: [1; 16],
            generation: 1,
        };
        let prior = ControlReport {
            key: key.clone(),
            cancel_requested: false,
            run: RunReport {
                turn_id: Some(1),
                commit: CommitState::Completed,
                text: Some("已提交完整结果".into()),
                transcript: Some(vec![]),
                started_tools: Some(1),
                tool_results: vec![ToolResult::success("receipt", json!({"saved":true})).unwrap()],
                failure: None,
            },
        };
        let injected = Arc::new(RacingControl {
            snapshot: Arc::new(Mutex::new(ControlSnapshot {
                key: key.clone(),
                input_text: "原要求".into(),
                phase: ControlPhase::Finished,
                cancel_requested: false,
                events_retired: false,
                turn_id: Some(1),
                report: Some(prior.clone()),
            })),
            prior: prior.clone(),
            race,
        });
        let rig = Rig::new(
            Provider::new(vec![]),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install_using(&rig, Arc::new(RulesJudge), Some(injected)).await;
        let report = route(&s, message(&key, "new", "/new 独立任务")).await;
        let retained = match report.outcome {
            RouteOutcome::Stale { prior: Some(p) } if !matches!(race, Race::SubmitFails) => p,
            RouteOutcome::ControlFailed {
                error: ControlError::Unavailable,
                prior: p,
            } if matches!(race, Race::SubmitFails) => p,
            other => panic!("报告丢失：{other:?}"),
        };
        assert_eq!(*retained, prior);
        rig.stop().await;
    }
}

fn semantic(intent: &str, confidence: u8, text: Option<&str>) -> Step {
    Step::new(final_response(
        &json!({
            "parts":[{"intent":intent,"confidence":confidence,"text":text}],
            "explanation":"可见依据"
        })
        .to_string(),
    ))
}

#[tokio::test]
async fn optional_judge_failures_fall_back_once_and_cancel_waits_for_commit() {
    let primaries: Vec<Option<Arc<dyn RelationJudge>>> = vec![
        None,
        Some(Judge::new(Behavior::Error, None)),
        Some(Judge::new(Behavior::PanicCreate, None)),
        Some(Judge::new(Behavior::PanicPoll, None)),
        Some(Judge::new(Behavior::Forged, None)),
        Some(Judge::new(Behavior::Confidence(70), None)),
        Some(Judge::new(Behavior::Rules, Some(Arc::new(Notify::new())))),
    ];
    for primary in primaries {
        let task = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
        let model = Provider::new(vec![semantic("cancel", 95, None)]);
        let rig = Rig::new(
            task.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install_plugin(
            &rig,
            RelationPlugin::with_fallback(
                primary,
                Arc::new(LlmRelationJudge::new(model.clone())),
            )
            .unwrap(),
            None,
        )
        .await;
        config(&s, "judge_timeout_ms", 100, ApplyMode::Immediate);
        let old = s.control.submit(request("s", "任务"), discard()).unwrap();
        task.wait_requests(1).await;
        let report = route(&s, message(&old, "natural", "取消这个任务")).await;
        let RouteOutcome::Cancelled { prior } = report.outcome else {
            panic!("回退未完成取消");
        };
        assert_eq!(prior.run.commit, CommitState::Failed);
        assert_eq!(prior.run.started_tools, Some(0));
        assert_eq!(prior.key, old);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert_eq!(task.requests.lock().unwrap().len(), 1);
        rig.stop().await;
    }
}

#[tokio::test]
async fn explicit_and_malformed_commands_never_reach_semantic_providers() {
    let task = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let primary = Provider::new(vec![]);
    let fallback = Provider::new(vec![]);
    let rig = Rig::new(
        task.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install_plugin(
        &rig,
        RelationPlugin::with_fallback(
            Some(Arc::new(LlmRelationJudge::new(primary.clone()))),
            Arc::new(LlmRelationJudge::new(fallback.clone())),
        )
        .unwrap(),
        None,
    )
    .await;
    let old = s.control.submit(request("s", "任务"), discard()).unwrap();
    task.wait_requests(1).await;
    for (index, text) in [
        "/continue",
        "/pause",
        "/cancel 带有未解释参数",
        "/add",
        "/unknown",
        "/add 新要求\n其他文字",
    ]
    .into_iter()
    .enumerate()
    {
        let report = route(&s, message(&old, &format!("command-{index}"), text)).await;
        assert!(matches!(
            report.outcome,
            RouteOutcome::Unchanged | RouteOutcome::Clarify { .. }
        ));
    }
    assert!(primary.requests.lock().unwrap().is_empty());
    assert!(fallback.requests.lock().unwrap().is_empty());
    assert!(!s.control.snapshot(&old.session).unwrap().unwrap().cancel_requested);
    cancel(&s, &old).await;
    rig.stop().await;
}

#[tokio::test]
async fn primary_confidence_snapshot_controls_fallback_and_final_action() {
    for mode in [ApplyMode::Immediate, ApplyMode::NewRequests] {
        let gate = Arc::new(Notify::new());
        let mut step = semantic("unrelated", 85, None);
        step.gate = Some(gate.clone());
        let primary = Provider::new(vec![step]);
        let fallback = Provider::new(vec![semantic("unrelated", 95, None)]);
        let task = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
        let rig = Rig::new(
            task.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install_plugin(
            &rig,
            RelationPlugin::with_fallback(
                Some(Arc::new(LlmRelationJudge::new(primary.clone()))),
                Arc::new(LlmRelationJudge::new(fallback.clone())),
            )
            .unwrap(),
            None,
        )
        .await;
        let old = s.control.submit(request("s", "任务"), discard()).unwrap();
        task.wait_requests(1).await;
        let ticket = s.messages.submit(message(&old, "m", "顺便问个问题"), discard()).unwrap();
        primary.wait_requests(1).await;
        config(&s, "confidence_threshold", 90, mode);
        gate.notify_one();
        assert_eq!(s.messages.wait(&ticket).await.unwrap().outcome, RouteOutcome::Unchanged);
        assert_eq!(
            fallback.requests.lock().unwrap().len(),
            usize::from(mode == ApplyMode::Immediate)
        );
        assert!(!s.control.snapshot(&old.session).unwrap().unwrap().cancel_requested);
        cancel(&s, &old).await;
        rig.stop().await;
    }
}

#[tokio::test]
async fn semantic_errors_low_confidence_and_timeout_clarify_without_cancelling() {
    for (step, expected) in [
        (Step::new(final_response("bad json")), ClarifyReason::Judge(RelationError::Protocol)),
        (semantic("cancel", 40, None), ClarifyReason::LowConfidence),
        (Step::blocked(Arc::new(Notify::new())), ClarifyReason::Judge(RelationError::Timeout)),
    ] {
        let task = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
        let model = Provider::new(vec![step]);
        let rig = Rig::new(
            task.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        let s = install_plugin(
            &rig,
            RelationPlugin::with_fallback(None, Arc::new(LlmRelationJudge::new(model.clone()))).unwrap(),
            None,
        )
        .await;
        config(&s, "judge_timeout_ms", 100, ApplyMode::Immediate);
        let old = s.control.submit(request("s", "任务"), discard()).unwrap();
        task.wait_requests(1).await;
        let report = route(&s, message(&old, "m", "可以调整一下吗")).await;
        assert_eq!(reason(&report), expected);
        assert!(!s.control.snapshot(&old.session).unwrap().unwrap().cancel_requested);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        cancel(&s, &old).await;
        rig.stop().await;
    }
}

#[tokio::test]
async fn late_semantic_cancel_cannot_cancel_a_replacement_generation() {
    let task = Provider::new(vec![
        Step::new(final_response("旧完成")),
        Step::blocked(Arc::new(Notify::new())),
    ]);
    let gate = Arc::new(Notify::new());
    let mut step = semantic("cancel", 95, None);
    step.gate = Some(gate.clone());
    let model = Provider::new(vec![step]);
    let rig = Rig::new(
        task.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install_plugin(
        &rig,
        RelationPlugin::with_fallback(None, Arc::new(LlmRelationJudge::new(model.clone()))).unwrap(),
        None,
    )
    .await;
    let old = s.control.submit(request("s", "旧任务"), discard()).unwrap();
    done(&s, &old).await;
    let ticket = s.messages.submit(message(&old, "m", "取消它"), discard()).unwrap();
    model.wait_requests(1).await;
    let new = s.control.submit(request("s", "新任务"), discard()).unwrap();
    task.wait_requests(2).await;
    gate.notify_one();
    assert!(matches!(
        s.messages.wait(&ticket).await.unwrap().outcome,
        RouteOutcome::Stale { prior: None }
    ));
    assert!(!s.control.snapshot(&new.session).unwrap().unwrap().cancel_requested);
    cancel(&s, &new).await;
    rig.stop().await;
}

#[tokio::test]
async fn qualified_optional_result_skips_fallback_provider() {
    let primary = Provider::new(vec![semantic("unrelated", 95, None)]);
    let fallback = Provider::new(vec![]);
    let task = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        task.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let s = install_plugin(
        &rig,
        RelationPlugin::with_fallback(
            Some(Arc::new(LlmRelationJudge::new(primary.clone()))),
            Arc::new(LlmRelationJudge::new(fallback.clone())),
        )
        .unwrap(),
        None,
    )
    .await;
    let old = s.control.submit(request("s", "任务"), discard()).unwrap();
    task.wait_requests(1).await;
    assert_eq!(
        route(&s, message(&old, "m", "闲聊")).await.outcome,
        RouteOutcome::Unchanged
    );
    assert_eq!(primary.requests.lock().unwrap().len(), 1);
    assert!(fallback.requests.lock().unwrap().is_empty());
    cancel(&s, &old).await;
    rig.stop().await;
}
