use eve_cognition_api::{
    CognitionError, CognitiveEvent, CognitiveEventKind, CognitiveSnapshot,
    FILE_OBSERVATION_CHANNEL, FileObservation, Goal, GoalStatus, GoalUserFeedback, SourceKind,
    validate_id,
};
use eve_cognition_loop_api::*;
use eve_control_api::CommitState;
use eve_llm_api::{ChatRole, ToolOutput};
use ring::digest::{SHA256, digest};
use std::{cmp::Ordering, collections::BTreeSet};

pub struct PriorityDrivePolicy;
impl DrivePolicy for PriorityDrivePolicy {
    fn rank(&self, goals: &[Goal], now_ms: u64) -> LoopResult<Vec<RankedGoal>> {
        let mut ranked: Vec<_> = goals
            .iter()
            .map(|goal| {
                let urgent = goal
                    .expires_at_ms
                    .is_some_and(|expires| expires.saturating_sub(now_ms) <= 60_000);
                RankedGoal {
                    goal_id: goal.id.clone(),
                    strength: goal
                        .priority
                        .saturating_add(if urgent { 10 } else { 0 })
                        .min(100),
                    reason: "未完成目标可推进；按优先级与临近期限排序".into(),
                }
            })
            .collect();
        ranked.sort_by(|a, b| b.strength.cmp(&a.strength).then(a.goal_id.cmp(&b.goal_id)));
        Ok(ranked)
    }
}

/// 根据同一持久快照中的来源证据评分；不写状态，也不扩展候选的执行权限。
/// 理由仅包含固定字段、分值及证据事件 ID 的 SHA-256，不复制正文或原始标识。
/// 同分、同优先级且初始时间相同时，按证据所属父目标 ID 排序；普通目标使用自身 ID。
/// 最后以实际目标 ID 消除剩余并列，保持父议程与反思执行的顺序一致。
#[derive(Clone, Copy, Debug, Default)]
pub struct EvidenceDrivePolicy;

impl DrivePolicy for EvidenceDrivePolicy {
    fn rank(&self, goals: &[Goal], now_ms: u64) -> LoopResult<Vec<RankedGoal>> {
        Ok(sort_scores(
            goals
                .iter()
                .map(|goal| score(goal, &goal.id, now_ms, None, None))
                .collect(),
        ))
    }

    fn rank_with_state(
        &self,
        snapshot: &CognitiveSnapshot,
        goals: &[Goal],
        now_ms: u64,
    ) -> LoopResult<Vec<RankedGoal>> {
        validate_id(&snapshot.subject_id)?;
        let mut unique = BTreeSet::new();
        let mut scored = Vec::with_capacity(goals.len());
        for goal in goals {
            if snapshot.state.goals.get(&goal.id) != Some(goal) || !unique.insert(&goal.id) {
                return Err(LoopError::InvalidInput);
            }
            goal.validate()?;
            let evidence_goal = evidence_goal(snapshot, goal)?;
            let initial = initial_evidence(snapshot, evidence_goal, now_ms);
            let fresh = current_input_evidence(snapshot, evidence_goal, now_ms)?;
            scored.push(score(goal, &evidence_goal.id, now_ms, initial, fresh));
        }
        Ok(sort_scores(scored))
    }
}

/// 只准入受信宿主范围内、与 Waiting 父目标当前修订严格匹配的反思子目标。
/// 不接受脱离快照的 rank，以免用旧父状态或仅凭来源引用授予执行资格。
#[derive(Clone, Debug)]
pub struct ReflectionDrivePolicy {
    parent_scope: ExecutionScope,
}

impl ReflectionDrivePolicy {
    pub fn new(parent_scope: ExecutionScope) -> LoopResult<Self> {
        parent_scope.validate()?;
        Ok(Self { parent_scope })
    }
}

impl DrivePolicy for ReflectionDrivePolicy {
    fn rank(&self, _goals: &[Goal], _now_ms: u64) -> LoopResult<Vec<RankedGoal>> {
        Err(LoopError::InvalidInput)
    }

