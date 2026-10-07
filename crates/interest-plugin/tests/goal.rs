mod support;

use eve_cognition_api::{
    CognitionAdmin, CognitionError, CognitionReader, CognitionResult, CognitiveEventKind,
    CognitiveSnapshot, CognitiveState, ExecutionBudget, Goal, GoalStatus, ReadAccess, Source,
    SourceKind, Visibility,
};
use eve_cognition_loop_api::{AllowedSource, EndogenousOptions, ExecutionScope};
use eve_cognition_loop_plugin::EndogenousPlanner;
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_interest_api::*;
use eve_interest_plugin::{
    InterestController, InterestPlugin, LearningGoalDeriver, learning_goal_id,
};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use std::sync::{Arc, Mutex};
use support::*;

async fn open() -> (Kernel, InterestController, CognitionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(MemoryStateStore::default()),
        ..KernelServices::default()
    });
    let interests = InterestPlugin::new().unwrap();
    let interest_admin = interests.controller();
    let cognition = CognitionPlugin::new("eve").unwrap();
    let cognition_admin = cognition.controller();
    kernel.register(Box::new(interests)).unwrap();
    kernel.register(Box::new(cognition)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, interest_admin, cognition_admin)
}

/// 通过真实账本创建兴趣，确保派生器只消费合法记录。
fn observed(admin: &InterestController, texts: &[&str], at_ms: u64) -> Vec<InterestRecord> {
    let alice = scope("session-a", "qq:alice");
    let batch = reserve(admin, &memory(alice.clone(), texts), at_ms);
    let update = if texts.len() == 1 {
        new_interest(&batch, "Mindustry 模组创作")
    } else {
        let id = admin.snapshot(&alice).unwrap().interests[0].id.clone();
        InterestUpdateDraft {
            target: InterestTarget::Existing { id },
            statements: vec![statement(
                StatementKind::Experience,
                texts.last().unwrap(),
                &batch.evidence[0].id,
            )],
            inferred_need: Some("可能需要先看最小示例".into()),
        }
    };
    admin
        .finish(&batch, at_ms, ObservationOutcome::Completed(vec![update]))
        .unwrap();
    admin.snapshot(&alice).unwrap().interests
}

