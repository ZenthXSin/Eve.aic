//! 多步计划到面板契约的投影，以及本机操作者的撤销；只调用计划账本的读取、撤销与认知只读句柄。
//! 面板不能建议、确认计划或开始步骤；撤销只阻止未开始的步骤，已有证据保留。
use eve_cognition_api::CognitionReader;
use eve_plan_api::*;
use eve_web_panel_api::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn goal_plans(
    plans: &dyn PlanJournal,
    cognition: &dyn CognitionReader,
    goal_id: &str,
) -> PanelResult<GoalPlans> {
    let view = cognition.snapshot().map_err(|_| PanelError::Unavailable)?;
    let goal = view.state.goals.get(goal_id).ok_or(PanelError::NotFound)?;
    let snapshot = plans.snapshot().map_err(|_| PanelError::Unavailable)?;
    let mut list: Vec<&Plan> = snapshot
        .plans
        .iter()
        .filter(|plan| plan.binding.goal_id == goal_id)
        .collect();
    list.sort_by_key(|plan| std::cmp::Reverse(plan.created_at_ms));
    let mut proposals: Vec<&ProposalRecord> = snapshot
        .proposals
        .iter()
        .filter(|record| record.binding.goal_id == goal_id)
        .collect();
    proposals.sort_by_key(|record| std::cmp::Reverse(record.requested_at_ms));
    Ok(GoalPlans {
        goal_id: goal.id.clone(),
        goal_revision: goal.revision,
        plans: list.into_iter().map(plan_view).collect(),
        proposals: proposals
            .into_iter()
            .map(|record| {
                let (status, failure) = match &record.status {
                    ProposalStatus::Requested => ("requested", None),
                    ProposalStatus::Proposed { .. } => ("proposed", None),
                    ProposalStatus::Empty => ("empty", None),
                    ProposalStatus::Failed { failure } => {
                        ("failed", Some(proposal_failure(*failure)))
                    }
                };
                PlanProposalView {
                    id: record.id.clone(),
                    goal_revision: record.binding.goal_revision,
                    status,
                    failure,
                    requested_at_ms: record.requested_at_ms,
                    finished_at_ms: record.finished_at_ms,
                }
            })
            .collect(),
    })
}

/// 只接受读取时的计划修订；修订变化、计划已结束或有步骤执行中都不改动记录。
pub(crate) fn withdraw(
    plans: &dyn PlanJournal,
    plan_id: &str,
    revision: u64,
) -> PanelResult<PlanView> {
    let snapshot = plans.snapshot().map_err(|_| PanelError::Unavailable)?;
    let plan = snapshot
        .plans
        .iter()
        .find(|plan| plan.id == plan_id)
        .ok_or(PanelError::NotFound)?;
    if plan.revision != revision || !withdrawable(plan) {
        return Err(PanelError::Stale);
    }
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .ok_or(PanelError::Unavailable)?;
    match plans.withdraw(plan_id, revision, at_ms) {
        Ok(plan) => Ok(plan_view(&plan)),
        Err(
            PlanError::Conflict
            | PlanError::NotReady
            | PlanError::InvalidTransition
            | PlanError::InvalidInput,
        ) => Err(PanelError::Stale),
        Err(PlanError::NotFound) => Err(PanelError::NotFound),
        Err(_) => Err(PanelError::Unavailable),
    }
}

fn withdrawable(plan: &Plan) -> bool {
    plan.is_open()
        && !plan
            .steps
            .iter()
            .any(|step| step.status == StepStatus::Executing)
}

fn plan_view(plan: &Plan) -> PlanView {
    PlanView {
        id: plan.id.clone(),
        revision: plan.revision,
        goal_revision: plan.binding.goal_revision,
        status: match plan.status {
            PlanStatus::Proposed => "proposed",
            PlanStatus::Active => "active",
            PlanStatus::Completed => "completed",
            PlanStatus::Blocked => "blocked",
            PlanStatus::Stale => "stale",
            PlanStatus::Withdrawn => "withdrawn",
        },
        origin: match plan.origin {
            PlanOrigin::Operator => "operator",
            PlanOrigin::Model { .. } => "model",
        },
        created_at_ms: plan.created_at_ms,
        confirmed_at_ms: plan.confirmed_at_ms,
        withdrawn_at_ms: plan.withdrawn_at_ms,
        stale_at_ms: plan.stale_at_ms,
        withdrawable: withdrawable(plan),
        steps: plan
            .steps
            .iter()
            .map(|step| PlanStepView {
                id: step.spec.id.clone(),
                title: step.spec.title.clone(),
                capability: step.spec.capability.clone(),
                depends_on: step.spec.depends_on.clone(),
                status: match step.status {
                    StepStatus::Pending => "pending",
                    StepStatus::Executing => "executing",
                    StepStatus::Satisfied => "satisfied",
                    StepStatus::Failed => "failed",
                    StepStatus::Blocked => "blocked",
                    StepStatus::Invalidated => "invalidated",
                },
                max_attempts: step.spec.max_attempts,
                timeout_ms: step.spec.timeout_ms,
                attempts: step
                    .attempts
                    .iter()
                    .map(|attempt| PlanAttemptView {
                        number: attempt.number,
                        started_at_ms: attempt.started_at_ms,
                        finished_at_ms: attempt.finished_at_ms,
                        effect_met: attempt.effect_met,
                        failure: attempt.failure.map(step_failure),
                        evidence: attempt.evidence.as_ref().map(|evidence| PlanEvidenceView {
                            source_id: evidence.source_id.clone(),
                            sha256: evidence.sha256.clone(),
                            bytes: evidence.bytes,
                            verified_at_ms: evidence.verified_at_ms,
                        }),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn step_failure(failure: StepFailure) -> &'static str {
    match failure {
        StepFailure::CapabilityFailed => "capability_failed",
        StepFailure::EffectNotMet => "effect_not_met",
        StepFailure::Timeout => "timeout",
        StepFailure::Cancelled => "cancelled",
        StepFailure::Interrupted => "interrupted",
        StepFailure::BindingChanged => "binding_changed",
    }
}

fn proposal_failure(failure: ProposalFailure) -> &'static str {
    match failure {
        ProposalFailure::Provider => "provider",
        ProposalFailure::Timeout => "timeout",
        ProposalFailure::Cancelled => "cancelled",
        ProposalFailure::InvalidOutput => "invalid_output",
        ProposalFailure::Interrupted => "interrupted",
        ProposalFailure::BindingChanged => "binding_changed",
    }
}
