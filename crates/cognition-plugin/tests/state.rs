use eve_cognition_api::*;
use eve_cognition_plugin::{COGNITION_STATE_KEY, CognitionController, CognitionPlugin};
use eve_kernel::{
    Kernel, KernelServices,
    backends::{FileStateStore, MemoryStateStore},
};
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, ServiceRegistry, StateStore};
use serde_json::json;
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
            return Err(PluginError::State("backend-secret".into()));
        }
        self.memory.set(namespace, key, value)
    }
}
fn id() -> PluginId {
    PluginId::new(COGNITION_PLUGIN_ID).unwrap()
}
fn goal(name: &str, visibility: Visibility) -> Goal {
    Goal {
        id: name.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: "qq".into(),
            reference: "message-1".into(),
        },
        visibility,
        description: "private-goal-body".into(),
        verification: "验证回执".into(),
        priority: 50,
        budget: ExecutionBudget {
            max_model_requests: 4,
            max_tool_calls: 1,
            max_attempts: 1,
            timeout_ms: 1000,
        },
        stop_condition: "保存回执后结束".into(),
        expires_at_ms: None,
        status: GoalStatus::Ready,
        wait_reason: None,
        block_reason: None,
        execution: None,
        feedback: None,
    }
}
fn executing(goal: &mut Goal, name: &str) {
    goal.status = GoalStatus::Executing;
    goal.execution = Some(ExecutionAttempt {
        attempt_id: name.into(),
        session_id: "internal-session".into(),
        task_id: name.into(),
        turn_id: Some(1),
        started_at_ms: 1,
    });
}
fn complete(goal: &mut Goal) {
    goal.status = GoalStatus::Completed;
    goal.feedback = Some(Feedback {
        commit: ExecutionCommit::Completed,
        verification_met: true,
        started_tools: Some(1),
        summary: "private-feedback-body".into(),
        at_ms: 2,
    });
}
async fn open(
    state: Arc<dyn StateStore>,
) -> (Kernel, CognitionController, Arc<dyn ServiceRegistry>) {
    let services = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let plugin = CognitionPlugin::new("eve").unwrap();
    let controller = plugin.controller();
    assert_eq!(controller.snapshot(), Err(CognitionError::Unavailable));
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, controller, registry)
}
fn public(registry: &Arc<dyn ServiceRegistry>) -> Arc<dyn CognitionReader> {
    registry
        .get(&ServiceId::new(COGNITION_READ_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<CognitionReadHandle>()
        .unwrap()
        .0
        .clone()
}

#[tokio::test]
async fn views_bind_identity_and_filter_goals_and_derived_data() {
    let (kernel, admin, registry) = open(Arc::new(MemoryStateStore::default())).await;
    let mut state = CognitiveState::default();
    state
        .goals
        .insert("public".into(), goal("public", Visibility::Public));
    state.goals.insert(
        "alice".into(),
        goal("alice", Visibility::User("alice".into())),
    );
    state
        .goals
        .insert("bob".into(), goal("bob", Visibility::User("bob".into())));
    state
        .goals
        .insert("internal".into(), goal("internal", Visibility::Internal));
    state.drives.insert(
        "private-drive".into(),
        Drive {
            id: "private-drive".into(),
            visibility: Visibility::User("alice".into()),
            goal_ids: vec!["alice".into()],
            strength: 80,
            reason: "private-drive-body".into(),
            evaluated_at_ms: 1,
            valid_until_ms: 100,
        },
    );
    state.agenda = Some(Agenda {
        visibility: Visibility::Internal,
        candidates: vec!["alice".into()],
        selected: Some("alice".into()),
        reason: "private-agenda-body".into(),
        valid_until_ms: 100,
    });
    state.events.push(CognitiveEvent {
        id: "event-1".into(),
        kind: CognitiveEventKind::ExternalInput,
        source: state.goals["alice"].source.clone(),
        visibility: Visibility::User("alice".into()),
        goal_id: Some("alice".into()),
        caused_by: None,
        at_ms: 1,
        summary: "private-event-body".into(),
    });
    admin.replace(0, state.clone()).unwrap();
    let public = public(&registry);
    let alice = admin.reader(ReadAccess::User("alice".into())).unwrap();
    let bob = admin.reader(ReadAccess::User("bob".into())).unwrap();
    let internal = admin.reader(ReadAccess::Internal).unwrap();
    assert_eq!(public.snapshot().unwrap().state.goals.len(), 1);
    let alice_view = alice.snapshot().unwrap();
    assert_eq!(alice_view.state.goals.len(), 2);
    assert_eq!(alice_view.state.drives.len(), 1);
    assert_eq!(alice_view.state.events.len(), 1);
    assert!(alice_view.state.agenda.is_none());
    assert!(!alice_view.state.goals.contains_key("bob"));
    let bob_view = bob.snapshot().unwrap();
    assert!(bob_view.state.drives.is_empty() && bob_view.state.events.is_empty());
    assert_eq!(internal.snapshot().unwrap().state.goals.len(), 4);
    assert!(internal.snapshot().unwrap().state.agenda.is_some());
    let diagnostic = format!(
        "{:?} {:?} {:?}",
        alice_view,
        admin.snapshot().unwrap(),
        state
    );
    for secret in [
        "private-goal-body",
        "private-drive-body",
        "private-event-body",
        "private-agenda-body",
        "alice",
        "bob",
    ] {
        assert!(!diagnostic.contains(secret));
    }
    let mut widened = admin.snapshot().unwrap().state;
    widened.drives.get_mut("private-drive").unwrap().visibility = Visibility::Public;
    assert_eq!(admin.replace(1, widened), Err(CognitionError::AccessDenied));
    kernel.stop_all().await.unwrap();
    for reader in [public, alice, bob, internal] {
        assert_eq!(reader.snapshot(), Err(CognitionError::Unavailable));
    }
    assert_eq!(admin.snapshot(), Err(CognitionError::Unavailable));
    assert!(
        registry
            .get(&ServiceId::new(COGNITION_READ_SERVICE_ID).unwrap())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn concurrent_cas_commits_once_and_preserves_old_revision() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin, _) = open(store.clone()).await;
    let mut state = CognitiveState::default();
    state
        .goals
        .insert("original".into(), goal("original", Visibility::Public));
    admin.replace(0, state).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let jobs: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            let admin = admin.clone();
            let barrier = barrier.clone();
            let mut next = admin.snapshot().unwrap().state;
            next.goals
                .insert(name.into(), goal(name, Visibility::Internal));
            std::thread::spawn(move || {
                barrier.wait();
                admin.replace(1, next)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err(CognitionError::StaleRevision))
            .count(),
        1
    );
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.revision, 2);
    assert_eq!(snapshot.state.goals.len(), 2);
    assert_eq!(snapshot.state.goals["original"].revision, 1);
    let before = store.get(&id(), COGNITION_STATE_KEY).unwrap();
    assert_eq!(
        admin.replace(1, snapshot.state),
        Err(CognitionError::StaleRevision)
    );
    assert_eq!(store.get(&id(), COGNITION_STATE_KEY).unwrap(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_commit_keeps_disk_memory_and_execution_mark() {
    let store = Arc::new(FaultStore::default());
    let (kernel, admin, _) = open(store.clone()).await;
    let mut state = CognitiveState::default();
    state
        .goals
        .insert("g".into(), goal("g", Visibility::Internal));
    store.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        admin.replace(0, state.clone()),
        Err(CognitionError::Storage)
    );
    assert_eq!(admin.snapshot().unwrap().revision, 0);
    assert!(store.get(&id(), COGNITION_STATE_KEY).unwrap().is_none());
    store.fail.store(false, Ordering::SeqCst);
    let mut state = admin.replace(0, state).unwrap().state;
    executing(state.goals.get_mut("g").unwrap(), "attempt-1");
    let pending = admin.replace(1, state).unwrap();
    let bytes = store.get(&id(), COGNITION_STATE_KEY).unwrap();
    let mut completed = pending.state.clone();
    complete(completed.goals.get_mut("g").unwrap());
    store.fail.store(true, Ordering::SeqCst);
    assert_eq!(admin.replace(2, completed), Err(CognitionError::Storage));
    assert_eq!(admin.snapshot().unwrap(), pending);
    assert_eq!(store.get(&id(), COGNITION_STATE_KEY).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
    let plugin = CognitionPlugin::new("eve").unwrap();
    let new_admin = plugin.controller();
    kernel.unregister(&id()).unwrap();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(new_admin.snapshot(), Err(CognitionError::Unavailable));
    assert_eq!(store.get(&id(), COGNITION_STATE_KEY).unwrap(), bytes);
    store.fail.store(false, Ordering::SeqCst);
    kernel.start_all().await.unwrap();
    let recovered = new_admin.snapshot().unwrap();
    assert_eq!(recovered.revision, 3);
    assert_eq!(recovered.state.goals["g"].status, GoalStatus::Blocked);
    assert_eq!(
        recovered.state.goals["g"].block_reason,
        Some(BlockReason::Interrupted)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn terminal_uncertain_and_inflight_goals_cannot_be_replayed_or_rewritten() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin, _) = open(store.clone()).await;
    let mut state = CognitiveState::default();
    state
        .goals
        .insert("g".into(), goal("g", Visibility::Internal));
    let ready = admin.replace(0, state).unwrap();
    let mut changed_owner = ready.state.clone();
    changed_owner.goals.get_mut("g").unwrap().visibility = Visibility::Public;
    assert_eq!(
        admin.replace(1, changed_owner),
        Err(CognitionError::AccessDenied)
    );
    let mut running = ready.state;
    executing(running.goals.get_mut("g").unwrap(), "attempt");
    let running = admin.replace(1, running).unwrap();
    let mut rewritten = running.state.clone();
    rewritten.goals.get_mut("g").unwrap().budget.timeout_ms += 1;
    assert_eq!(
        admin.replace(2, rewritten),
        Err(CognitionError::InvalidTransition)
    );
    let mut cancelled = running.state.clone();
    cancelled.goals.get_mut("g").unwrap().status = GoalStatus::Cancelled;
    assert_eq!(
        admin.replace(2, cancelled),
        Err(CognitionError::InvalidTransition)
    );
    let mut unverified = running.state.clone();
    complete(unverified.goals.get_mut("g").unwrap());
    unverified
        .goals
        .get_mut("g")
        .unwrap()
        .feedback
        .as_mut()
        .unwrap()
        .verification_met = false;
    assert_eq!(
        admin.replace(2, unverified),
        Err(CognitionError::InvalidInput)
    );
    let mut complete_state = running.state;
    complete(complete_state.goals.get_mut("g").unwrap());
    let completed = admin.replace(2, complete_state).unwrap();
    let bytes = store.get(&id(), COGNITION_STATE_KEY).unwrap();
    let mut replay = completed.state.clone();
    let goal = replay.goals.get_mut("g").unwrap();
    goal.status = GoalStatus::Ready;
    goal.execution = None;
    goal.feedback = None;
    assert_eq!(
        admin.replace(3, replay),
        Err(CognitionError::InvalidTransition)
    );
    let mut deleted = completed.state;
    deleted.goals.clear();
    assert_eq!(
        admin.replace(3, deleted),
        Err(CognitionError::InvalidTransition)
    );
    assert_eq!(store.get(&id(), COGNITION_STATE_KEY).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn file_restart_preserves_sources_and_only_blocks_executing() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, admin, registry) =
        open(Arc::new(FileStateStore::open(directory.path()).unwrap())).await;
    let mut state = CognitiveState::default();
    for name in ["ready", "done", "uncertain"] {
        state
            .goals
            .insert(name.into(), goal(name, Visibility::Public));
    }
    let mut state = admin.replace(0, state).unwrap().state;
    executing(state.goals.get_mut("done").unwrap(), "done-1");
    let mut state = admin.replace(1, state).unwrap().state;
    complete(state.goals.get_mut("done").unwrap());
    let mut state = admin.replace(2, state).unwrap().state;
    executing(state.goals.get_mut("uncertain").unwrap(), "uncertain-1");
    state.agenda = Some(Agenda {
        visibility: Visibility::Public,
        candidates: vec!["ready".into()],
        selected: Some("uncertain".into()),
        reason: "当前执行".into(),
        valid_until_ms: 10,
    });
    let saved = admin.replace(3, state).unwrap();
    let old_reader = public(&registry);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    let (kernel, recovered, _) =
        open(Arc::new(FileStateStore::open(directory.path()).unwrap())).await;
    let after = recovered.snapshot().unwrap();
    assert_eq!(after.revision, 5);
    assert_eq!(after.state.goals["done"], saved.state.goals["done"]);
    assert_eq!(after.state.goals["ready"], saved.state.goals["ready"]);
    assert_eq!(after.state.goals["uncertain"].status, GoalStatus::Blocked);
    assert_eq!(
        after.state.goals["uncertain"].execution,
        saved.state.goals["uncertain"].execution
    );
    assert!(after.state.agenda.unwrap().selected.is_none());
    assert_eq!(old_reader.snapshot(), Err(CognitionError::Unavailable));
    assert_eq!(admin.snapshot(), Err(CognitionError::Unavailable));
    kernel.stop_all().await.unwrap();
    drop(kernel);
    let bytes = std::fs::read(directory.path().join("state.json")).unwrap();
    let (kernel, unchanged, _) =
        open(Arc::new(FileStateStore::open(directory.path()).unwrap())).await;
    assert_eq!(unchanged.snapshot().unwrap().revision, 5);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        bytes
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_version_subject_and_duplicate_keys_leave_original_bytes_untouched() {
    let mut state = CognitiveState::default();
    let mut g = goal("g", Visibility::Public);
    g.revision = 1;
    state.goals.insert("g".into(), g);
    let document = CognitiveSnapshot {
        format_version: 1,
        subject_id: "eve".into(),
        revision: 1,
        state,
    };
    let valid = serde_json::to_value(&document).unwrap();
    let mut cases = vec![b"broken-secret".to_vec()];
    for (pointer, value) in [
        ("/format_version", json!(2)),
        ("/subject_id", json!("another-subject")),
        ("/state/goals/g/budget/timeout_ms", json!(0)),
        ("/state/goals/g/revision", json!(0)),
        ("/state/goals/g/status", json!({"Ready": {"unknown": true}})),
    ] {
        let mut value_doc = valid.clone();
        *value_doc.pointer_mut(pointer).unwrap() = value;
        cases.push(serde_json::to_vec(&value_doc).unwrap());
    }
    let mut unknown = valid.clone();
    unknown["state"]["unknown"] = json!("private-secret");
    cases.push(serde_json::to_vec(&unknown).unwrap());
    let goal_json = serde_json::to_string(&document.state.goals["g"]).unwrap();
    cases.push(format!("{{\"format_version\":1,\"subject_id\":\"eve\",\"revision\":1,\"state\":{{\"goals\":{{\"g\":{goal_json},\"g\":{goal_json}}},\"drives\":{{}},\"agenda\":null,\"events\":[]}}}}").into_bytes());
    cases.push(vec![b'x'; MAX_STATE_BYTES + 1]);
    for bytes in cases {
        let store = Arc::new(MemoryStateStore::default());
        store
            .set(&id(), COGNITION_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let services = KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        };
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        let plugin = CognitionPlugin::new("eve").unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        let error = kernel.start_all().await.unwrap_err();
        let message = format!("{error:?} {error}");
        assert!(!message.contains("private-secret") && !message.contains("broken-secret"));
        assert_eq!(admin.snapshot(), Err(CognitionError::Unavailable));
        assert!(
            registry
                .get(&ServiceId::new(COGNITION_READ_SERVICE_ID).unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(store.get(&id(), COGNITION_STATE_KEY).unwrap(), Some(bytes));
    }
}

#[tokio::test]
async fn expiration_and_private_causal_derivations_are_checked() {
    let (kernel, admin, _) = open(Arc::new(MemoryStateStore::default())).await;
    let mut state = CognitiveState::default();
    let mut g = goal("g", Visibility::User("u".into()));
    g.expires_at_ms = Some(10);
    state.goals.insert("g".into(), g);
    state.events.push(CognitiveEvent {
        id: "private".into(),
        kind: CognitiveEventKind::ExternalInput,
        source: state.goals["g"].source.clone(),
        visibility: Visibility::User("u".into()),
        goal_id: Some("g".into()),
        caused_by: None,
        at_ms: 1,
        summary: "用户原文".into(),
    });
    let saved = admin.replace(0, state).unwrap();
    let reader = admin.reader(ReadAccess::User("u".into())).unwrap();
    assert_eq!(reader.snapshot().unwrap().ready_goal_ids(9), vec!["g"]);
    assert!(reader.snapshot().unwrap().ready_goal_ids(10).is_empty());
    let mut public_event = saved.state.clone();
    public_event.events.push(CognitiveEvent {
        id: "leak".into(),
        kind: CognitiveEventKind::StateChanged,
        source: Source {
            kind: SourceKind::Inference,
            channel: "internal".into(),
            reference: "private".into(),
        },
        visibility: Visibility::Public,
        goal_id: None,
        caused_by: Some("private".into()),
        at_ms: 2,
        summary: "派生摘要".into(),
    });
    assert_eq!(
        admin.replace(1, public_event),
        Err(CognitionError::AccessDenied)
    );
    let mut edited = saved.state;
    edited.events[0].summary = "修改历史".into();
    assert_eq!(
        admin.replace(1, edited),
        Err(CognitionError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
}
