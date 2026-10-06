use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{EndogenousPlanner, ReflectionPlannerFactory, current_reflection};
use eve_cognition_plugin::{
    COGNITION_STATE_KEY, CognitionController, CognitionPlugin, UserGoalFeedback,
};
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
            return Err(PluginError::State("injected-write-failure".into()));
        }
        self.memory.set(namespace, key, value)
    }
}
fn owner() -> PluginId {
    PluginId::new(COGNITION_PLUGIN_ID).unwrap()
}
async fn open(store: Arc<dyn StateStore>) -> (Kernel, CognitionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&owner()).await.unwrap();
    (kernel, admin)
}
fn parent(id: &str, user: &str) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: "cognition.cli".into(),
            reference: id.into(),
        },
        visibility: Visibility::User(user.into()),
        description: "尚未解决的具体事项".into(),
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
        wait_reason: Some("需要整理下一步".into()),
        block_reason: None,
        execution: None,
        feedback: None,
    }
}
fn options(access: ReadAccess) -> EndogenousOptions {
    EndogenousOptions {
        scope: ExecutionScope {
            subject_id: "eve".into(),
            access,
            sources: vec![AllowedSource {
                kind: SourceKind::User,
                channel: "cognition.cli".into(),
            }],
        },
        max_derivations: 1,
        timeout_ms: 30_000,
    }
}
fn seed(admin: &CognitionController, goals: Vec<Goal>) {
    let snapshot = admin.snapshot().unwrap();
    let mut state = snapshot.state;
    for goal in goals {
        state.goals.insert(goal.id.clone(), goal);
    }
    admin.replace(snapshot.revision, state).unwrap();
}
fn planner(admin: &CognitionController, access: ReadAccess) -> EndogenousPlanner {
    EndogenousPlanner::new(Arc::new(admin.clone()), options(access)).unwrap()
}

