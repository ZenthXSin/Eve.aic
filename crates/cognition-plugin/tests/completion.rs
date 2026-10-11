//! 用户确认完成：等待中的目标只能凭确认记录一次性转为完成；确认记录随状态持久化。
use eve_cognition_api::*;
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginId, StateStore};
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
fn goal(id: &str, status: GoalStatus) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: "qq.goal".into(),
            reference: format!("source-{id}"),
        },
        visibility: Visibility::User("alice".into()),
        description: "做一个 Mindustry 方块模组".into(),
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
        wait_reason: (status == GoalStatus::Waiting).then(|| "等待用户处理".into()),
        status,
        block_reason: None,
        execution: None,
        feedback: None,
        completion: None,
    }
}
fn confirmation(goal_revision: u64, by: &str) -> CompletionConfirmation {
    CompletionConfirmation {
        plan_id: "plan:1".into(),
        plan_revision: 4,
        goal_revision,
        confirmed_by: by.into(),
        session_id: "session-alice".into(),
        source_id: format!("tool:{}", "a".repeat(64)),
        at_ms: 1_000,
    }
}
fn completed(mut goal: Goal, completion: CompletionConfirmation) -> Goal {
    goal.status = GoalStatus::Completed;
    goal.wait_reason = None;
    goal.completion = Some(completion);
    goal
}
fn seed(admin: &CognitionController) -> CognitiveSnapshot {
    let mut state = CognitiveState::default();
    state
        .goals
        .insert("goal".into(), goal("goal", GoalStatus::Waiting));
    state
        .goals
        .insert("ready".into(), goal("ready", GoalStatus::Ready));
    admin.replace(0, state).unwrap()
}
fn with(snapshot: &CognitiveSnapshot, id: &str, next: Goal) -> CognitiveState {
    let mut state = snapshot.state.clone();
    state.goals.insert(id.into(), next);
    state
}

#[tokio::test]
async fn waiting_goal_is_confirmed_complete_once_and_the_record_survives_reopening() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    let snapshot = seed(&admin);
    let current = snapshot.state.goals["goal"].clone();
    assert_eq!(current.revision, 1);
    let bytes = store
        .get(&PluginId::new("eve.cognition").unwrap(), "cognition.v1")
        .unwrap()
        .unwrap();
    assert!(
        !String::from_utf8(bytes).unwrap().contains("completion"),
        "没有确认记录时不写入该字段，旧状态文件不变"
    );

    let next = completed(current.clone(), confirmation(1, "alice"));
    let saved = admin
        .replace(snapshot.revision, with(&snapshot, "goal", next))
        .unwrap();
    let done = &saved.state.goals["goal"];
    assert_eq!(
        (done.status.clone(), done.revision),
        (GoalStatus::Completed, 2)
    );
    assert_eq!(done.completion.as_ref().unwrap().plan_id, "plan:1");

    // 完成是终态：确认之后不能再改动或撤回。
    let mut again = done.clone();
    again.completion = Some(confirmation(2, "alice"));
    assert_eq!(
        admin
            .replace(saved.revision, with(&saved, "goal", again))
            .unwrap_err(),
        CognitionError::InvalidTransition
    );
    drop(kernel);

    let (_kernel, reopened) = open(store).await;
    let restored = reopened.snapshot().unwrap();
    assert_eq!(restored.state.goals["goal"], saved.state.goals["goal"]);
}

#[tokio::test]
async fn confirmation_requires_the_current_waiting_version_owner_and_unchanged_content() {
    let (_kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let snapshot = seed(&admin);
    let current = snapshot.state.goals["goal"].clone();
    let attempt =
        |id: &str, next: Goal| admin.replace(snapshot.revision, with(&snapshot, id, next));

    // 计划绑定的是旧版本。
    assert_eq!(
        attempt("goal", completed(current.clone(), confirmation(7, "alice"))).unwrap_err(),
        CognitionError::InvalidTransition
    );
    // 确认者不是目标所属用户。
    assert_eq!(
        attempt("goal", completed(current.clone(), confirmation(1, "bob"))).unwrap_err(),
        CognitionError::InvalidInput
    );
    // 确认时改动目标内容。
    let mut changed = completed(current.clone(), confirmation(1, "alice"));
    changed.description = "换一个目标".into();
    assert_eq!(
        attempt("goal", changed).unwrap_err(),
        CognitionError::InvalidTransition
    );
    // 没有确认记录也没有执行记录，不能直接完成。
    let mut bare = current.clone();
    bare.status = GoalStatus::Completed;
    bare.wait_reason = None;
    assert_eq!(
        attempt("goal", bare).unwrap_err(),
        CognitionError::InvalidInput
    );
    // 确认记录只在完成时出现。
    let mut still_waiting = current.clone();
    still_waiting.completion = Some(confirmation(1, "alice"));
    assert_eq!(
        attempt("goal", still_waiting).unwrap_err(),
        CognitionError::InvalidInput
    );
    // 确认完成不能同时带执行记录。
    let mut executed = completed(current.clone(), confirmation(1, "alice"));
    executed.execution = Some(ExecutionAttempt {
        attempt_id: "attempt".into(),
        session_id: "session".into(),
        task_id: "task".into(),
        turn_id: Some(1),
        started_at_ms: 1,
    });
    assert_eq!(
        attempt("goal", executed).unwrap_err(),
        CognitionError::InvalidInput
    );
    // 就绪中的目标不是等待用户处理的目标，不能凭确认完成。
    let ready = snapshot.state.goals["ready"].clone();
    assert_eq!(
        attempt("ready", completed(ready, confirmation(1, "alice"))).unwrap_err(),
        CognitionError::InvalidTransition
    );
    // 以上都没有写入。
    assert_eq!(admin.snapshot().unwrap(), snapshot);
}
