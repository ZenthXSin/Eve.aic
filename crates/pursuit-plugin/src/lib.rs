//! 推进判断的默认规则与停止执行。
//!
//! `RulePursuitPolicy` 按固定顺序给出判断：先看正在进行和可以开始的研究、实践，再看与用户的对话，
//! 最后才判断是否停止。`stop_goal` 用一次认知状态替换把仍在等待的目标标为已取消并追加停止事件；
//! 替换要么整体生效，要么不生效，没有中间状态需要在重启后补齐。
use eve_cognition_api::{
    CognitionAdmin, CognitionError, CognitiveEvent, CognitiveEventKind, CognitiveState, GoalStatus,
    MAX_RECORDS, Source, SourceKind,
};
use eve_pursuit_api::{
    Capability, Exchange, Outcome, POLICY_VERSION, PURSUIT_CHANNEL, PursuitFacts, PursuitPolicy,
    STOP_EVENT_PREFIX, STOP_EVENT_SCHEMA, Step, StopReason, StopRecord, Verdict, WaitReason,
};
use ring::digest::{Context, SHA256};

/// 默认规则；见模块文档与《学习目标的推进与停止》。
#[derive(Clone, Copy, Debug, Default)]
pub struct RulePursuitPolicy;

impl PursuitPolicy for RulePursuitPolicy {
    fn version(&self) -> &str {
        POLICY_VERSION
    }

    fn evaluate(&self, facts: &PursuitFacts) -> Verdict {
        let research = facts.research.as_ref();
        let practice = facts.practice.as_ref();
        if research.is_none() && practice.is_none() {
            return wait(WaitReason::NoCapability);
        }
        let running = |capability: Option<&Capability>| {
            capability.is_some_and(|capability| capability.current == Some(Outcome::Running))
        };
        if running(research) {
            return continuing(Step::Research, true);
        }
        if running(practice) {
            return continuing(Step::Practice, true);
        }
        // 当前版本还没做、还有次数的能力：预算够就开始，不够就等窗口重置；账本满了交给操作者。
        let mut capacity = false;
        if let Some(research) = research.filter(|research| fresh(research)) {
            match next(research) {
                Some(verdict) => return verdict.with(Step::Research),
                None => capacity = true,
            }
        }
        // 实践等研究对当前版本有了结局，或研究次数已经用完。
        let researched =
            research.is_none_or(|research| research.current.is_some() || research.spent());
        if let Some(practice) = practice.filter(|practice| researched && fresh(practice)) {
            match next(practice) {
                Some(verdict) => return verdict.with(Step::Practice),
                None => capacity = true,
            }
        }
        if facts.clarify == Exchange::Preparing {
            return Verdict::Clarify;
        }
        if facts.outreach == Exchange::Preparing {
            return continuing(Step::Invite, true);
        }
        if facts.clarify == Exchange::AwaitingReply || facts.outreach == Exchange::AwaitingReply {
            return wait(WaitReason::UserReply);
        }
        // 开启实践时由实践的结局决定当前版本是否卡住；只开启研究时看研究。
        let decisive = match practice {
            Some(practice) => practice.current,
            None => research.and_then(|research| research.current),
        };
        if matches!(
            decisive,
            Some(Outcome::Unsuccessful | Outcome::NotApplicable)
        ) && facts.clarify == Exchange::NotStarted
        {
            return Verdict::Clarify;
        }
        if practice.is_some_and(|practice| practice.succeeded_ever)
            && facts.outreach == Exchange::NotStarted
        {
            return continuing(Step::Invite, false);
        }
        if facts.open_follow_ups > 0 {
            return wait(WaitReason::FollowUp);
        }
        if capacity {
            return wait(WaitReason::Capacity);
        }
        if research.is_none_or(Capability::spent) && practice.is_none_or(Capability::spent) {
            return Verdict::Stop {
                reason: StopReason::LearningBudgetSpent,
            };
        }
        if !facts.room_for_input {
            return Verdict::Stop {
                reason: StopReason::InputClosed,
            };
        }
        wait(WaitReason::NewInput)
    }
}

fn wait(reason: WaitReason) -> Verdict {
    Verdict::Wait { reason }
}

fn continuing(step: Step, started: bool) -> Verdict {
    Verdict::Continue { step, started }
}

/// 当前版本还没开始，且这个目标还有次数。
fn fresh(capability: &Capability) -> bool {
    capability.current.is_none() && !capability.spent()
}

enum Next {
    Start,
    Budget,
}
impl Next {
    fn with(self, step: Step) -> Verdict {
        match self {
            Self::Start => continuing(step, false),
            Self::Budget => wait(WaitReason::AutonomyBudget),
        }
    }
}

