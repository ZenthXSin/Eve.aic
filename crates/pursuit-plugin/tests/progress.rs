use eve_cognition_api::*;
use eve_cognition_plugin::CognitionPlugin;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_pursuit_api::{ProgressFacts, ProgressOutcome};
use eve_pursuit_plugin::{fair_order_key, progress_record, reconcile_progress};
use std::sync::Arc;

fn goal() -> Goal {
    Goal {
        id: "learn".into(),
        revision: 0,
        source: Source {
            kind: SourceKind::Inference,
            channel: "eve.interest".into(),
            reference: "interest".into(),
        },
        visibility: Visibility::User("alice".into()),
        description: "学习自动化".into(),
        verification: "interest-learning:v1".into(),
        priority: 30,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 0,
            max_attempts: 1,
            timeout_ms: 30_000,
        },
        stop_condition: "预算耗尽或用户撤回".into(),
        expires_at_ms: None,
        status: GoalStatus::Waiting,
        wait_reason: Some("已同步的兴趣标记".into()),
        block_reason: None,
        execution: None,
        feedback: None,
        completion: None,
    }
}
fn facts(now_ms: u64) -> ProgressFacts {
    ProgressFacts {
        goal_id: "learn".into(),
        goal_revision: 1,
        evidence_revision: 1,
        last_activity_at_ms: 1,
        waiting_for_new_input: true,
        can_resume: true,
        now_ms,
    }
}

#[tokio::test]
async fn durable_pause_needs_new_evidence_and_does_not_reset_goal_budget() {
    let store = Arc::new(MemoryStateStore::default());
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let mut state = CognitiveState::default();
    state.goals.insert("learn".into(), goal());
    let seeded = admin.replace(0, state).unwrap();
    assert_eq!(
        reconcile_progress(&admin, &facts(100), 1000).unwrap(),
        ProgressOutcome::Observed
    );
    assert_eq!(
        reconcile_progress(&admin, &facts(1100), 1000).unwrap(),
        ProgressOutcome::Paused
    );
    let paused = admin.snapshot().unwrap();
    assert_eq!(paused.state.goals, seeded.state.goals);
    assert!(
        progress_record(&paused.state, "learn")
            .unwrap()
            .unwrap()
            .paused
    );
    assert_eq!(
        reconcile_progress(&admin, &facts(2100), 1000).unwrap(),
        ProgressOutcome::Unchanged
    );
    assert_eq!(admin.snapshot().unwrap(), paused);
    drop(kernel);
    let restarted = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    restarted.register(Box::new(plugin)).unwrap();
    restarted.start_all().await.unwrap();
    let mut fresh = facts(3100);
    fresh.evidence_revision = 2;
    fresh.can_resume = false; // 宿主发现预算用尽、未同步或未知提交时拒绝恢复
    assert_eq!(
        reconcile_progress(&admin, &fresh, 1000).unwrap(),
        ProgressOutcome::Unchanged
    );
    fresh.can_resume = true;
    assert_eq!(
        reconcile_progress(&admin, &fresh, 1000).unwrap(),
        ProgressOutcome::Resumed
    );
    let resumed = admin.snapshot().unwrap();
    assert_eq!(resumed.state.goals, seeded.state.goals);
    assert!(
        !progress_record(&resumed.state, "learn")
            .unwrap()
            .unwrap()
            .paused
    );
    assert!(resumed.state.events.starts_with(&paused.state.events));
    assert_eq!(
        reconcile_progress(&admin, &fresh, 1000).unwrap(),
        ProgressOutcome::Unchanged
    );
}

#[tokio::test]
async fn externally_cancelled_goal_is_never_reopened() {
    let kernel = Kernel::new();
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let mut state = CognitiveState::default();
    state.goals.insert("learn".into(), goal());
    admin.replace(0, state).unwrap();
    reconcile_progress(&admin, &facts(100), 1000).unwrap();
    reconcile_progress(&admin, &facts(1100), 1000).unwrap();
    let before = admin.snapshot().unwrap();
    let mut state = before.state;
    let goal = state.goals.get_mut("learn").unwrap();
    goal.status = GoalStatus::Cancelled;
    goal.wait_reason = None;
    let cancelled = admin.replace(before.revision, state).unwrap();
    let mut fresh = facts(3100);
    fresh.goal_revision = 2;
    fresh.evidence_revision = 2;
    assert_eq!(
        reconcile_progress(&admin, &fresh, 1000).unwrap(),
        ProgressOutcome::Unchanged
    );
    assert_eq!(admin.snapshot().unwrap(), cancelled);
}

#[test]
fn fairness_uses_persisted_service_before_priority() {
    assert!(fair_order_key(None, 1, "low") < fair_order_key(Some(10), 100, "high"));
    assert!(fair_order_key(Some(1), 1, "low") < fair_order_key(Some(10), 100, "high"));
    assert!(fair_order_key(None, 90, "high") < fair_order_key(None, 10, "low"));
}

#[tokio::test]
async fn capacity_and_corruption_do_not_clear_history_or_allow_fresh_start() {
    let kernel = Kernel::new();
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let mut state = CognitiveState::default();
    state.goals.insert("learn".into(), goal());
    admin.replace(0, state).unwrap();
    reconcile_progress(&admin, &facts(100), 1000).unwrap();
    let before = admin.snapshot().unwrap();
    let mut corrupt = before.state.clone();
    corrupt.events[0].summary = "broken".into();
    assert_eq!(
        progress_record(&corrupt, "learn"),
        Err(CognitionError::InvalidInput)
    );
    let mut full = before.state;
    while full.events.len() < MAX_RECORDS {
        full.events.push(CognitiveEvent {
            id: format!("other-{}", full.events.len()),
            kind: CognitiveEventKind::StateChanged,
            source: Source {
                kind: SourceKind::Internal,
                channel: "test".into(),
                reference: "learn".into(),
            },
            visibility: Visibility::User("alice".into()),
            goal_id: Some("learn".into()),
            caused_by: None,
            at_ms: 100,
            summary: "other".into(),
        });
    }
    let full = admin.replace(before.revision, full).unwrap();
    assert_eq!(
        reconcile_progress(&admin, &facts(1100), 1000).unwrap(),
        ProgressOutcome::Deferred
    );
    assert_eq!(admin.snapshot().unwrap(), full);
    assert!(!eve_pursuit_plugin::progress_admitted(&full.state, "learn").unwrap());
}

#[tokio::test]
async fn admission_rechecks_pause_after_shared_guard_and_old_evidence_is_not_new_progress() {
    use eve_pursuit_plugin::{progress_admission_guard, progress_admitted};
    let kernel = Kernel::new();
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let mut state = CognitiveState::default();
    state.goals.insert("learn".into(), goal());
    admin.replace(0, state).unwrap();
    let guard = progress_admission_guard().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = admin.clone();
    let waiting = std::thread::spawn(move || {
        sender.send(()).unwrap();
        let _guard = progress_admission_guard().unwrap();
        progress_admitted(&reader.snapshot().unwrap().state, "learn").unwrap()
    });
    receiver.recv().unwrap();
    // 首次观察继承持久化旧证据时间，不能以服务刚启动为由延长停滞期限。
    assert_eq!(
        reconcile_progress(&admin, &facts(1100), 1000).unwrap(),
        ProgressOutcome::Paused
    );
    drop(guard);
    assert!(!waiting.join().unwrap());
    assert_eq!(
        progress_record(&admin.snapshot().unwrap().state, "learn")
            .unwrap()
            .unwrap()
            .last_progress_at_ms,
        1
    );
}
