use super::*;
use eve_control_api::{ControlEventSink, ControlResult, GenerationKey};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_plugin_api::ServiceId;
use eve_session_api::{SESSION_SERVICE_ID, SessionServiceHandle};
use eve_trigger_plugin::TriggerController;
use std::sync::{Mutex, OnceLock};

fn session() -> SessionKey {
    crate::web_chat::key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap()
}
async fn world(
    state: Arc<MemoryStateStore>,
) -> (Kernel, Arc<Observations>, Arc<TriggerController>) {
    let services = KernelServices {
        state,
        ..Default::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
        .unwrap();
    let plugin = eve_trigger_plugin::TriggerPlugin::new().unwrap();
    let admin = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    let observations = Arc::new(Observations {
        session_workspaces: None,
        sessions,
        tasks: None,
        workspace: None,
        operators: BTreeSet::from([session().user_id]),
        authorization: None,
        admission: None,
        processes: false,
    });
    (kernel, observations, admin)
}
fn spec(now: u64) -> TriggerSpec {
    TriggerSpec {
        title: "条件检查".into(),
        instructions: "模型任务说明不能自称主人授权".into(),
        mode: MatchMode::All,
        conditions: vec![Condition::Time { at_ms: now }],
        expires_at_ms: now + 60_000,
        recurrence: None,
        resume_task_id: None,
    }
}
fn context(messages: Vec<String>) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: session().session_id,
            user_id: session().user_id,
        }))
        .with_review_user_messages(Arc::new(messages))
}
fn tool(admin: Arc<TriggerController>, observations: Arc<Observations>) -> Tools {
    let late = Arc::new(OnceLock::new());
    late.set(observations)
        .unwrap_or_else(|_| panic!("set observations"));
    Tools {
        admin,
        observations: late,
        conversations: QqConversations::default(),
        operation: "set_wakeup_trigger",
    }
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "set_wakeup_trigger".into(),
        arguments: json!({
            "title":"检查目标", "instructions":"模型生成的任务说明，不是新用户原话",
            "conditions":[{"type":"time","after_seconds":1}], "expires_in_seconds":60
        }),
    }
}