    fn rank_with_state(
        &self,
        snapshot: &CognitiveSnapshot,
        goals: &[Goal],
        now_ms: u64,
    ) -> LoopResult<Vec<RankedGoal>> {
        if snapshot.subject_id != self.parent_scope.subject_id {
            return Err(CognitionError::SubjectMismatch.into());
        }
        let mut admitted = Vec::new();
        for goal in goals {
            if snapshot.state.goals.get(&goal.id) != Some(goal) {
                return Err(LoopError::InvalidInput);
            }
            if !goal.is_ready(now_ms)
                || goal.source.kind != SourceKind::Inference
                || goal.source.channel != "endogenous"
                || goal.verification != "reflection:v1"
            {
                continue;
            }
            let Some(parent) = snapshot.state.goals.get(&goal.source.reference) else {
                continue;
            };
            if parent.status != GoalStatus::Waiting
                || parent
                    .expires_at_ms
                    .is_some_and(|expires| now_ms >= expires)
                || !self.parent_scope.permits(parent)
                || parent.visibility != goal.visibility
            {
                continue;
            }
            if crate::current_reflection(&snapshot.state, &snapshot.subject_id, parent)?
                == Some(goal)
            {
                admitted.push(goal.clone());
            }
        }
        EvidenceDrivePolicy.rank_with_state(snapshot, &admitted, now_ms)
    }
}

/// 反思只能继承当前父修订的证据；普通目标仅使用自身的来源证据。
fn evidence_goal<'a>(snapshot: &'a CognitiveSnapshot, goal: &'a Goal) -> LoopResult<&'a Goal> {
    if goal.source.kind == SourceKind::Inference
        && goal.source.channel == "endogenous"
        && goal.verification == "reflection:v1"
        && let Some(parent) = snapshot.state.goals.get(&goal.source.reference)
        && parent.visibility == goal.visibility
        && crate::current_reflection(&snapshot.state, &snapshot.subject_id, parent)? == Some(goal)
    {
        return Ok(parent);
    }
    Ok(goal)
}

fn initial_evidence<'a>(
    snapshot: &'a CognitiveSnapshot,
    goal: &Goal,
    now_ms: u64,
) -> Option<&'a CognitiveEvent> {
    if !matches!(
        goal.source.kind,
        SourceKind::User | SourceKind::Environment | SourceKind::Tool
    ) {
        return None;
    }
    snapshot
        .state
        .events
        .iter()
        .filter(|event| {
            event.kind == CognitiveEventKind::ExternalInput
                && event.source == goal.source
                && event.goal_id.as_deref() == Some(goal.id.as_str())
                && event.visibility == goal.visibility
                && event.at_ms != 0
                && event.at_ms <= now_ms
                && validate_id(&event.id).is_ok()
        })
        .min_by(|left, right| {
            left.at_ms
                .cmp(&right.at_ms)
                .then_with(|| left.id.cmp(&right.id))
        })
}

/// 新信息加分只承认与当前父修订 wait_reason 完全一致的合法反馈或读取回执。
fn current_input_evidence<'a>(
    snapshot: &'a CognitiveSnapshot,
    parent: &Goal,
    now_ms: u64,
) -> LoopResult<Option<&'a CognitiveEvent>> {
    if parent.source.kind != SourceKind::User || parent.verification != "user-goal:v1" {
        return Ok(None);
    }
    let Some(summary) = parent.wait_reason.as_deref() else {
        return Ok(None);
    };
    let feedback = GoalUserFeedback::parse(summary).ok().filter(|feedback| {
        feedback.goal_id == parent.id && feedback.goal_revision == parent.revision
    });
    let observation = FileObservation::parse(summary).ok().filter(|observation| {
        observation.goal_id == parent.id && observation.goal_revision == parent.revision
    });
    let observation_id = observation
        .as_ref()
        .map(|observation| observation.event_id(&snapshot.subject_id))
        .transpose()?;
    let mut matched = None;
    for event in &snapshot.state.events {
        if event.kind != CognitiveEventKind::ExternalInput
            || event.goal_id.as_deref() != Some(parent.id.as_str())
            || event.visibility != parent.visibility
            || event.summary != summary
            || event.at_ms == 0
            || event.at_ms > now_ms
            || event.source.validate().is_err()
            || validate_id(&event.id).is_err()
        {
            continue;
        }
        let is_feedback = feedback.as_ref().is_some_and(|feedback| {
            event.source.kind == SourceKind::User
                && event.source.channel != parent.source.channel
                && event.source.reference == feedback.feedback_id
                && event.id == feedback.feedback_id
        });
        let is_observation = observation.as_ref().is_some_and(|observation| {
            event.source.kind == SourceKind::Environment
                && event.source.channel == FILE_OBSERVATION_CHANNEL
                && event.source.reference == observation.observation_source_id
                && event.at_ms == observation.observed_at_ms
                && observation_id.as_deref() == Some(event.id.as_str())
        });
        if (is_feedback || is_observation) && matched.replace(event).is_some() {
            return Err(CognitionError::InvalidInput.into());
        }
    }
    Ok(matched)
}

