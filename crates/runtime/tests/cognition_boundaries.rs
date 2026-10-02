#[path = "../examples/support/cognition.rs"]
mod support;

use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{
    CognitionLoopPlugin, EchoReceiptVerifier, LoopController, PriorityDrivePolicy,
};
use eve_cognition_plugin::COGNITION_STATE_KEY;
use eve_control_api::{CommitState, GenerationKey};
use eve_kernel::backends::MemoryStateStore;
use eve_llm_api::*;
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_runtime::ControlGoalExecutor;
use eve_session_api::SESSION_PLUGIN_ID;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use support::*;
use tokio::sync::Notify;

#[derive(Default)]
struct ExecutingStore {
    memory: MemoryStateStore,
    paused: AtomicBool,
    entered: Notify,
    executing: Mutex<Option<CognitiveSnapshot>>,
    entered_at: Mutex<Option<Instant>>,
    released: Mutex<bool>,
    release: Condvar,
}
impl ExecutingStore {
    fn paused() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(true),
            ..Self::default()
        })
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }

    async fn wait_executing(&self) -> CognitiveSnapshot {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("应抵达 Executing 持久化屏障");
        self.executing.lock().unwrap().clone().unwrap()
    }

    fn persisted(&self) -> CognitiveSnapshot {
        let bytes = self
            .memory
            .get(&id(COGNITION_PLUGIN_ID), COGNITION_STATE_KEY)
            .unwrap()
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
impl StateStore for ExecutingStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }

    fn set(&self, namespace: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        if namespace.as_str() == COGNITION_PLUGIN_ID && key == COGNITION_STATE_KEY {
            let snapshot: CognitiveSnapshot = serde_json::from_slice(&bytes).unwrap();
            snapshot.state.validate().expect("每次持久化状态均须合法");
            if snapshot
                .state
                .goals
                .values()
                .any(|goal| goal.status == GoalStatus::Executing)
                && self.paused.swap(false, Ordering::SeqCst)
            {
                *self.executing.lock().unwrap() = Some(snapshot);
                *self.entered_at.lock().unwrap() = Some(Instant::now());
                self.entered.notify_one();
                let released = self.released.lock().unwrap();
                let (released, timeout) = self
                    .release
                    .wait_timeout_while(released, Duration::from_secs(5), |value| !*value)
                    .unwrap();
                if timeout.timed_out() && !*released {
                    return Err(PluginError::State("Executing 屏障等待超时".into()));
                }
            }
        }
        self.memory.set(namespace, key, bytes)
    }
}

#[derive(Default)]
struct Model {
    requests: AtomicUsize,
}
impl LlmProvider for Model {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == ChatRole::Tool)
                {
                    ModelResponse::Final {
                        text: "已完成".into(),
                    }
                } else {
                    call_response(1)
                },
            )
        })
    }
}

struct CountingExecutor {
    inner: ControlGoalExecutor,
    submissions: AtomicUsize,
    deadline: Mutex<Option<Instant>>,
}
impl GoalExecutor for CountingExecutor {
    fn submit(&self, goal: &Goal, attempt: &ExecutionAttempt) -> LoopResult<GenerationKey> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        self.inner.submit(goal, attempt)
    }

    fn submit_before(
        &self,
        goal: &Goal,
        attempt: &ExecutionAttempt,
        deadline: Instant,
    ) -> LoopResult<GenerationKey> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        *self.deadline.lock().unwrap() = Some(deadline);
        self.inner.submit_before(goal, attempt, deadline)
    }

    fn cancel(&self, key: &GenerationKey) -> LoopResult<()> {
        self.inner.cancel(key)
    }

    fn wait(&self, key: &GenerationKey) -> LoopFuture<'static, GoalExecutionReport> {
        self.inner.wait(key)
    }
}

async fn start_counted_loop(rig: &Rig) -> (LoopController, Arc<CountingExecutor>) {
    let executor = Arc::new(CountingExecutor {
        inner: ControlGoalExecutor::new(rig.control.clone(), rig.runner.clone(), "internal")
            .unwrap(),
        submissions: AtomicUsize::new(0),
        deadline: Mutex::new(None),
    });
    let plugin = CognitionLoopPlugin::new(
        Arc::new(rig.admin.clone()),
        Arc::new(PriorityDrivePolicy),
        Arc::new(EchoReceiptVerifier),
        executor.clone(),
        options(),
        vec![],
    )
    .unwrap();
    let controller = plugin.controller();
    rig.kernel.register(Box::new(plugin)).unwrap();
    rig.kernel.start(&id(LOOP_PLUGIN_ID)).await.unwrap();
    (controller, executor)
}