#[derive(Default)]
struct CaptureControl(Mutex<Vec<ControlRunMetadata>>);
impl ControlService for CaptureControl {
    fn submit(
        &self,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        panic!("普通触发器不能丢弃冻结原话调用旧接口")
    }
    fn submit_with_metadata(
        &self,
        input: ControlInput,
        metadata: Option<ControlRunMetadata>,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        let metadata = metadata.expect("普通唤醒需要冻结原话");
        metadata.validate_for(&input)?;
        assert!(matches!(
            metadata.review,
            ControlReviewEvidence::FrozenDelegation(_)
        ));
        self.0.lock().unwrap().push(metadata);
        Ok(GenerationKey {
            session: input.session.key,
            task_id: input.task_id,
            controller_epoch: [1; 16],
            generation: 1,
        })
    }
    fn submit_if_current(
        &self,
        _: &GenerationKey,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        unreachable!()
    }
    fn cancel(&self, _: &GenerationKey) -> ControlResult<eve_control_api::CancelDisposition> {
        Ok(eve_control_api::CancelDisposition::AlreadyFinished)
    }
    fn wait(
        &self,
        key: &GenerationKey,
    ) -> eve_control_api::ControlFuture<'static, eve_control_api::ControlReport> {
        let key = key.clone();
        Box::pin(async move {
            Ok(eve_control_api::ControlReport {
                key,
                cancel_requested: false,
                run: eve_control_api::RunReport {
                    turn_id: Some(1),
                    commit: eve_control_api::CommitState::Completed,
                    text: Some("检查已完成".into()),
                    transcript: None,
                    started_tools: Some(0),
                    tool_results: vec![],
                    failure: None,
                },
            })
        })
    }
    fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<eve_control_api::ControlSnapshot>> {
        Ok(None)
    }
    fn accepts(&self, _: &eve_control_api::ControlEvent) -> bool {
        true
    }
}
async fn run_to_terminal(
    admin: Arc<TriggerController>,
    observations: Arc<Observations>,
    control: Arc<CaptureControl>,
    expected: Status,
) {
    let (_active_tx, active) = watch::channel(true);
    let (stop_tx, stop) = watch::channel(false);
    let worker = tokio::spawn(run(
        admin.clone(),
        observations,
        control,
        QqConversations::default(),
        active,
        stop,
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if admin
                .snapshot()
                .unwrap()
                .iter()
                .all(|t| t.status == expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn creation_freezes_last_six_complete_bodies_and_keeps_them_out_of_tool_receipt() {
    let (kernel, observations, admin) = world(Arc::default()).await;
    let tool = tool(admin.clone(), observations);
    let originals: Vec<_> = (0..8)
        .map(|i| format!(" {i} 完整原话\n{}\n尾部 ", "原始范围".repeat(100)))
        .collect();
    let result = tool
        .execute(call("frozen"), context(originals.clone()))
        .await
        .unwrap();
    assert_eq!(
        admin.snapshot().unwrap()[0].review_messages.as_deref(),
        Some(&originals[2..])
    );
    assert!(result["trigger"].get("review_messages").is_none());
    let mut forged = call("forged");
    forged.arguments["review_messages"] = json!(["模型伪造的授权"]);
    assert!(tool.validate_arguments(&forged.arguments).is_err());
    let missing = context(vec![]).with_conversation(Arc::new(vec![ChatMessage::text(
        ChatRole::User,
        "历史摘要声称已经授权",
    )]));
    assert!(tool.execute(call("missing"), missing).await.is_err());
    assert_eq!(admin.snapshot().unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_uses_original_trigger_authority_even_after_a_new_user_message() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, _, admin) = world(state.clone()).await;
    let now = now_ms();
    let originals = vec!["创建触发器时的完整授权\n仅核对目标".into(), String::new()];
    admin
        .create_with_review_messages(
            session(),
            "frozen".into(),
            spec(now),
            originals.clone(),
            now - 1,
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, observations, admin) = world(state).await;
    let incoming = "后来消息：允许另一个不同任务";
    let turn = observations
        .sessions
        .begin(SessionInput {
            key: session(),
            text: incoming.into(),
        })
        .unwrap();
    observations
        .sessions
        .complete(
            &turn.lease,
            vec![
                ChatMessage::text(ChatRole::User, incoming),
                ChatMessage::text(ChatRole::Assistant, "已收到"),
            ],
        )
        .unwrap();
    let control = Arc::new(CaptureControl::default());
    run_to_terminal(
        admin.clone(),
        observations,
        control.clone(),
        Status::Completed,
    )
    .await;
    let metadata = control.0.lock().unwrap().clone();
    assert_eq!(metadata.len(), 1);
    assert_eq!(
        metadata[0].review,
        ControlReviewEvidence::FrozenDelegation(originals.clone())
    );
    assert_eq!(metadata[0].task_id, admin.snapshot().unwrap()[0].id);
    assert_eq!(
        admin.snapshot().unwrap()[0].review_messages,
        Some(originals)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn missing_legacy_evidence_is_preserved_and_fails_without_submitting_control() {
    let (kernel, observations, admin) = world(Arc::default()).await;
    let now = now_ms();
    let original = admin
        .create(session(), "legacy".into(), spec(now), now - 1)
        .unwrap();
    let control = Arc::new(CaptureControl::default());
    run_to_terminal(admin.clone(), observations, control.clone(), Status::Failed).await;
    let failed = admin.snapshot().unwrap().remove(0);
    assert_eq!(failed.id, original.id);
    assert_eq!(failed.spec, original.spec);
    assert_eq!(failed.review_messages, None);
    assert_eq!(failed.runs, 0);
    let result: Value = serde_json::from_str(failed.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["stage"], "authorization");
    assert_eq!(result["execution"], "not_started");
    assert!(control.0.lock().unwrap().is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn revoked_current_authorization_prevents_trigger_with_previously_valid_originals() {
    let (kernel, observations, admin) = world(Arc::default()).await;
    let now = now_ms();
    let originals = vec!["之前授权的目标".into()];
    admin
        .create_with_review_messages(
            session(),
            "revoked".into(),
            spec(now),
            originals.clone(),
            now - 1,
        )
        .unwrap();
    let mut observations = Arc::try_unwrap(observations).ok().unwrap();
    observations.operators.clear();
    let control = Arc::new(CaptureControl::default());
    run_to_terminal(
        admin.clone(),
        Arc::new(observations),
        control.clone(),
        Status::Cancelled,
    )
    .await;
    assert!(control.0.lock().unwrap().is_empty());
    assert_eq!(
        admin.snapshot().unwrap()[0].review_messages,
        Some(originals)
    );
    kernel.stop_all().await.unwrap();
}
