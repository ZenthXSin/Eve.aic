use eve_cognition_api::*;
use eve_cognition_plugin::{CognitionController, CognitionPlugin, UserGoalFeedback};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct FaultStore {
    memory: MemoryStateStore,
    fail: AtomicBool,
}
impl StateStore for FaultStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-error".into()));
        }
        self.memory.set(namespace, key, value)
    }
}

async fn open(store: Arc<dyn StateStore>) -> (Kernel, CognitionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}
fn waiting(id: &str, user: &str) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: "qq.goal".into(),
            reference: format!("source-{id}"),
        },
        visibility: Visibility::User(user.into()),
        description: "保留原始目标。".into(),
        verification: "user-goal:v1".into(),
        priority: 50,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 0,
            max_attempts: 1,
            timeout_ms: 1000,
        },
        stop_condition: "user-confirmation".into(),
        expires_at_ms: None,
        status: GoalStatus::Waiting,
        wait_reason: Some("等待用户事实".into()),
        block_reason: None,
        execution: None,
        feedback: None,
    }
}
fn seed(admin: &CognitionController) {
    let mut state = CognitiveState::default();
    state.goals.insert("goal".into(), waiting("goal", "alice"));
    state.goals.insert("other".into(), waiting("other", "bob"));
    admin.replace(0, state).unwrap();
}
fn service(admin: Arc<dyn CognitionAdmin>, user: &str) -> UserGoalFeedback {
    UserGoalFeedback::new(
        admin,
        "eve".into(),
        user.into(),
        "qq.goal".into(),
        "qq.goal-feedback".into(),
    )
    .unwrap()
}
fn input(id: &str, revision: u64) -> GoalFeedbackInput {
    GoalFeedbackInput {
        goal_id: "goal".into(),
        expected_goal_revision: revision,
        feedback_id: id.into(),
        text: "实际只允许一页 📚。".into(),
        at_ms: 100,
    }
}

#[tokio::test]
async fn feedback_atomically_preserves_goal_budget_and_recovers_duplicate_without_new_revision() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin);
    let before = admin.snapshot().unwrap();
    let feedback = service(Arc::new(admin.clone()), "alice");
    let applied = feedback.submit(input("feedback-a", 1)).unwrap();
    assert_eq!(
        (applied.goal_revision, applied.revision, applied.duplicate),
        (2, 2, false)
    );
    let after = admin.snapshot().unwrap();
    let mut unchanged = after.state.goals["goal"].clone();
    unchanged.revision = 1;
    unchanged.wait_reason = before.state.goals["goal"].wait_reason.clone();
    assert_eq!(unchanged, before.state.goals["goal"]);
    assert!(after.state.goals["goal"].feedback.is_none());
    let event = &after.state.events[0];
    let payload = GoalUserFeedback::parse(&event.summary).unwrap();
    assert_eq!(payload.text, "实际只允许一页 📚。");
    assert_eq!(payload.previous_goal_revision, 1);
    assert_eq!(payload.goal_revision, 2);
    assert_eq!(
        after.state.goals["goal"].wait_reason.as_deref(),
        Some(event.summary.as_str())
    );
    assert_eq!(event.kind, CognitiveEventKind::ExternalInput);
    assert_eq!(event.source.kind, SourceKind::User);
    assert_eq!(event.source.channel, "qq.goal-feedback");
    assert_eq!(event.source.reference, "feedback-a");
    let mut repeated = input("feedback-a", 1);
    repeated.at_ms = 999;
    repeated.expected_goal_revision = after.state.goals["goal"].revision;
    assert!(feedback.submit(repeated.clone()).unwrap().duplicate);
    assert_eq!(admin.snapshot().unwrap(), after);
    kernel.stop_all().await.unwrap();
    let (restarted, admin) = open(store).await;
    assert!(
        service(Arc::new(admin.clone()), "alice")
            .submit(repeated)
            .unwrap()
            .duplicate
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    assert!(
        admin
            .reader(ReadAccess::User("bob".into()))
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .events
            .is_empty()
    );
    assert!(!format!("{:?} {:?}", input("feedback-a", 1), payload).contains("实际"));
    restarted.stop_all().await.unwrap();
}

