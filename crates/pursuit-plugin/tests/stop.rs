//! 停止执行：一次认知状态替换把仍在等待、版本未变的目标标为已取消并追加停止事件。
use eve_cognition_api::*;
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::StateStore;
use eve_pursuit_api::{POLICY_VERSION, PURSUIT_CHANNEL, STOP_EVENT_SCHEMA, StopReason};
use eve_pursuit_plugin::{StopOutcome, StopRequest, stop_event_id, stop_goal, stop_record};
use std::sync::Arc;

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
fn goal(id: &str) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::Inference,
            channel: "eve.interest".into(),
            reference: format!("interest-{id}"),
        },
        visibility: Visibility::User("alice".into()),
        description: "学习 Mindustry 模组创作".into(),
        verification: "interest-learning:v1".into(),
        priority: 30,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 0,
            max_attempts: 1,
            timeout_ms: 30_000,
        },
        stop_condition: "用户撤回该兴趣，或学习预算耗尽".into(),
        expires_at_ms: None,
        status: GoalStatus::Waiting,
        wait_reason: Some("等待学习".into()),
        block_reason: None,
        execution: None,
        feedback: None,
        completion: None,
    }
}
fn seed(admin: &CognitionController, ids: &[&str]) -> CognitiveSnapshot {
    let mut state = CognitiveState::default();
    for id in ids {
        state.goals.insert((*id).into(), goal(id));
    }
    admin.replace(0, state).unwrap()
}
fn request(goal_id: &str, goal_revision: u64) -> StopRequest {
    StopRequest {
        goal_id: goal_id.into(),
        goal_revision,
        reason: StopReason::LearningBudgetSpent,
        research_runs: 3,
        practice_runs: 2,
        policy_version: POLICY_VERSION.into(),
    }
}

#[tokio::test]
async fn stops_a_waiting_goal_once_with_a_durable_record() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    let before = seed(&admin, &["goal", "other"]);
    assert_eq!(stop_record(&before.state, "goal"), None);

    assert_eq!(
        stop_goal(&admin, &request("goal", 1), 5_000).unwrap(),
        StopOutcome::Stopped
    );
    let after = admin.snapshot().unwrap();
    assert_eq!(after.revision, before.revision + 1);
    let stopped = &after.state.goals["goal"];
    assert_eq!(
        (
            stopped.status.clone(),
            stopped.revision,
            stopped.wait_reason.clone()
        ),
        (GoalStatus::Cancelled, 2, None)
    );
    assert_eq!(after.state.goals["other"], before.state.goals["other"]);
    let [event] = after.state.events.as_slice() else {
        panic!("恰好一条停止事件");
    };
    assert_eq!(event.id, stop_event_id("goal", 1));
    assert_eq!(
        (event.source.kind.clone(), event.source.channel.as_str()),
        (SourceKind::Internal, PURSUIT_CHANNEL)
    );
    assert_eq!(event.visibility, Visibility::User("alice".into()));
    assert_eq!(event.at_ms, 5_000);
    let record = stop_record(&after.state, "goal").unwrap();
    assert_eq!(record.schema, STOP_EVENT_SCHEMA);
    assert_eq!(record.reason, StopReason::LearningBudgetSpent);
    assert_eq!(
        (
            record.goal_revision,
            record.research_runs,
            record.practice_runs
        ),
        (1, 3, 2)
    );
    assert!(!record.user_instruction);
    assert_eq!(stop_record(&after.state, "other"), None);

    // 再次执行不改动：目标已不在等待。
    assert_eq!(
        stop_goal(&admin, &request("goal", 1), 6_000).unwrap(),
        StopOutcome::AlreadyClosed
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    drop(kernel);

    let (_kernel, reopened) = open(store).await;
    let restored = reopened.snapshot().unwrap();
    assert_eq!(restored.state, after.state);
    assert_eq!(stop_record(&restored.state, "goal"), Some(record));
}

#[tokio::test]
async fn changed_missing_or_closed_goals_are_left_alone() {
    let (_kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let snapshot = seed(&admin, &["goal", "cancelled"]);
    let mut state = snapshot.state.clone();
    let closed = state.goals.get_mut("cancelled").unwrap();
    closed.status = GoalStatus::Cancelled;
    closed.wait_reason = None;
    let snapshot = admin.replace(snapshot.revision, state).unwrap();

    // 判断之后目标有了新版本（例如用户又提到了这个兴趣），须重新判断。
    assert_eq!(
        stop_goal(&admin, &request("goal", 7), 1).unwrap(),
        StopOutcome::Changed
    );
    assert_eq!(
        stop_goal(&admin, &request("missing", 1), 1).unwrap(),
        StopOutcome::Changed
    );
    assert_eq!(
        stop_goal(&admin, &request("cancelled", 2), 1).unwrap(),
        StopOutcome::AlreadyClosed
    );
    assert_eq!(admin.snapshot().unwrap(), snapshot);
}

#[tokio::test]
async fn full_event_records_defer_without_writing() {
    let (_kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let snapshot = seed(&admin, &["goal"]);
    let mut state = snapshot.state.clone();
    for index in 0..MAX_RECORDS {
        state.events.push(CognitiveEvent {
            id: format!("event-{index}"),
            kind: CognitiveEventKind::StateChanged,
            source: Source {
                kind: SourceKind::Internal,
                channel: "test".into(),
                reference: "goal".into(),
            },
            visibility: Visibility::User("alice".into()),
            goal_id: None,
            caused_by: None,
            at_ms: 1,
            summary: "{}".into(),
        });
    }
    let full = admin.replace(snapshot.revision, state).unwrap();
    assert_eq!(
        stop_goal(&admin, &request("goal", 1), 1).unwrap(),
        StopOutcome::Deferred
    );
    assert_eq!(admin.snapshot().unwrap(), full);
}

#[test]
fn records_from_other_sources_or_versions_are_not_stop_records() {
    let mut state = CognitiveState::default();
    let summary = format!(
        r#"{{"schema":"{STOP_EVENT_SCHEMA}","reason":"learning_budget_spent","goal_revision":1,"research_runs":3,"practice_runs":2,"policy_version":"{POLICY_VERSION}","user_instruction":false}}"#
    );
    let event = |id: String, channel: &str| CognitiveEvent {
        id,
        kind: CognitiveEventKind::StateChanged,
        source: Source {
            kind: SourceKind::Internal,
            channel: channel.into(),
            reference: "goal".into(),
        },
        visibility: Visibility::User("alice".into()),
        goal_id: Some("goal".into()),
        caused_by: None,
        at_ms: 1,
        summary: summary.clone(),
    };
    // 其他来源写出同样的摘要不算；ID 与版本不对应也不算。
    state
        .events
        .push(event(stop_event_id("goal", 1), "eve.interest"));
    state
        .events
        .push(event(stop_event_id("goal", 2), PURSUIT_CHANNEL));
    assert_eq!(stop_record(&state, "goal"), None);
    state
        .events
        .push(event(stop_event_id("goal", 1), PURSUIT_CHANNEL));
    assert_eq!(stop_record(&state, "goal").unwrap().goal_revision, 1);
}
