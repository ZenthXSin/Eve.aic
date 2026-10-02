#[path = "../examples/support/cognition.rs"]
mod support;
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_plugin::COGNITION_STATE_KEY;
use eve_kernel::backends::MemoryStateStore;
use eve_llm_api::*;
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_session_api::SESSION_PLUGIN_ID;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;
use tokio::sync::Notify;

#[derive(Default)]
struct FaultStore {
    memory: MemoryStateStore,
    fail_cognition: AtomicBool,
    fail_session: AtomicBool,
    pause_session: AtomicBool,
    entered: Notify,
    released: Mutex<bool>,
    release: Condvar,
}
impl StateStore for FaultStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        if (namespace.as_str() == COGNITION_PLUGIN_ID && self.fail_cognition.load(Ordering::SeqCst))
            || (namespace.as_str() == SESSION_PLUGIN_ID && self.fail_session.load(Ordering::SeqCst))
        {
            return Err(PluginError::State("backend-secret".into()));
        }
        if namespace.as_str() == SESSION_PLUGIN_ID
            && self.pause_session.load(Ordering::SeqCst)
            && std::str::from_utf8(&bytes).unwrap().contains("Completed")
        {
            self.entered.notify_one();
            let released = self.released.lock().unwrap();
            let (released, timeout) = self
                .release
                .wait_timeout_while(released, Duration::from_secs(5), |value| !*value)
                .unwrap();
            if timeout.timed_out() && !*released {
                return Err(PluginError::State("提交等待超时".into()));
            }
        }
        self.memory.set(namespace, key, bytes)
    }
}
struct Model {
    requests: AtomicUsize,
    calls: usize,
    direct_final: bool,
    fault: Option<Arc<FaultStore>>,
    fail_cognition: bool,
    fail_session: bool,
}
impl Model {
    fn new(calls: usize) -> Arc<Self> {
        Arc::new(Self {
            requests: AtomicUsize::new(0),
            calls,
            direct_final: false,
            fault: None,
            fail_cognition: false,
            fail_session: false,
        })
    }
}
impl LlmProvider for Model {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let final_request = request
            .messages
            .iter()
            .any(|message| message.role == ChatRole::Tool);
        if let Some(fault) = self.fault.as_ref().filter(|_| final_request) {
            if self.fail_cognition {
                fault.fail_cognition.store(true, Ordering::SeqCst);
            }
            if self.fail_session {
                fault.fail_session.store(true, Ordering::SeqCst);
            }
        }
        Box::pin(async move {
            Ok(if final_request || self.direct_final {
                ModelResponse::Final {
                    text: "模型声称完成".into(),
                }
            } else {
                call_response(self.calls)
            })
        })
    }
}
async fn wait_idle_evaluations(controller: &eve_cognition_loop_plugin::LoopController) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while controller.stats().unwrap().evaluations < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn no_input_progress_coalesces_wakes_and_uses_real_control_feedback() {
    let model = Model::new(1);
    let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
    rig.seed(vec![goal("target")]);
    let controller = rig.start_loop(options()).await;
    for _ in 0..100 {
        controller.wake(WakeReason::StateChanged).unwrap();
        rig.events
            .emit(eve_plugin_api::Event::new(LOOP_WAKE_EVENT_ID, Vec::new()).unwrap())
            .unwrap();
    }
    let status = rig
        .registry
        .get(&eve_plugin_api::ServiceId::new(LOOP_STATUS_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<LoopStatusHandle>()
        .unwrap()
        .0
        .clone();
    rig.wait_terminal("target").await;
    controller.shutdown().await.unwrap();
    assert!(status.stats().is_err());
    let snapshot = rig.admin.snapshot().unwrap();
    let target = &snapshot.state.goals["target"];
    assert_eq!(target.status, GoalStatus::Completed);
    assert!(target.feedback.as_ref().unwrap().verification_met);
    assert_eq!(target.feedback.as_ref().unwrap().started_tools, Some(1));
    assert_eq!(model.requests.load(Ordering::SeqCst), 2);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
    let stats = controller.stats().unwrap();
    assert_eq!(
        (
            stats.submitted,
            stats.completed,
            stats.model_requests,
            stats.admitted_tool_calls
        ),
        (1, 1, 2, 1)
    );
    assert!(stats.coalesced_wakes > 0);
    assert!(!format!("{target:?}").contains("cognition-proof"));
    rig.close().await;
    assert!(controller.wake(WakeReason::StateChanged).is_err());
}
#[tokio::test]
async fn request_and_batch_limits_reject_before_tool_side_effects() {
    for (requests, tools, calls) in [(1, 1, 1), (2, 0, 1), (2, 1, 2)] {
        let model = Model::new(calls);
        let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
        let mut target = goal("target");
        target.budget.max_model_requests = requests;
        target.budget.max_tool_calls = tools;
        rig.seed(vec![target]);
        let controller = rig.start_loop(options()).await;
        rig.wait_terminal("target").await;
        controller.shutdown().await.unwrap();
        assert_eq!(model.requests.load(Ordering::SeqCst), 1);
        assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
        let snapshot = rig.admin.snapshot().unwrap();
        assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Blocked);
        assert_eq!(
            snapshot.state.goals["target"]
                .feedback
                .as_ref()
                .unwrap()
                .started_tools,
            Some(0)
        );
        rig.close().await;
    }
}
#[tokio::test]
async fn model_success_text_without_receipt_cannot_complete_goal() {
    let model = Arc::new(Model {
        direct_final: true,
        requests: AtomicUsize::new(0),
        calls: 1,
        fault: None,
        fail_cognition: false,
        fail_session: false,
    });
    let rig = Rig::open(Arc::new(MemoryStateStore::default()), model, None).await;
    rig.seed(vec![goal("target")]);
    let controller = rig.start_loop(options()).await;
    rig.wait_terminal("target").await;
    controller.shutdown().await.unwrap();
    let snapshot = rig.admin.snapshot().unwrap();
    assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Blocked);
    assert!(
        !snapshot.state.goals["target"]
            .feedback
            .as_ref()
            .unwrap()
            .verification_met
    );
    rig.close().await;
}
#[tokio::test]
async fn scope_expiration_waiting_and_unknown_verification_stay_idle() {
    let model = Model::new(1);
    let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
    let mut foreign = goal("foreign");
    foreign.visibility = Visibility::User("alice".into());
    let mut source = goal("source");
    source.source.channel = "qq".into();
    let mut expired = goal("expired");
    expired.expires_at_ms = Some(now_ms() - 1);
    let mut waiting = goal("waiting");
    waiting.status = GoalStatus::Waiting;
    waiting.wait_reason = Some("等待用户".into());
    let mut unknown = goal("unknown");
    unknown.verification = "模型说成功即可".into();
    for target in [&mut source, &mut expired, &mut waiting, &mut unknown] {
        target.visibility = Visibility::Public;
    }
    rig.seed(vec![foreign, source, expired, waiting, unknown]);
    let before = rig.admin.snapshot().unwrap();
    let mut settings = options();
    settings.scope.access = ReadAccess::User("bob".into());
    let controller = rig.start_loop(settings).await;
    wait_idle_evaluations(&controller).await;
    controller.shutdown().await.unwrap();
    assert_eq!(before, rig.admin.snapshot().unwrap());
    assert_eq!(model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    rig.close().await;
}

#[tokio::test]
async fn cancel_and_total_deadline_wait_for_tool_drop() {
    for deadline in [false, true] {
        let model = Model::new(1);
        let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
        rig.probe.block.store(true, Ordering::SeqCst);
        let mut target = goal("target");
        if deadline {
            target.budget.timeout_ms = 250;
        }
        rig.seed(vec![target]);
        let controller = rig.start_loop(options()).await;
        tokio::time::timeout(Duration::from_secs(3), rig.probe.entered.notified())
            .await
            .unwrap();
        if !deadline {
            assert!(controller.cancel_current().unwrap());
        }
        rig.wait_terminal("target").await;
        controller.shutdown().await.unwrap();
        assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
        assert_eq!(rig.probe.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(model.requests.load(Ordering::SeqCst), 1);
        let snapshot = rig.admin.snapshot().unwrap();
        assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Cancelled);
        assert!(
            !snapshot.state.goals["target"]
                .feedback
                .as_ref()
                .unwrap()
                .verification_met
        );
        rig.close().await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_verified_result_wins_cancel_race() {
    let store = Arc::new(FaultStore::default());
    store.pause_session.store(true, Ordering::SeqCst);
    let model = Model::new(1);
    let rig = Rig::open(store.clone(), model, None).await;
    rig.seed(vec![goal("target")]);
    let controller = rig.start_loop(options()).await;
    tokio::time::timeout(Duration::from_secs(3), store.entered.notified())
        .await
        .unwrap();
    assert!(controller.cancel_current().unwrap());
    let attempt = rig.admin.snapshot().unwrap().state.goals["target"]
        .execution
        .clone()
        .unwrap();
    let session = eve_session_api::SessionKey::new(attempt.session_id, "internal").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !rig
            .control
            .snapshot(&session)
            .unwrap()
            .unwrap()
            .cancel_requested
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    *store.released.lock().unwrap() = true;
    store.release.notify_all();
    rig.wait_terminal("target").await;
    controller.shutdown().await.unwrap();
    let snapshot = rig.admin.snapshot().unwrap();
    assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Completed);
    assert!(
        snapshot.state.goals["target"]
            .feedback
            .as_ref()
            .unwrap()
            .verification_met
    );
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
    rig.close().await;
}
#[tokio::test]
async fn feedback_save_failure_preserves_executing_and_restart_blocks_without_replay() {
    let store = Arc::new(FaultStore::default());
    let model = Arc::new(Model {
        requests: AtomicUsize::new(0),
        calls: 1,
        direct_final: false,
        fault: Some(store.clone()),
        fail_cognition: true,
        fail_session: false,
    });
    let rig = Rig::open(store.clone(), model.clone(), None).await;
    rig.seed(vec![goal("target")]);
    let controller = rig.start_loop(options()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while controller.stats().unwrap().feedback_save_failures == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(controller.shutdown().await.is_err());
    let stats = controller.stats().unwrap();
    assert_eq!(
        (
            stats.model_requests,
            stats.admitted_tool_calls,
            stats.started_tools
        ),
        (2, 1, 1)
    );
    assert_eq!(
        rig.admin.snapshot().unwrap().state.goals["target"].status,
        GoalStatus::Executing
    );
    let original = store
        .get(&id(COGNITION_PLUGIN_ID), COGNITION_STATE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
    assert_eq!(model.requests.load(Ordering::SeqCst), 2);
    assert!(rig.kernel.stop_all().await.is_err());
    assert!(rig.admin.snapshot().is_err());
    for owner in [LOOP_PLUGIN_ID, eve_control_api::CONTROL_PLUGIN_ID] {
        rig.kernel.unregister(&id(owner)).unwrap();
    }
    assert_eq!(
        store
            .get(&id(COGNITION_PLUGIN_ID), COGNITION_STATE_KEY)
            .unwrap(),
        Some(original)
    );
    drop(rig);
    store.fail_cognition.store(false, Ordering::SeqCst);
    let resumed_model = Model::new(1);
    let resumed = Rig::open(store, resumed_model.clone(), None).await;
    let snapshot = resumed.admin.snapshot().unwrap();
    assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Blocked);
    assert_eq!(
        snapshot.state.goals["target"].block_reason,
        Some(BlockReason::Interrupted)
    );
    let controller = resumed.start_loop(options()).await;
    wait_idle_evaluations(&controller).await;
    controller.shutdown().await.unwrap();
    assert_eq!(resumed_model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(resumed.probe.started.load(Ordering::SeqCst), 0);
    resumed.close().await;
}
#[tokio::test]
async fn pending_session_feedback_is_blocked_and_never_marked_completed() {
    let store = Arc::new(FaultStore::default());
    let model = Arc::new(Model {
        requests: AtomicUsize::new(0),
        calls: 1,
        direct_final: false,
        fault: Some(store.clone()),
        fail_cognition: false,
        fail_session: true,
    });
    let rig = Rig::open(store.clone(), model, None).await;
    rig.seed(vec![goal("target")]);
    let controller = rig.start_loop(options()).await;
    rig.wait_terminal("target").await;
    controller.shutdown().await.unwrap();
    let snapshot = rig.admin.snapshot().unwrap();
    let target = &snapshot.state.goals["target"];
    assert_eq!(target.status, GoalStatus::Blocked);
    assert_eq!(target.block_reason, Some(BlockReason::UnknownCommit));
    assert_eq!(
        target.feedback.as_ref().unwrap().commit,
        ExecutionCommit::Pending
    );
    assert!(!target.feedback.as_ref().unwrap().verification_met);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
    store.fail_session.store(false, Ordering::SeqCst);
    rig.close().await;
}
struct InvalidResolver;
impl LlmModelResolver for InvalidResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Err(LlmError::Configuration("无效主模型".into()))
    }
}
#[tokio::test]
async fn invalid_model_selection_has_zero_requests_and_no_session_pending() {
    let store = Arc::new(MemoryStateStore::default());
    let model = Model::new(1);
    let rig = Rig::open(
        store.clone(),
        model.clone(),
        Some(Arc::new(InvalidResolver)),
    )
    .await;
    rig.seed(vec![goal("target")]);
    let controller = rig.start_loop(options()).await;
    rig.wait_terminal("target").await;
    controller.shutdown().await.unwrap();
    let snapshot = rig.admin.snapshot().unwrap();
    assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Blocked);
    assert_eq!(
        snapshot.state.goals["target"]
            .feedback
            .as_ref()
            .unwrap()
            .commit,
        ExecutionCommit::NotStarted
    );
    assert_eq!(model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    assert!(
        store
            .get(
                &id(SESSION_PLUGIN_ID),
                eve_session_plugin::SESSION_STATE_KEY,
            )
            .unwrap()
            .is_none()
    );
    rig.close().await;
}
#[tokio::test]
async fn drive_ranking_and_start_budget_leave_other_goals_ready() {
    let model = Model::new(1);
    let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
    let mut high = goal("high");
    high.priority = 90;
    let mut low = goal("low");
    low.priority = 10;
    rig.seed(vec![low, high]);
    let controller = rig.start_loop(options()).await;
    rig.wait_terminal("high").await;
    wait_idle_evaluations(&controller).await;
    controller.shutdown().await.unwrap();
    let snapshot = rig.admin.snapshot().unwrap();
    assert_eq!(snapshot.state.goals["high"].status, GoalStatus::Completed);
    assert_eq!(snapshot.state.goals["low"].status, GoalStatus::Ready);
    assert_eq!(snapshot.state.drives["eve.loop.drive.0"].strength, 90);
    assert_eq!(model.requests.load(Ordering::SeqCst), 2);
    assert_eq!(controller.stats().unwrap().submitted, 1);
    rig.close().await;
}

#[tokio::test]
async fn control_requests_without_host_budget_binding_cannot_start_a_session() {
    use eve_control_api::{CommitState, ControlInput, DiscardControlEvents};
    let store = Arc::new(MemoryStateStore::default());
    let model = Model::new(1);
    let rig = Rig::open(store.clone(), model.clone(), None).await;
    let key = rig
        .control
        .submit(
            ControlInput {
                session: eve_session_api::SessionInput {
                    key: eve_session_api::SessionKey::new("bypass", "internal").unwrap(),
                    text: "尝试不带预算执行".into(),
                },
                task_id: "bypass".into(),
            },
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    let report = rig.control.wait(&key).await.unwrap();
    assert_eq!(report.run.commit, CommitState::NotStarted);
    assert_eq!(report.run.started_tools, Some(0));
    assert_eq!(model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    assert!(
        store
            .get(
                &id(SESSION_PLUGIN_ID),
                eve_session_plugin::SESSION_STATE_KEY
            )
            .unwrap()
            .is_none()
    );
    rig.close().await;
}

#[tokio::test]
async fn executing_marker_save_failure_has_zero_submit_and_keeps_ready_bytes() {
    let store = Arc::new(FaultStore::default());
    let model = Model::new(1);
    let rig = Rig::open(store.clone(), model.clone(), None).await;
    rig.seed(vec![goal("target")]);
    let original = store
        .get(&id(COGNITION_PLUGIN_ID), COGNITION_STATE_KEY)
        .unwrap()
        .unwrap();
    store.fail_cognition.store(true, Ordering::SeqCst);
    let controller = rig.start_loop(options()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while controller.stats().unwrap().evaluations == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let error = controller.shutdown().await.unwrap_err();
    assert!(!error.to_string().contains("backend-secret"));
    let stats = controller.stats().unwrap();
    assert_eq!(
        (stats.submitted, stats.model_requests, stats.started_tools),
        (0, 0, 0)
    );
    assert_eq!(model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.admin.snapshot().unwrap().state.goals["target"].status,
        GoalStatus::Ready
    );
    assert_eq!(
        store
            .get(&id(COGNITION_PLUGIN_ID), COGNITION_STATE_KEY)
            .unwrap(),
        Some(original)
    );
    store.fail_cognition.store(false, Ordering::SeqCst);
    assert!(rig.kernel.stop_all().await.is_err());
    for owner in [LOOP_PLUGIN_ID, eve_control_api::CONTROL_PLUGIN_ID] {
        rig.kernel.unregister(&id(owner)).unwrap();
    }
}

#[tokio::test]
async fn service_conflict_rolls_back_without_waiting_for_unspawned_worker() {
    use eve_cognition_loop_plugin::{
        CognitionLoopPlugin, EchoReceiptVerifier, PriorityDrivePolicy,
    };
    use eve_plugin_api::{
        Cleanup, Plugin, PluginContext, PluginFuture, PluginManifest, ServiceId,
    };
    use eve_runtime::ControlGoalExecutor;
    struct Conflict(PluginManifest);
    impl Plugin for Conflict {
        fn manifest(&self) -> &PluginManifest {
            &self.0
        }
        fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
            Box::pin(async move {
                context.provide_service(ServiceId::new(LOOP_STATUS_SERVICE_ID)?, ())?;
                Ok(None)
            })
        }
    }
    let model = Model::new(1);
    let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
    rig.seed(vec![goal("target")]);
    let before = rig.admin.snapshot().unwrap();
    rig.kernel
        .register(Box::new(Conflict(
            PluginManifest::new("cognition.conflict", "0.1.0").unwrap(),
        )))
        .unwrap();
    rig.kernel.start(&id("cognition.conflict")).await.unwrap();
    let executor = Arc::new(
        ControlGoalExecutor::new(rig.control.clone(), rig.runner.clone(), "internal").unwrap(),
    );
    let plugin = CognitionLoopPlugin::new(
        Arc::new(rig.admin.clone()),
        Arc::new(PriorityDrivePolicy),
        Arc::new(EchoReceiptVerifier),
        executor,
        options(),
        vec![],
    )
    .unwrap();
    let controller = plugin.controller();
    rig.kernel.register(Box::new(plugin)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        rig.kernel.start(&id(LOOP_PLUGIN_ID)),
    )
    .await
    .expect("启动回滚不应等待一个未启动的任务");
    assert!(result.is_err());
    assert!(controller.stats().is_err());
    assert_eq!(rig.admin.snapshot().unwrap(), before);
    assert_eq!(model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    rig.kernel.unregister(&id(LOOP_PLUGIN_ID)).unwrap();
    rig.close().await;
}
