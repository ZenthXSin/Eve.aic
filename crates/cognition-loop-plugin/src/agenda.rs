use eve_cognition_api::{CognitionError, CognitiveSnapshot, GoalStatus, validate_text};
use eve_cognition_loop_api::{
    AgendaBlocker, AgendaEvaluation, AgendaExclusion, AgendaExclusionReason, DrivePolicy,
    GoalVerifier, LoopError, LoopOptions, LoopResult,
};
use std::collections::BTreeSet;

/// 从同一快照评估可执行议程，不派生目标、不修改状态、不调用模型。
/// 与循环使用完全相同的候选过滤和策略入口；不可见记录不出现在排除清单。
/// blocker 是当前宿主全局准入约束，ranked 仍解释约束解除后的候选顺序。
pub fn evaluate_agenda(
    snapshot: &CognitiveSnapshot,
    options: &LoopOptions,
    policy: &dyn DrivePolicy,
    verifier: &dyn GoalVerifier,
    submitted: u64,
    now_ms: u64,
) -> LoopResult<AgendaEvaluation> {
    options.validate()?;
    if snapshot.subject_id != options.scope.subject_id {
        return Err(CognitionError::SubjectMismatch.into());
    }
    if now_ms == 0 {
        return Err(LoopError::InvalidInput);
    }
    let blocker = if submitted >= u64::from(options.max_executions) {
        Some(AgendaBlocker::ExecutionLimitReached)
    } else if snapshot
        .state
        .goals
        .values()
        .any(|goal| goal.status == GoalStatus::Executing)
    {
        Some(AgendaBlocker::AlreadyExecuting)
    } else {
        None
    };
    let mut goals = Vec::new();
    let mut excluded = Vec::new();
    for goal in snapshot.state.goals.values() {
        if !goal.visibility.visible_to(&options.scope.access) {
            continue;
        }
        let reason = if !options.scope.permits(goal) {
            Some(AgendaExclusionReason::ScopeDenied)
        } else if goal.status != GoalStatus::Ready {
            Some(AgendaExclusionReason::NotReady)
        } else if goal.expires_at_ms.is_some_and(|end| now_ms >= end) {
            Some(AgendaExclusionReason::Expired)
        } else if goal.budget.validate().is_err() {
            Some(AgendaExclusionReason::InvalidBudget)
        } else if !verifier.supports(goal) {
            Some(AgendaExclusionReason::UnsupportedVerification)
        } else {
            None
        };
        if let Some(reason) = reason {
            excluded.push(AgendaExclusion {
                goal_id: goal.id.clone(),
                reason,
            });
        } else {
            goals.push(goal.clone());
        }
    }
    let ranked = if goals.is_empty() {
        Vec::new()
    } else {
        policy.rank_with_state(snapshot, &goals, now_ms)?
    };
    let mut unique = BTreeSet::new();
    for item in &ranked {
        if !goals.iter().any(|goal| goal.id == item.goal_id)
            || !unique.insert(&item.goal_id)
            || item.strength > 100
            || validate_text(&item.reason).is_err()
        {
            return Err(LoopError::InvalidInput);
        }
    }
    for goal in &goals {
        if !unique.contains(&goal.id) {
            excluded.push(AgendaExclusion {
                goal_id: goal.id.clone(),
                reason: AgendaExclusionReason::PolicyOmitted,
            });
        }
    }
    excluded.sort_by(|a, b| a.goal_id.cmp(&b.goal_id));
    Ok(AgendaEvaluation {
        evaluated_at_ms: now_ms,
        revision: snapshot.revision,
        ranked,
        excluded,
        blocker,
    })
}