#[tokio::test]
async fn conflicts_cross_user_stale_terminal_expired_and_invalid_payload_never_write() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin);
    let feedback = service(Arc::new(admin.clone()), "alice");
    feedback.submit(input("feedback-a", 1)).unwrap();
    let baseline = admin.snapshot().unwrap();
    let mut changed = input("feedback-a", 1);
    changed.text = "不同正文".into();
    assert_eq!(feedback.submit(changed), Err(CognitionError::InvalidInput));
    assert_eq!(
        service(Arc::new(admin.clone()), "bob").submit(input("feedback-b", 2)),
        Err(CognitionError::AccessDenied)
    );
    assert_eq!(
        feedback.submit(input("feedback-b", 1)),
        Err(CognitionError::StaleRevision)
    );
    let mut too_large = input("feedback-b", 2);
    too_large.text = "x".repeat(MAX_GOAL_FEEDBACK_BYTES + 1);
    assert_eq!(
        feedback.submit(too_large),
        Err(CognitionError::InvalidInput)
    );
    let mut escaped = input("feedback-b", 2);
    escaped.text = "\u{0001}".repeat(MAX_GOAL_FEEDBACK_BYTES);
    assert_eq!(feedback.submit(escaped), Err(CognitionError::InvalidInput));
    assert_eq!(admin.snapshot().unwrap(), baseline);
    let mut state = baseline.state;
    state.goals.get_mut("goal").unwrap().expires_at_ms = Some(100);
    let expired = admin.replace(baseline.revision, state).unwrap();
    assert_eq!(
        feedback.submit(input("feedback-b", 3)),
        Err(CognitionError::InvalidTransition)
    );
    assert_eq!(admin.snapshot().unwrap(), expired);
    let mut state = expired.state;
    let goal = state.goals.get_mut("goal").unwrap();
    goal.status = GoalStatus::Cancelled;
    goal.wait_reason = None;
    let terminal = admin.replace(expired.revision, state).unwrap();
    assert_eq!(
        feedback.submit(input("feedback-b", 4)),
        Err(CognitionError::InvalidTransition)
    );
    assert!(feedback.submit(input("feedback-a", 1)).unwrap().duplicate);
    assert_eq!(admin.snapshot().unwrap(), terminal);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_event_log_and_storage_failure_do_not_publish_partial_feedback() {
    let store = Arc::new(FaultStore::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin);
    let feedback = service(Arc::new(admin.clone()), "alice");
    let before = admin.snapshot().unwrap();
    store.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        feedback.submit(input("feedback-a", 1)),
        Err(CognitionError::Storage)
    );
    assert_eq!(admin.snapshot().unwrap(), before);
    store.fail.store(false, Ordering::SeqCst);
    let mut state = before.state;
    for index in 0..MAX_RECORDS {
        state.events.push(CognitiveEvent {
            id: format!("event-{index}"),
            kind: CognitiveEventKind::ExternalInput,
            source: Source {
                kind: SourceKind::User,
                channel: "qq.goal".into(),
                reference: format!("source-{index}"),
            },
            visibility: Visibility::User("alice".into()),
            goal_id: Some("goal".into()),
            caused_by: None,
            at_ms: 1,
            summary: "已有事件".into(),
        });
    }
    let full = admin.replace(before.revision, state).unwrap();
    assert_eq!(
        feedback.submit(input("feedback-a", 1)),
        Err(CognitionError::LimitReached)
    );
    assert_eq!(admin.snapshot().unwrap(), full);
    kernel.stop_all().await.unwrap();
}

struct SynchronizedAdmin {
    admin: CognitionController,
    barrier: Barrier,
}
impl CognitionAdmin for SynchronizedAdmin {
    fn snapshot(&self) -> CognitionResult<CognitiveSnapshot> {
        let snapshot = self.admin.snapshot()?;
        self.barrier.wait();
        Ok(snapshot)
    }
    fn reader(&self, access: ReadAccess) -> CognitionResult<Arc<dyn CognitionReader>> {
        self.admin.reader(access)
    }
    fn replace(&self, revision: u64, state: CognitiveState) -> CognitionResult<CognitiveSnapshot> {
        self.admin.replace(revision, state)
    }
}
#[tokio::test]
async fn concurrent_feedback_keeps_one_atomic_winner_and_does_not_retry_the_loser() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin);
    let scoped = Arc::new(SynchronizedAdmin {
        admin: admin.clone(),
        barrier: Barrier::new(2),
    });
    let left = service(scoped.clone(), "alice");
    let right = service(scoped, "alice");
    let left = std::thread::spawn(move || left.submit(input("left", 1)));
    let right = std::thread::spawn(move || right.submit(input("right", 1)));
    let results = [left.join().unwrap(), right.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CognitionError::StaleRevision)))
            .count(),
        1
    );
    let state = admin.snapshot().unwrap();
    assert_eq!(state.revision, 2);
    assert_eq!(state.state.goals["goal"].revision, 2);
    assert_eq!(state.state.events.len(), 1);
    kernel.stop_all().await.unwrap();
}