async fn wait_idle(controller: &LoopController) {
    let initial = controller.stats().unwrap().evaluations;
    tokio::time::timeout(Duration::from_secs(5), async {
        while controller.stats().unwrap().evaluations < initial + 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("反馈后循环应继续评估，而非因容量或期限错误停止");
}

async fn exhausted_while_saving(absolute_expiry: bool, stop: bool) {
    let store = ExecutingStore::paused();
    let model = Arc::new(Model::default());
    let rig = Rig::open(store.clone(), model.clone(), None).await;
    let mut target = goal("target");
    if absolute_expiry {
        target.expires_at_ms = Some(now_ms() + 500);
    } else {
        target.budget.timeout_ms = 150;
    }
    rig.seed(vec![target]);
    let (controller, executor) = start_counted_loop(&rig).await;
    let marker = store.wait_executing().await;
    let target = &marker.state.goals["target"];
    assert_eq!(target.status, GoalStatus::Executing);
    assert_eq!(executor.submissions.load(Ordering::SeqCst), 0);
    let deadline = if absolute_expiry {
        target.expires_at_ms.unwrap()
    } else {
        target.execution.as_ref().unwrap().started_at_ms + target.budget.timeout_ms
    };
    while now_ms() <= deadline {
        tokio::time::sleep(Duration::from_millis(deadline.saturating_sub(now_ms()) + 1)).await;
    }
    let shutdown = stop.then(|| controller.shutdown());
    store.release();
    if let Some(shutdown) = shutdown {
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("停止必须等待 Executing 保存和未准入反馈收尾")
            .unwrap();
    } else {
        rig.wait_terminal("target").await;
        wait_idle(&controller).await;
        controller.shutdown().await.unwrap();
    }

    let snapshot = rig.admin.snapshot().unwrap();
    assert_eq!(snapshot, store.persisted());
    snapshot.state.validate().unwrap();
    assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Cancelled);
    let feedback = snapshot.state.goals["target"].feedback.as_ref().unwrap();
    assert_eq!(feedback.commit, ExecutionCommit::NotStarted);
    assert_eq!(feedback.started_tools, Some(0));
    assert!(!feedback.verification_met);
    assert_eq!(
        executor.submissions.load(Ordering::SeqCst),
        0,
        "过期后不得调用执行器"
    );
    assert_eq!(model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    let stats = controller.stats().unwrap();
    assert_eq!(
        (
            stats.submitted,
            stats.model_requests,
            stats.admitted_tool_calls,
            stats.started_tools
        ),
        (0, 0, 0, 0)
    );
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

    let resumed_model = Arc::new(Model::default());
    let resumed = Rig::open(store, resumed_model.clone(), None).await;
    let controller = resumed.start_loop(options()).await;
    wait_idle(&controller).await;
    controller.shutdown().await.unwrap();
    assert_eq!(resumed.admin.snapshot().unwrap(), snapshot);
    assert_eq!(resumed_model.requests.load(Ordering::SeqCst), 0);
    assert_eq!(resumed.probe.started.load(Ordering::SeqCst), 0);
    resumed.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executing_save_crosses_total_timeout_without_submitting() {
    exhausted_while_saving(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executing_save_crosses_goal_expiry_without_submitting() {
    exhausted_while_saving(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_while_executing_save_crosses_timeout_preserves_not_started_feedback() {
    exhausted_while_saving(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_execution_keeps_deadline_from_before_executing_save() {
    let store = ExecutingStore::paused();
    let model = Arc::new(Model::default());
    let rig = Rig::open(store.clone(), model.clone(), None).await;
    let mut target = goal("target");
    target.budget.timeout_ms = 2_000;
    rig.seed(vec![target]);
    let (controller, executor) = start_counted_loop(&rig).await;
    store.wait_executing().await;
    let entered_at = store.entered_at.lock().unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    store.release();
    rig.wait_terminal("target").await;
    controller.shutdown().await.unwrap();

    let deadline = executor.deadline.lock().unwrap().unwrap();
    assert!(
        deadline <= entered_at + Duration::from_secs(2),
        "Executing 保存不得重置执行总期限"
    );
    let snapshot = rig.admin.snapshot().unwrap();
    assert_eq!(snapshot.state.goals["target"].status, GoalStatus::Completed);
    assert_eq!(model.requests.load(Ordering::SeqCst), 2);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot, store.persisted());
    rig.close().await;
}

#[derive(Default)]
struct PausedModel {
    requests: AtomicUsize,
    entered: Notify,
    release: Notify,
}
impl LlmProvider for PausedModel {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if request
                .messages
                .iter()
                .any(|message| message.role == ChatRole::Tool)
            {
                return Ok(ModelResponse::Final {
                    text: "已完成".into(),
                });
            }
            self.entered.notify_one();
            self.release.notified().await;
            Ok(call_response(1))
        })
    }
}

#[derive(Clone, Copy)]
enum AdmissionDeadline {
    Monotonic,
    AttemptTimeout,
    GoalExpiry,
}

async fn host_uses_remaining_deadline(mode: AdmissionDeadline) {
    let model = Arc::new(PausedModel::default());
    let rig = Rig::open(Arc::new(MemoryStateStore::default()), model.clone(), None).await;
    let executor =
        ControlGoalExecutor::new(rig.control.clone(), rig.runner.clone(), "internal").unwrap();
    let mut target = goal("target");
    let now = now_ms();
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut attempt = ExecutionAttempt {
        attempt_id: "direct-attempt".into(),
        session_id: "direct-session".into(),
        task_id: "direct-task".into(),
        turn_id: None,
        started_at_ms: now,
    };
    match mode {
        AdmissionDeadline::Monotonic => {}
        AdmissionDeadline::AttemptTimeout => {
            attempt.started_at_ms -= target.budget.timeout_ms - 400;
        }
        AdmissionDeadline::GoalExpiry => target.expires_at_ms = Some(now + 400),
    }
    let key = match mode {
        AdmissionDeadline::Monotonic => executor.submit_before(&target, &attempt, deadline),
        AdmissionDeadline::AttemptTimeout | AdmissionDeadline::GoalExpiry => {
            executor.submit(&target, &attempt)
        }
    }
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    tokio::time::sleep_until((deadline + Duration::from_millis(10)).into()).await;
    model.release.notify_one();
    // 此处没有外层认知循环定时取消；必须由宿主预算拒绝已过期的工具准入。
    let report = tokio::time::timeout(Duration::from_secs(3), executor.wait(&key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.control.run.commit, CommitState::Failed);
    assert!(!report.control.cancel_requested);
    assert_eq!(report.usage.model_requests, 1);
    assert_eq!(report.usage.admitted_tool_calls, 0);
    assert_eq!(report.control.run.started_tools, Some(0));
    assert_eq!(model.requests.load(Ordering::SeqCst), 1);
    assert_eq!(rig.probe.started.load(Ordering::SeqCst), 0);
    rig.close().await;
}

#[tokio::test]
async fn control_executor_preserves_monotonic_deadline_in_host_budget() {
    host_uses_remaining_deadline(AdmissionDeadline::Monotonic).await;
}

#[tokio::test]
async fn control_executor_submit_preserves_elapsed_attempt_budget() {
    host_uses_remaining_deadline(AdmissionDeadline::AttemptTimeout).await;
}

#[tokio::test]
async fn control_executor_submit_honors_earlier_goal_expiry() {
    host_uses_remaining_deadline(AdmissionDeadline::GoalExpiry).await;
}

#[tokio::test]
async fn full_ready_goal_set_preserves_custom_drives_and_keeps_evaluating() {
    for custom_count in [1, MAX_RECORDS] {
        let store = Arc::new(ExecutingStore::default());
        let model = Arc::new(Model::default());
        let rig = Rig::open(store.clone(), model.clone(), None).await;
        let mut goals: Vec<_> = (0..MAX_RECORDS)
            .map(|index| goal(&format!("goal-{index:03}")))
            .collect();
        goals[0].priority = 100;
        rig.seed(goals);
        let snapshot = rig.admin.snapshot().unwrap();
        let mut state = snapshot.state;
        for index in 0..custom_count {
            let id = format!("host.drive.{index}");
            state.drives.insert(
                id.clone(),
                Drive {
                    id,
                    visibility: Visibility::Internal,
                    goal_ids: vec!["goal-255".into()],
                    strength: 40,
                    reason: "宿主自定义驱动必须保留".into(),
                    evaluated_at_ms: now_ms(),
                    valid_until_ms: now_ms() + 60_000,
                },
            );
        }
        let custom_drives = state.drives.clone();
        state.validate().unwrap();
        rig.admin.replace(snapshot.revision, state).unwrap();
        let (controller, executor) = start_counted_loop(&rig).await;
        rig.wait_terminal("goal-000").await;
        wait_idle(&controller).await;
        controller.shutdown().await.unwrap();

        let snapshot = rig.admin.snapshot().unwrap();
        snapshot.state.validate().unwrap();
        assert_eq!(snapshot, store.persisted());
        assert_eq!(snapshot.state.goals.len(), MAX_RECORDS);
        assert!(snapshot.state.drives.len() <= MAX_RECORDS);
        assert_eq!(
            snapshot.state.goals["goal-000"].status,
            GoalStatus::Completed
        );
        assert_eq!(
            snapshot
                .state
                .goals
                .values()
                .filter(|goal| goal.status == GoalStatus::Ready)
                .count(),
            MAX_RECORDS - 1
        );
        for (id, drive) in custom_drives {
            assert_eq!(snapshot.state.drives.get(&id), Some(&drive));
        }
        assert_eq!(executor.submissions.load(Ordering::SeqCst), 1);
        assert_eq!(model.requests.load(Ordering::SeqCst), 2);
        assert_eq!(rig.probe.started.load(Ordering::SeqCst), 1);
        rig.close().await;
    }
}