#[tokio::test]
async fn derives_once_with_private_provenance_and_frozen_budget() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let mut goal = parent("waiting", "alice");
    goal.expires_at_ms = Some(5000);
    seed(&admin, vec![goal]);
    let before = admin.snapshot().unwrap();
    let public_planner = ReflectionPlannerFactory
        .create(
            Arc::new(admin.clone()),
            options(ReadAccess::User("alice".into())),
        )
        .unwrap();
    let result = public_planner.reconcile(1000).unwrap();
    assert_eq!(result.created_goal_ids.len(), 1);
    let after = admin.snapshot().unwrap();
    let child = &after.state.goals[&result.created_goal_ids[0]];
    assert_eq!(child.status, GoalStatus::Ready);
    assert_eq!(child.source.kind, SourceKind::Inference);
    assert_eq!(child.source.channel, "endogenous");
    assert_eq!(child.source.reference, "waiting");
    assert_eq!(child.visibility, Visibility::User("alice".into()));
    assert_eq!(child.expires_at_ms, Some(5000));
    assert_eq!(child.budget.max_model_requests, 1);
    assert_eq!(child.budget.max_tool_calls, 0);
    assert_eq!(child.budget.max_attempts, 1);
    assert!(child.budget.timeout_ms <= 1000);
    assert_eq!(after.state.goals["waiting"], before.state.goals["waiting"]);
    assert_eq!(after.state.events.len(), before.state.events.len() + 2);
    let bob = admin
        .reader(ReadAccess::User("bob".into()))
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(bob.state.goals.is_empty());
    assert!(bob.state.events.is_empty());
    assert!(bob.state.drives.is_empty());
    // A fresh planner cannot manufacture another child for the same saved parent revision.
    assert!(
        planner(&admin, ReadAccess::Internal)
            .reconcile(1001)
            .unwrap()
            .created_goal_ids
            .is_empty()
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn ineligible_or_untrusted_sources_do_not_write() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let mut expired = parent("expired", "alice");
    expired.expires_at_ms = Some(1000);
    let mut wrong_source = parent("other-source", "alice");
    wrong_source.source.channel = "untrusted".into();
    let mut ready = parent("ready", "alice");
    ready.status = GoalStatus::Ready;
    ready.wait_reason = None;
    seed(
        &admin,
        vec![expired, wrong_source, ready, parent("bob-private", "bob")],
    );
    let before = admin.snapshot().unwrap();
    let result = planner(&admin, ReadAccess::User("alice".into()))
        .reconcile(1000)
        .unwrap();
    assert!(result.created_goal_ids.is_empty());
    assert_eq!(admin.snapshot().unwrap(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_atomic_save_does_not_publish_child_or_consume_limit() {
    let store = Arc::new(FaultStore::default());
    let (kernel, admin) = open(store.clone()).await;
    seed(&admin, vec![parent("pending", "alice")]);
    let before = admin.snapshot().unwrap();
    let bytes = store.get(&owner(), COGNITION_STATE_KEY).unwrap();
    let engine = planner(&admin, ReadAccess::Internal);
    store.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        engine.reconcile(1000),
        Err(LoopError::Cognition(CognitionError::Storage))
    );
    assert_eq!(admin.snapshot().unwrap(), before);
    assert_eq!(store.get(&owner(), COGNITION_STATE_KEY).unwrap(), bytes);
    store.fail.store(false, Ordering::SeqCst);
    assert_eq!(engine.reconcile(1001).unwrap().created_goal_ids.len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_record_capacity_keeps_host_records_and_does_not_partially_derive() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin, vec![parent("pending", "alice")]);
    let snapshot = admin.snapshot().unwrap();
    let mut state = snapshot.state;
    for index in 0..MAX_RECORDS {
        let id = format!("host-drive-{index}");
        state.drives.insert(
            id.clone(),
            Drive {
                id,
                visibility: Visibility::Internal,
                goal_ids: vec!["pending".into()],
                strength: 25,
                reason: "保留宿主驱动".into(),
                evaluated_at_ms: 1,
                valid_until_ms: 5000,
            },
        );
    }
    admin.replace(snapshot.revision, state).unwrap();
    let before = admin.snapshot().unwrap();
    let result = planner(&admin, ReadAccess::Internal).reconcile(1000);
    assert!(result.is_err());
    assert_eq!(admin.snapshot().unwrap(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn concurrent_planners_persist_one_derivation_for_one_parent_revision() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin, vec![parent("one", "alice")]);
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let engine = planner(&admin, ReadAccess::Internal);
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            engine.reconcile(1000)
        }));
    }
    barrier.wait();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    let created: usize = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|r| r.created_goal_ids.len())
        .sum();
    assert_eq!(created, 1);
    assert!(
        results
            .iter()
            .all(|r| r.is_ok() || *r == Err(LoopError::Cognition(CognitionError::StaleRevision)))
    );
    assert_eq!(admin.snapshot().unwrap().state.goals.len(), 2);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn new_parent_revision_has_distinct_child_and_limit_is_per_start() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin, vec![parent("a", "alice"), parent("b", "alice")]);
    let engine = planner(&admin, ReadAccess::Internal);
    let first = engine.reconcile(1000).unwrap().created_goal_ids;
    assert_eq!(first.len(), 1);
    assert!(engine.reconcile(1001).unwrap().created_goal_ids.is_empty());
    let snapshot = admin.snapshot().unwrap();
    let mut state = snapshot.state;
    state.goals.get_mut("a").unwrap().description = "用户更新了未解决事项".into();
    admin.replace(snapshot.revision, state).unwrap();
    let second = planner(&admin, ReadAccess::Internal)
        .reconcile(1002)
        .unwrap()
        .created_goal_ids;
    assert_eq!(second.len(), 1);
    assert_ne!(first, second);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn parent_cancellation_invalidates_ready_child_before_execution_without_replay() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin, vec![parent("one", "alice")]);
    let engine = planner(&admin, ReadAccess::Internal);
    let child_id = engine.reconcile(1000).unwrap().created_goal_ids.remove(0);
    let snapshot = admin.snapshot().unwrap();
    let mut state = snapshot.state;
    let target = state.goals.get_mut("one").unwrap();
    target.status = GoalStatus::Cancelled;
    target.wait_reason = None;
    admin.replace(snapshot.revision, state).unwrap();
    let report = engine.reconcile(1001).unwrap();
    assert_eq!(report.invalidated_goal_ids, vec![child_id.clone()]);
    assert!(report.created_goal_ids.is_empty());
    let after = admin.snapshot().unwrap();
    assert_eq!(after.state.goals[&child_id].status, GoalStatus::Cancelled);
    assert!(after.state.goals[&child_id].execution.is_none());
    assert_eq!(after.state.goals["one"].status, GoalStatus::Cancelled);
    assert!(
        planner(&admin, ReadAccess::Internal)
            .reconcile(1002)
            .unwrap()
            .created_goal_ids
            .is_empty()
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn expired_parent_invalidates_only_its_unstarted_child() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let mut goal = parent("expiring", "alice");
    goal.expires_at_ms = Some(1200);
    seed(&admin, vec![goal]);
    let engine = planner(&admin, ReadAccess::Internal);
    let child_id = engine.reconcile(1000).unwrap().created_goal_ids.remove(0);
    let report = engine.reconcile(1200).unwrap();
    assert_eq!(report.invalidated_goal_ids, vec![child_id.clone()]);
    assert_eq!(
        admin.snapshot().unwrap().state.goals[&child_id].status,
        GoalStatus::Cancelled
    );
    assert_eq!(
        admin.snapshot().unwrap().state.goals["expiring"].status,
        GoalStatus::Waiting
    );
    kernel.stop_all().await.unwrap();
}