/// 账本已满时为空。
fn next(capability: &Capability) -> Option<Next> {
    if capability.ledger_full {
        None
    } else if capability.admitted {
        Some(Next::Start)
    } else {
        Some(Next::Budget)
    }
}

/// 要停止的目标与停止时的事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopRequest {
    pub goal_id: String,
    /// 做判断时目标的版本；目标此后有变化就不停止。
    pub goal_revision: u64,
    pub reason: StopReason,
    pub research_runs: usize,
    pub practice_runs: usize,
    pub policy_version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopOutcome {
    Stopped,
    /// 目标已不在等待（例如用户撤回了兴趣），不改动。
    AlreadyClosed,
    /// 目标不存在或版本已变化，须重新判断。
    Changed,
    /// 认知状态正在被其他组件更新，或事件记录已满；未写入，下一轮再试。
    Deferred,
}

/// 同一目标的同一版本固定映射到一条停止事件。
pub fn stop_event_id(goal_id: &str, goal_revision: u64) -> String {
    let mut context = Context::new(&SHA256);
    for part in [
        STOP_EVENT_SCHEMA.as_bytes(),
        goal_id.as_bytes(),
        &goal_revision.to_be_bytes(),
    ] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part);
    }
    let hex: String = context
        .finish()
        .as_ref()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{STOP_EVENT_PREFIX}{hex}")
}

/// 把仍在等待、版本未变的目标标为已取消，并在同一次替换中追加停止事件。
/// 只交给可信宿主；由宿主决定哪些目标可以按推进判断停止。
pub fn stop_goal(
    admin: &dyn CognitionAdmin,
    request: &StopRequest,
    now_ms: u64,
) -> Result<StopOutcome, CognitionError> {
    let snapshot = admin.snapshot()?;
    let mut state = snapshot.state.clone();
    let Some(goal) = state.goals.get_mut(&request.goal_id) else {
        return Ok(StopOutcome::Changed);
    };
    if goal.status != GoalStatus::Waiting {
        return Ok(StopOutcome::AlreadyClosed);
    }
    if goal.revision != request.goal_revision {
        return Ok(StopOutcome::Changed);
    }
    if state.events.len() >= MAX_RECORDS {
        return Ok(StopOutcome::Deferred);
    }
    let id = stop_event_id(&request.goal_id, request.goal_revision);
    if state.events.iter().any(|event| event.id == id) {
        return Ok(StopOutcome::Changed);
    }
    goal.status = GoalStatus::Cancelled;
    goal.wait_reason = None;
    let visibility = goal.visibility.clone();
    let caused_by = state
        .events
        .iter()
        .rev()
        .find(|event| event.goal_id.as_deref() == Some(request.goal_id.as_str()))
        .map(|event| event.id.clone());
    let summary = serde_json::to_string(&StopRecord {
        schema: STOP_EVENT_SCHEMA.into(),
        reason: request.reason,
        goal_revision: request.goal_revision,
        research_runs: request.research_runs,
        practice_runs: request.practice_runs,
        policy_version: request.policy_version.clone(),
        user_instruction: false,
    })
    .map_err(|_| CognitionError::InvalidInput)?;
    state.events.push(CognitiveEvent {
        id,
        kind: CognitiveEventKind::StateChanged,
        source: Source {
            kind: SourceKind::Internal,
            channel: PURSUIT_CHANNEL.into(),
            reference: request.goal_id.clone(),
        },
        visibility,
        goal_id: Some(request.goal_id.clone()),
        caused_by,
        at_ms: now_ms,
        summary,
    });
    match admin.replace(snapshot.revision, state) {
        Ok(_) => Ok(StopOutcome::Stopped),
        Err(CognitionError::StaleRevision | CognitionError::LimitReached) => {
            Ok(StopOutcome::Deferred)
        }
        Err(error) => Err(error),
    }
}

/// 目标因推进判断停止时的记录；只认本通道、本格式且与目标版本一致的事件。
pub fn stop_record(state: &CognitiveState, goal_id: &str) -> Option<StopRecord> {
    state
        .events
        .iter()
        .filter(|event| {
            event.goal_id.as_deref() == Some(goal_id)
                && event.source.kind == SourceKind::Internal
                && event.source.channel == PURSUIT_CHANNEL
        })
        .filter_map(|event| {
            let record: StopRecord = serde_json::from_str(&event.summary).ok()?;
            (record.schema == STOP_EVENT_SCHEMA
                && event.id == stop_event_id(goal_id, record.goal_revision))
            .then_some(record)
        })
        .next_back()
}