struct ScoredGoal {
    ranked: RankedGoal,
    priority: u8,
    initial_at_ms: Option<u64>,
    order_goal_id: String,
}

fn score(
    goal: &Goal,
    order_goal_id: &str,
    now_ms: u64,
    initial: Option<&CognitiveEvent>,
    fresh: Option<&CognitiveEvent>,
) -> ScoredGoal {
    let base = goal.priority.min(100);
    let deadline: u8 = match goal
        .expires_at_ms
        .and_then(|expires| expires.checked_sub(now_ms))
    {
        Some(1..=60_000) => 15,
        Some(60_001..=3_600_000) => 8,
        _ => 0,
    };
    let age = initial.map_or(0, |event| {
        (now_ms.saturating_sub(event.at_ms) / 21_600_000).min(10) as u8
    });
    let fresh_strength: u8 = match fresh.and_then(|event| now_ms.checked_sub(event.at_ms)) {
        Some(0..=300_000) => 15,
        Some(300_001..=3_600_000) => 8,
        _ => 0,
    };
    let strength =
        (u16::from(base) + u16::from(deadline) + u16::from(age) + u16::from(fresh_strength))
            .min(100) as u8;
    let reason = format!(
        "证据议程评分：base={base}；deadline={deadline}；age={age}；fresh={fresh_strength}；total={strength}；initial_evidence_id_sha256={}；fresh_evidence_id_sha256={}；同分依次按原优先级、初始时间、证据所属父目标ID（普通目标为自身ID）、实际目标ID排序",
        evidence_digest(initial),
        evidence_digest(fresh),
    );
    ScoredGoal {
        ranked: RankedGoal {
            goal_id: goal.id.clone(),
            strength,
            reason,
        },
        priority: goal.priority,
        initial_at_ms: initial.map(|event| event.at_ms),
        order_goal_id: order_goal_id.into(),
    }
}

fn evidence_digest(event: Option<&CognitiveEvent>) -> String {
    event.map_or_else(
        || "none".into(),
        |event| {
            digest(&SHA256, event.id.as_bytes())
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        },
    )
}

fn sort_scores(mut scored: Vec<ScoredGoal>) -> Vec<RankedGoal> {
    scored.sort_by(|left, right| {
        right
            .ranked
            .strength
            .cmp(&left.ranked.strength)
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| match (left.initial_at_ms, right.initial_at_ms) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| left.order_goal_id.cmp(&right.order_goal_id))
            .then_with(|| left.ranked.goal_id.cmp(&right.ranked.goal_id))
    });
    scored.into_iter().map(|score| score.ranked).collect()
}

/// 第一项受信验证规则：echo:<预期文本>；不把模型回复当成执行证据。
pub struct EchoReceiptVerifier;
impl GoalVerifier for EchoReceiptVerifier {
    fn supports(&self, goal: &Goal) -> bool {
        goal.verification
            .strip_prefix("echo:")
            .is_some_and(|text| !text.is_empty())
            && goal.stop_condition == "single-attempt"
    }
    fn verify(&self, goal: &Goal, report: &GoalExecutionReport) -> LoopResult<bool> {
        let Some(expected) = goal.verification.strip_prefix("echo:") else {
            return Ok(false);
        };
        let run = &report.control.run;
        if !self.supports(goal)
            || run.commit != CommitState::Completed
            || run.turn_id.is_none()
            || run.started_tools != Some(1)
            || report.usage.admitted_tool_calls != 1
            || run.tool_results.len() != 1
        {
            return Ok(false);
        }
        let Some(transcript) = &run.transcript else {
            return Ok(false);
        };
        let calls: Vec<_> = transcript
            .iter()
            .filter(|message| message.role == ChatRole::Assistant)
            .flat_map(|message| &message.tool_calls)
            .collect();
        let receipts: Vec<_> = transcript
            .iter()
            .filter(|message| message.role == ChatRole::Tool)
            .flat_map(|message| &message.tool_results)
            .collect();
        if calls.len() != 1 || receipts.len() != 1 {
            return Ok(false);
        }
        let call = calls[0];
        let receipt = &run.tool_results[0];
        Ok(call.name == "echo"
            && call.arguments.as_object().is_some_and(|args| {
                args.len() == 1
                    && args.get("text").and_then(|value| value.as_str()) == Some(expected)
            })
            && receipt.call_id == call.id
            && receipts[0] == receipt
            && matches!(&receipt.output, ToolOutput::Success(value)
                if value.as_object().is_some_and(|object| object.len() == 1)
                    && value.get("echo").and_then(|value| value.as_str()) == Some(expected)))
    }
}