fn feedback(admin: &CognitionController, id: &str, revision: u64, text: &str) {
    UserGoalFeedback::new(
        Arc::new(admin.clone()),
        "eve".into(),
        "alice".into(),
        "cognition.cli".into(),
        "cognition.cli.feedback".into(),
    )
    .unwrap()
    .submit(GoalFeedbackInput {
        goal_id: id.into(),
        expected_goal_revision: revision,
        feedback_id: format!("feedback-{revision}"),
        text: text.into(),
        at_ms: 1010 + revision,
    })
    .unwrap();
}

#[tokio::test]
async fn public_current_reflection_rejects_partial_or_forged_provenance() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin, vec![parent("one", "alice")]);
    let id = planner(&admin, ReadAccess::Internal)
        .reconcile(1000)
        .unwrap()
        .created_goal_ids
        .remove(0);
    let state = admin.snapshot().unwrap().state;
    let parent = &state.goals["one"];
    assert_eq!(
        current_reflection(&state, "eve", parent)
            .unwrap()
            .unwrap()
            .id,
        id
    );
    assert!(
        current_reflection(&state, "another-subject", parent)
            .unwrap()
            .is_none()
    );
    let mut stale = parent.clone();
    stale.revision += 1;
    assert!(current_reflection(&state, "eve", &stale).is_err());
    let input = state
        .events
        .iter()
        .position(|event| event.id.starts_with("eve.reflection.input."))
        .unwrap();
    let created = state
        .events
        .iter()
        .position(|event| event.id.starts_with("eve.reflection.created."))
        .unwrap();
    for variant in 0..12 {
        let mut broken = state.clone();
        match variant {
            0 => {
                broken.goals.remove(&id);
            }
            1 => {
                broken.events.remove(input);
            }
            2 => {
                broken.events.remove(created);
            }
            3 => broken.events[input].source.kind = SourceKind::User,
            4 => {
                broken.events[input].summary = broken.events[input].summary.replace("false", "true")
            }
            5 => broken.events[created].caused_by = Some("wrong-input".into()),
            6 => broken.events[created].kind = CognitiveEventKind::ExternalInput,
            7 => broken.goals.get_mut(&id).unwrap().budget.max_tool_calls = 1,
            8 => broken.events[created].visibility = Visibility::Public,
            9 => broken.events.swap(input, created),
            10 => broken.events.push(broken.events[input].clone()),
            11 => broken.goals.get_mut(&id).unwrap().source.reference = "other-parent".into(),
            _ => unreachable!(),
        }
        assert!(
            current_reflection(&broken, "eve", &broken.goals["one"]).is_err(),
            "variant {variant}"
        );
    }
    let user = admin
        .reader(ReadAccess::User("alice".into()))
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(
        current_reflection(&user.state, "eve", &user.state.goals["one"])
            .unwrap()
            .unwrap()
            .id,
        id
    );
    let bob = admin
        .reader(ReadAccess::User("bob".into()))
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(current_reflection(&bob.state, "eve", parent).is_err());
    assert_eq!(admin.snapshot().unwrap().state, state);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn feedback_invalidates_only_old_ready_draft_and_new_revision_derives_once() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    seed(&admin, vec![parent("one", "alice")]);
    let old_id = planner(&admin, ReadAccess::Internal)
        .reconcile(1000)
        .unwrap()
        .created_goal_ids
        .remove(0);
    let before = admin.snapshot().unwrap();
    let original = before.state.goals["one"].description.clone();
    feedback(
        &admin,
        "one",
        before.state.goals["one"].revision,
        "新事实：原先的方案已经失败，请保留预算重新考虑。",
    );
    let changed = admin.snapshot().unwrap();
    assert!(
        current_reflection(&changed.state, "eve", &changed.state.goals["one"])
            .unwrap()
            .is_none()
    );
    let engine = planner(&admin, ReadAccess::Internal);
    let result = engine.reconcile(1100).unwrap();
    assert_eq!(result.invalidated_goal_ids, vec![old_id.clone()]);
    assert_eq!(result.created_goal_ids.len(), 1);
    let after = admin.snapshot().unwrap();
    let current = current_reflection(&after.state, "eve", &after.state.goals["one"])
        .unwrap()
        .unwrap();
    assert_ne!(current.id, old_id);
    assert_eq!(current.id, result.created_goal_ids[0]);
    assert_eq!(after.state.goals[&old_id].status, GoalStatus::Cancelled);
    assert_eq!(after.state.goals["one"].description, original);
    assert!(after.state.events.starts_with(&changed.state.events));
    assert!(current.description.contains("unverified_user_feedback"));
    assert!(current.description.contains("原先的方案已经失败"));
    assert!(
        planner(&admin, ReadAccess::Internal)
            .reconcile(1101)
            .unwrap()
            .created_goal_ids
            .is_empty()
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    kernel.stop_all().await.unwrap();
    kernel.start(&owner()).await.unwrap();
    assert!(
        planner(&admin, ReadAccess::Internal)
            .reconcile(1102)
            .unwrap()
            .created_goal_ids
            .is_empty()
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    let recovered = admin.snapshot().unwrap();
    assert_eq!(
        current_reflection(&recovered.state, "eve", &recovered.state.goals["one"])
            .unwrap()
            .unwrap()
            .id,
        result.created_goal_ids[0]
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn feedback_preserves_executing_or_completed_old_drafts_without_exposing_them_as_current() {
    for completed in [false, true] {
        let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
        seed(&admin, vec![parent("one", "alice")]);
        let old_id = planner(&admin, ReadAccess::Internal)
            .reconcile(1000)
            .unwrap()
            .created_goal_ids
            .remove(0);
        let snapshot = admin.snapshot().unwrap();
        let mut state = snapshot.state;
        let old = state.goals.get_mut(&old_id).unwrap();
        old.status = GoalStatus::Executing;
        old.execution = Some(ExecutionAttempt {
            attempt_id: "attempt-old".into(),
            session_id: "session-old".into(),
            task_id: old_id.clone(),
            turn_id: Some(1),
            started_at_ms: 1001,
        });
        admin.replace(snapshot.revision, state).unwrap();
        if completed {
            let snapshot = admin.snapshot().unwrap();
            let mut state = snapshot.state;
            let old = state.goals.get_mut(&old_id).unwrap();
            old.status = GoalStatus::Completed;
            old.feedback = Some(Feedback {
                commit: ExecutionCommit::Completed,
                verification_met: true,
                started_tools: Some(0),
                summary: "旧反思草稿，不是真实任务完成".into(),
                at_ms: 1002,
            });
            admin.replace(snapshot.revision, state).unwrap();
        }
        let before = admin.snapshot().unwrap();
        let old = before.state.goals[&old_id].clone();
        feedback(
            &admin,
            "one",
            before.state.goals["one"].revision,
            "用户纠正：补充一条尚未独立验证的新事实。",
        );
        let changed = admin.snapshot().unwrap();
        assert!(
            current_reflection(&changed.state, "eve", &changed.state.goals["one"])
                .unwrap()
                .is_none()
        );
        let report = planner(&admin, ReadAccess::Internal)
            .reconcile(1100)
            .unwrap();
        assert!(report.invalidated_goal_ids.is_empty());
        let after = admin.snapshot().unwrap();
        assert_eq!(after.state.goals[&old_id], old);
        let current = current_reflection(&after.state, "eve", &after.state.goals["one"])
            .unwrap()
            .unwrap();
        assert_ne!(current.id, old_id);
        assert_eq!(current.status, GoalStatus::Ready);
        assert!(after.state.events.starts_with(&changed.state.events));
        assert!(
            planner(&admin, ReadAccess::Internal)
                .reconcile(1101)
                .unwrap()
                .created_goal_ids
                .is_empty()
        );
        kernel.stop_all().await.unwrap();
    }
}