#[tokio::test]
async fn active_interest_derives_one_waiting_learning_goal_marked_as_inference() {
    let (kernel, interests, cognition) = open().await;
    let records = observed(&interests, &[INTEREST], 100);
    let deriver = LearningGoalDeriver::new(Arc::new(cognition.clone()), "eve").unwrap();
    let report = deriver.reconcile(&records, 1_000).unwrap();
    let id = learning_goal_id("eve", &records[0].id);
    assert_eq!(report.created, vec![id.clone()]);
    let snapshot = cognition.snapshot().unwrap();
    let goal = &snapshot.state.goals[&id];
    assert_eq!(goal.revision, 1);
    assert_eq!(
        goal.source,
        Source {
            kind: SourceKind::Inference,
            channel: INTEREST_GOAL_CHANNEL.into(),
            reference: records[0].id.clone(),
        }
    );
    assert_eq!(goal.visibility, Visibility::User("qq:alice".into()));
    assert_eq!(goal.verification, INTEREST_GOAL_VERIFICATION);
    assert_eq!(goal.status, GoalStatus::Waiting);
    assert!(goal.priority < 50, "低于用户待办的默认优先级");
    assert_eq!(goal.budget.max_tool_calls, 0);
    for expected in [
        "不是用户下达的任务",
        "Mindustry 模组创作",
        "[兴趣] “我喜欢 Mindustry 这个游戏的模组”",
        "[困难] “不知道怎么创作”",
        "来源 e-1",
        "模型推断（未经用户确认，不是任务）：可能希望学习如何制作模组",
    ] {
        assert!(
            goal.description.contains(expected),
            "{expected}\n{}",
            goal.description
        );
    }
    let events: Vec<_> = snapshot
        .state
        .events
        .iter()
        .filter(|event| event.goal_id.as_deref() == Some(id.as_str()))
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, CognitiveEventKind::StateChanged);
    assert_eq!(events[0].source, goal.source);
    assert!(events[0].summary.contains("\"user_instruction\":false"));
    // 重复同步零写入；其他用户的读取看不到这条目标。
    assert_eq!(
        deriver.reconcile(&records, 2_000).unwrap(),
        DerivationReport::default()
    );
    assert_eq!(cognition.snapshot().unwrap().revision, snapshot.revision);
    let bob = cognition.reader(ReadAccess::User("qq:bob".into())).unwrap();
    assert!(bob.snapshot().unwrap().state.goals.is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn new_statements_refresh_the_goal_and_withdrawal_cancels_it() {
    let (kernel, interests, cognition) = open().await;
    let deriver = LearningGoalDeriver::new(Arc::new(cognition.clone()), "eve").unwrap();
    let records = observed(&interests, &[INTEREST], 100);
    deriver.reconcile(&records, 1_000).unwrap();
    let id = learning_goal_id("eve", &records[0].id);
    let records = observed(&interests, &[INTEREST, "我会写一点 Java"], 200);
    let report = deriver.reconcile(&records, 2_000).unwrap();
    assert_eq!(report.updated, vec![id.clone()]);
    let goal = cognition.snapshot().unwrap().state.goals[&id].clone();
    assert_eq!(goal.revision, 2, "新陈述提高目标修订，供反思重新规划");
    assert!(goal.description.contains("[经验] “我会写一点 Java”"));
    assert!(goal.description.contains("可能需要先看最小示例"));
    let withdrawn = interests
        .withdraw(
            &records[0].scope,
            &records[0].id,
            WithdrawalRequest {
                evidence_id: "command-1".into(),
                message_id: "command-message-1".into(),
                text: format!("/forget-interest {}", records[0].id),
                at_ms: 300,
            },
        )
        .unwrap();
    let report = deriver
        .reconcile(std::slice::from_ref(&withdrawn), 3_000)
        .unwrap();
    assert_eq!(report.cancelled, vec![id.clone()]);
    let snapshot = cognition.snapshot().unwrap();
    let goal = &snapshot.state.goals[&id];
    assert_eq!(goal.status, GoalStatus::Cancelled);
    assert_eq!(goal.wait_reason, None);
    assert_eq!(
        snapshot
            .state
            .events
            .iter()
            .filter(|event| event.goal_id.as_deref() == Some(id.as_str()))
            .count(),
        3
    );
    assert_eq!(
        deriver.reconcile(&[withdrawn], 4_000).unwrap(),
        DerivationReport::default()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn withdrawn_interest_without_goal_is_never_derived() {
    let (kernel, interests, cognition) = open().await;
    let records = observed(&interests, &[INTEREST], 100);
    let withdrawn = interests
        .withdraw(
            &records[0].scope,
            &records[0].id,
            WithdrawalRequest {
                evidence_id: "command-1".into(),
                message_id: "command-message-1".into(),
                text: "/forget-interest".into(),
                at_ms: 150,
            },
        )
        .unwrap();
    let deriver = LearningGoalDeriver::new(Arc::new(cognition.clone()), "eve").unwrap();
    assert_eq!(
        deriver.reconcile(&[withdrawn], 1_000).unwrap(),
        DerivationReport::default()
    );
    assert!(cognition.snapshot().unwrap().state.goals.is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn goals_from_other_sources_are_never_overwritten() {
    let (kernel, interests, cognition) = open().await;
    let records = observed(&interests, &[INTEREST], 100);
    let id = learning_goal_id("eve", &records[0].id);
    let mut state = cognition.snapshot().unwrap().state;
    state.goals.insert(
        id.clone(),
        Goal {
            id: id.clone(),
            revision: 0,
            source: Source {
                kind: SourceKind::User,
                channel: "qq.goal".into(),
                reference: records[0].id.clone(),
            },
            visibility: Visibility::User("qq:alice".into()),
            description: "用户自己的待办".into(),
            verification: "user-goal:v1".into(),
            priority: 50,
            budget: ExecutionBudget {
                max_model_requests: 1,
                max_tool_calls: 0,
                max_attempts: 1,
                timeout_ms: 30_000,
            },
            stop_condition: "user-confirmation".into(),
            expires_at_ms: None,
            status: GoalStatus::Waiting,
            wait_reason: Some("等待".into()),
            block_reason: None,
            execution: None,
            feedback: None,
        },
    );
    let saved = cognition.replace(0, state).unwrap();
    let deriver = LearningGoalDeriver::new(Arc::new(cognition.clone()), "eve").unwrap();
    assert_eq!(
        deriver.reconcile(&records, 1_000),
        Err(InterestError::Derivation)
    );
    assert_eq!(cognition.snapshot().unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

/// 首次提交返回修订冲突，验证派生器暂缓并在下一次核对时成功。
struct Racing {
    inner: CognitionController,
    conflicts: Mutex<usize>,
}
impl CognitionAdmin for Racing {
    fn snapshot(&self) -> CognitionResult<CognitiveSnapshot> {
        self.inner.snapshot()
    }
    fn reader(&self, access: ReadAccess) -> CognitionResult<Arc<dyn CognitionReader>> {
        self.inner.reader(access)
    }
    fn replace(&self, expected: u64, state: CognitiveState) -> CognitionResult<CognitiveSnapshot> {
        let mut left = self.conflicts.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err(CognitionError::StaleRevision);
        }
        drop(left);
        self.inner.replace(expected, state)
    }
}

#[tokio::test]
async fn stale_revisions_defer_without_partial_goals_and_next_reconcile_succeeds() {
    let (kernel, interests, cognition) = open().await;
    let records = observed(&interests, &[INTEREST], 100);
    let deriver = LearningGoalDeriver::new(
        Arc::new(Racing {
            inner: cognition.clone(),
            conflicts: Mutex::new(1),
        }),
        "eve",
    )
    .unwrap();
    let report = deriver.reconcile(&records, 1_000).unwrap();
    assert_eq!(report.deferred, vec![records[0].id.clone()]);
    assert!(cognition.snapshot().unwrap().state.goals.is_empty());
    assert_eq!(deriver.reconcile(&records, 2_000).unwrap().created.len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn reflection_planner_accepts_learning_goals_only_when_the_channel_is_authorized() {
    let (kernel, interests, cognition) = open().await;
    let records = observed(&interests, &[INTEREST], 100);
    LearningGoalDeriver::new(Arc::new(cognition.clone()), "eve")
        .unwrap()
        .reconcile(&records, 1_000)
        .unwrap();
    let options = |sources: Vec<AllowedSource>| EndogenousOptions {
        scope: ExecutionScope {
            subject_id: "eve".into(),
            access: ReadAccess::Internal,
            sources,
        },
        max_derivations: 4,
        timeout_ms: 30_000,
    };
    let user_only = vec![AllowedSource {
        kind: SourceKind::User,
        channel: "qq.goal".into(),
    }];
    let denied = EndogenousPlanner::new(Arc::new(cognition.clone()), options(user_only.clone()))
        .unwrap()
        .reconcile(2_000)
        .unwrap();
    assert!(denied.created_goal_ids.is_empty(), "未授权通道不进入反思");
    let mut both = user_only;
    both.push(AllowedSource {
        kind: SourceKind::Inference,
        channel: INTEREST_GOAL_CHANNEL.into(),
    });
    let report = EndogenousPlanner::new(Arc::new(cognition.clone()), options(both))
        .unwrap()
        .reconcile(3_000)
        .unwrap();
    assert_eq!(report.created_goal_ids.len(), 1);
    let child = &cognition.snapshot().unwrap().state.goals[&report.created_goal_ids[0]];
    assert_eq!(
        child.source.reference,
        learning_goal_id("eve", &records[0].id)
    );
    assert_eq!(child.status, GoalStatus::Ready);
    assert_eq!(child.visibility, Visibility::User("qq:alice".into()));
    kernel.stop_all().await.unwrap();
}
