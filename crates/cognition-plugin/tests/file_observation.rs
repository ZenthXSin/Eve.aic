use eve_cognition_api::*;
use eve_cognition_plugin::{
    CognitionController, CognitionPlugin, UserGoalFeedback, UserGoalFileObservation,
};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Store {
    memory: MemoryStateStore,
    fail: AtomicBool,
    attempts: AtomicUsize,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-file-storage-error".into()));
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
fn seed(admin: &CognitionController) {
    let mut state = CognitiveState::default();
    state.goals.insert(
        "goal".into(),
        Goal {
            id: "goal".into(),
            revision: 0,
            source: Source {
                kind: SourceKind::User,
                channel: "qq.goal".into(),
                reference: "source-goal".into(),
            },
            visibility: Visibility::User("alice".into()),
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
            wait_reason: Some("等待实际观察".into()),
            block_reason: None,
            execution: None,
            feedback: None,
        },
    );
    admin.replace(0, state).unwrap();
}
fn service(admin: Arc<dyn CognitionAdmin>, user: &str) -> UserGoalFileObservation {
    UserGoalFileObservation::new(admin, "eve".into(), user.into(), "qq.goal".into()).unwrap()
}
fn input(revision: u64, content: &str) -> FileObservationInput {
    let digest = ring::digest::digest(&ring::digest::SHA256, content.as_bytes());
    FileObservationInput {
        goal_id: "goal".into(),
        expected_goal_revision: revision,
        observation_source_id: format!("file-source:{}", "a".repeat(64)),
        sha256: digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        byte_count: content.len() as u64,
        text_excerpt: content.into(),
        text_truncated: false,
        observed_at_ms: 100,
    }
}

#[tokio::test]
async fn observation_is_atomic_environment_data_and_survives_feedback_and_restart() {
    let store = Arc::new(Store::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin);
    let before = admin.snapshot().unwrap();
    let observer = service(Arc::new(admin.clone()), "alice");
    let applied = observer.submit(input(1, "实际文件 📚")).unwrap();
    assert_eq!(
        (applied.goal_revision, applied.revision, applied.duplicate),
        (2, 2, false)
    );
    let after = admin.snapshot().unwrap();
    let mut unchanged = after.state.goals["goal"].clone();
    unchanged.revision = 1;
    unchanged.wait_reason = before.state.goals["goal"].wait_reason.clone();
    assert_eq!(unchanged, before.state.goals["goal"]);
    let event = &after.state.events[0];
    assert_eq!(event.kind, CognitiveEventKind::ExternalInput);
    assert_eq!(event.source.kind, SourceKind::Environment);
    assert_eq!(event.source.channel, FILE_OBSERVATION_CHANNEL);
    assert_eq!(event.source.reference, input(1, "").observation_source_id);
    let observation = FileObservation::parse(&event.summary).unwrap();
    assert_eq!(observation.text_excerpt, "实际文件 📚");
    assert_eq!(
        (
            observation.previous_goal_revision,
            observation.goal_revision
        ),
        (1, 2)
    );
    assert_eq!(
        after.state.goals["goal"].wait_reason.as_deref(),
        Some(event.summary.as_str())
    );
    assert!(!format!("{observation:?} {:?}", input(1, "实际文件 📚")).contains("实际"));
    let feedback = UserGoalFeedback::new(
        Arc::new(admin.clone()),
        "eve".into(),
        "alice".into(),
        "qq.goal".into(),
        "qq.goal-feedback".into(),
    )
    .unwrap();
    feedback
        .submit(GoalFeedbackInput {
            goal_id: "goal".into(),
            expected_goal_revision: 2,
            feedback_id: "user-correction".into(),
            text: "仍须等我确认".into(),
            at_ms: 101,
        })
        .unwrap();
    let corrected = admin.snapshot().unwrap();
    let attempts = store.attempts.load(Ordering::SeqCst);
    let mut repeated = input(1, "实际文件 📚");
    repeated.observed_at_ms = 102;
    let duplicate = observer.submit(repeated.clone()).unwrap();
    assert_eq!(
        (
            duplicate.goal_revision,
            duplicate.revision,
            duplicate.duplicate
        ),
        (2, 3, true)
    );
    assert_eq!(store.attempts.load(Ordering::SeqCst), attempts);
    assert_eq!(admin.snapshot().unwrap(), corrected);
    assert!(corrected.state.goals["goal"].feedback.is_none());
    assert_eq!(corrected.state.events[1].source.kind, SourceKind::User);
    assert_eq!(
        corrected.state.events[1].caused_by.as_deref(),
        Some(event.id.as_str())
    );
    kernel.stop_all().await.unwrap();
    let (restarted, admin) = open(store.clone()).await;
    assert!(
        service(Arc::new(admin.clone()), "alice")
            .submit(repeated)
            .unwrap()
            .duplicate
    );
    assert_eq!(admin.snapshot().unwrap(), corrected);
    assert_eq!(store.attempts.load(Ordering::SeqCst), attempts);
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
    restarted.stop_all().await.unwrap();
}

#[tokio::test]
async fn latest_observation_controls_dedup_and_old_content_never_bypasses_cas() {
    let (kernel, admin) = open(Arc::new(Store::default())).await;
    seed(&admin);
    let observer = service(Arc::new(admin.clone()), "alice");
    observer.submit(input(1, "first")).unwrap();
    observer.submit(input(2, "second")).unwrap();
    let baseline = admin.snapshot().unwrap();
    assert_eq!(
        observer.submit(input(1, "first")),
        Err(CognitionError::StaleRevision)
    );
    assert_eq!(admin.snapshot().unwrap(), baseline);
    let reapplied = observer.submit(input(3, "first")).unwrap();
    assert!(!reapplied.duplicate);
    assert_eq!(reapplied.goal_revision, 4);
    let mut other = input(4, "different-file");
    other.observation_source_id = format!("file-source:{}", "b".repeat(64));
    observer.submit(other).unwrap();
    let latest = admin.snapshot().unwrap();
    assert_eq!(
        observer.submit(input(1, "first")),
        Err(CognitionError::StaleRevision)
    );
    assert_eq!(admin.snapshot().unwrap(), latest);
    assert_eq!(latest.state.events.len(), 4);
    assert_ne!(latest.state.events[0].id, latest.state.events[2].id);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn rebinding_a_b_a_creates_new_context_and_same_latest_file_preserves_user_feedback() {
    let store = Arc::new(Store::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin);
    let observer = service(Arc::new(admin.clone()), "alice");
    let feedback = UserGoalFeedback::new(
        Arc::new(admin.clone()),
        "eve".into(),
        "alice".into(),
        "qq.goal".into(),
        "qq.goal-feedback".into(),
    )
    .unwrap();
    let first_a = observer.submit(input(1, "same file content")).unwrap();
    assert_eq!((first_a.goal_revision, first_a.duplicate), (2, false));
    let mut file_b = input(2, "same file content");
    file_b.observation_source_id = format!("file-source:{}", "b".repeat(64));
    let first_b = observer.submit(file_b.clone()).unwrap();
    assert_eq!((first_b.goal_revision, first_b.duplicate), (3, false));
    feedback
        .submit(GoalFeedbackInput {
            goal_id: "goal".into(),
            expected_goal_revision: 3,
            feedback_id: "feedback-after-b".into(),
            text: "B 文件还需用户确认".into(),
            at_ms: 101,
        })
        .unwrap();
    let corrected_b = admin.snapshot().unwrap();
    let attempts = store.attempts.load(Ordering::SeqCst);
    assert!(observer.submit(file_b).unwrap().duplicate);
    assert_eq!(store.attempts.load(Ordering::SeqCst), attempts);
    assert_eq!(admin.snapshot().unwrap(), corrected_b);
    assert_eq!(
        observer.submit(input(1, "same file content")),
        Err(CognitionError::StaleRevision)
    );
    assert_eq!(store.attempts.load(Ordering::SeqCst), attempts);
    let rebound_a = observer.submit(input(4, "same file content")).unwrap();
    assert_eq!((rebound_a.goal_revision, rebound_a.duplicate), (5, false));
    let rebound = admin.snapshot().unwrap();
    let latest = rebound.state.events.last().unwrap();
    let observation = FileObservation::parse(&latest.summary).unwrap();
    assert_eq!(
        observation.observation_source_id,
        input(1, "").observation_source_id
    );
    assert_eq!(
        (
            observation.previous_goal_revision,
            observation.goal_revision
        ),
        (4, 5)
    );
    assert_ne!(latest.id, rebound.state.events[0].id);
    assert_eq!(latest.caused_by.as_deref(), Some("feedback-after-b"));
    assert_eq!(
        rebound.state.goals["goal"].wait_reason.as_deref(),
        Some(latest.summary.as_str())
    );
    feedback
        .submit(GoalFeedbackInput {
            goal_id: "goal".into(),
            expected_goal_revision: 5,
            feedback_id: "feedback-after-rebound-a".into(),
            text: "重新绑定 A 后仍需用户确认".into(),
            at_ms: 102,
        })
        .unwrap();
    let corrected_a = admin.snapshot().unwrap();
    let attempts = store.attempts.load(Ordering::SeqCst);
    let duplicate = observer.submit(input(1, "same file content")).unwrap();
    assert_eq!(
        (
            duplicate.goal_revision,
            duplicate.revision,
            duplicate.duplicate
        ),
        (5, 6, true)
    );
    assert_eq!(store.attempts.load(Ordering::SeqCst), attempts);
    assert_eq!(admin.snapshot().unwrap(), corrected_a);
    assert_eq!(corrected_a.state.events.len(), 5);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn owner_subject_stale_terminal_expiry_and_capacity_reject_without_partial_write() {
    let store = Arc::new(Store::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin);
    let observer = service(Arc::new(admin.clone()), "alice");
    assert_eq!(
        service(Arc::new(admin.clone()), "bob").submit(input(1, "x")),
        Err(CognitionError::AccessDenied)
    );
    let other_subject = UserGoalFileObservation::new(
        Arc::new(admin.clone()),
        "other-eve".into(),
        "alice".into(),
        "qq.goal".into(),
    )
    .unwrap();
    assert_eq!(
        other_subject.submit(input(1, "x")),
        Err(CognitionError::SubjectMismatch)
    );
    assert_eq!(
        observer.submit(input(2, "x")),
        Err(CognitionError::StaleRevision)
    );
    assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
    let baseline = admin.snapshot().unwrap();
    let mut state = baseline.state;
    state.goals.get_mut("goal").unwrap().expires_at_ms = Some(100);
    let expired = admin.replace(baseline.revision, state).unwrap();
    assert_eq!(
        observer.submit(input(2, "x")),
        Err(CognitionError::InvalidTransition)
    );
    assert_eq!(admin.snapshot().unwrap(), expired);
    let mut state = expired.state;
    let goal = state.goals.get_mut("goal").unwrap();
    goal.status = GoalStatus::Cancelled;
    goal.wait_reason = None;
    let terminal = admin.replace(expired.revision, state).unwrap();
    assert_eq!(
        observer.submit(input(3, "x")),
        Err(CognitionError::InvalidTransition)
    );
    assert_eq!(admin.snapshot().unwrap(), terminal);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(Arc::new(Store::default())).await;
    seed(&admin);
    let baseline = admin.snapshot().unwrap();
    let mut state = baseline.state;
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
    let full = admin.replace(baseline.revision, state).unwrap();
    assert_eq!(
        service(Arc::new(admin.clone()), "alice").submit(input(1, "x")),
        Err(CognitionError::LimitReached)
    );
    assert_eq!(admin.snapshot().unwrap(), full);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_save_leaves_memory_unchanged_and_retry_can_commit_once() {
    let store = Arc::new(Store::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin);
    let observer = service(Arc::new(admin.clone()), "alice");
    let before = admin.snapshot().unwrap();
    store.fail.store(true, Ordering::SeqCst);
    let failure = observer.submit(input(1, "private content"));
    assert_eq!(failure, Err(CognitionError::Storage));
    assert!(!format!("{failure:?}").contains("private"));
    assert_eq!(admin.snapshot().unwrap(), before);
    store.fail.store(false, Ordering::SeqCst);
    assert!(
        !observer
            .submit(input(1, "private content"))
            .unwrap()
            .duplicate
    );
    assert_eq!(admin.snapshot().unwrap().state.events.len(), 1);
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
async fn concurrent_same_observation_has_one_cas_winner_without_automatic_retry() {
    let (kernel, admin) = open(Arc::new(Store::default())).await;
    seed(&admin);
    let synchronized = Arc::new(SynchronizedAdmin {
        admin: admin.clone(),
        barrier: Barrier::new(2),
    });
    let left = service(synchronized.clone(), "alice");
    let right = service(synchronized, "alice");
    let left = std::thread::spawn(move || left.submit(input(1, "same file")));
    let right = std::thread::spawn(move || right.submit(input(1, "same file")));
    let results = [left.join().unwrap(), right.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CognitionError::StaleRevision)))
            .count(),
        1
    );
    let saved = admin.snapshot().unwrap();
    assert_eq!(
        (
            saved.revision,
            saved.state.goals["goal"].revision,
            saved.state.events.len()
        ),
        (2, 2, 1)
    );
    kernel.stop_all().await.unwrap();
}
