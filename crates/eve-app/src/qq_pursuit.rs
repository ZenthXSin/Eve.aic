//! 兴趣学习目标的推进判断：按研究、实践、请求澄清、主动交流与兴趣记录，判断接下来继续、请求澄清、
//! 等待还是停止。
//!
//! 判断只读各账本，交给议程上下文与 /interests，让 Eve 被问起时如实说出接下来会做什么、在等什么。
//! 开启 `--learning-stop` 时，后台把判断为停止的目标标为已取消并记下原因（目标的停止条件
//! “学习预算耗尽”）；停止是一次认知状态替换，失败或冲突时下一轮重新判断，不写其他状态。
use crate::{AppError, qq_budget};
use eve_clarify_api::{ClarifyAdmin, ClarifySnapshot, ClarifyStatus};
use eve_cognition_api::{
    CognitionAdmin, CognitiveSnapshot, Goal, GoalStatus, SourceKind, Visibility,
};
use eve_interest_api::{
    INTEREST_GOAL_CHANNEL, INTEREST_GOAL_VERIFICATION, InterestAdmin, InterestRecord,
    InterestStatus, MAX_STATEMENTS,
};
use eve_interest_plugin::synced_interest_revision;
use eve_knowledge_api::{KnowledgeAdmin, KnowledgeSnapshot, KnowledgeStatus, RunStatus};
use eve_memory_api::MemoryScope;
use eve_outreach_api::{
    InvitationStatus, OutreachAdmin, OutreachSnapshot, REQUEST_GOAL_CHANNEL, RESPONSE_WINDOW_MS,
    RequestMarker,
};
use eve_practice_api::{PracticeAdmin, PracticeSnapshot, PracticeStatus};
use eve_pursuit_api::{
    Capability, Exchange, Outcome, PursuitFacts, PursuitPolicy, StopRecord, Verdict,
};
use eve_pursuit_plugin::{StopOutcome, StopRequest, stop_goal, stop_record};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

/// 后台核对的间隔；判断只读账本，不请求模型。
const INTERVAL: Duration = Duration::from_millis(500);

fn now_ms() -> Result<u64, AppError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .filter(|now| *now > 0)
        .ok_or_else(|| "系统时间不可用".into())
}

fn scope(owner: &str) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: owner.into(),
        user_id: owner.into(),
    }
}

/// 兴趣派生的学习目标及其所属用户；其他目标不做推进判断。
pub(crate) fn learning_goal(goal: &Goal) -> Option<&str> {
    let Visibility::User(owner) = &goal.visibility else {
        return None;
    };
    (goal.source.kind == SourceKind::Inference
        && goal.source.channel == INTEREST_GOAL_CHANNEL
        && goal.verification == INTEREST_GOAL_VERIFICATION)
        .then_some(owner.as_str())
}

/// 学习目标接下来的情况：仍在等待的给出判断；已因推进判断停止的给出停止记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Next {
    Judged(Verdict),
    Stopped(StopRecord),
}

/// 交给对话的形式：判断原样给出；已停止的写成 `{"verdict":"stopped","reason":…}`。
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum NextView {
    Judged(Verdict),
    Stopped {
        verdict: &'static str,
        reason: eve_pursuit_api::StopReason,
    },
}
impl From<&Next> for NextView {
    fn from(next: &Next) -> Self {
        match next {
            Next::Judged(verdict) => Self::Judged(*verdict),
            Next::Stopped(record) => Self::Stopped {
                verdict: "stopped",
                reason: record.reason,
            },
        }
    }
}

/// 给用户看的一句话说明。
pub(crate) fn describe(next: &Next) -> &'static str {
    use eve_pursuit_api::{Step, StopReason, WaitReason};
    match next {
        Next::Judged(Verdict::Continue { step, started }) => match (step, started) {
            (Step::Research, false) => "准备查资料",
            (Step::Research, true) => "正在查资料",
            (Step::Practice, false) => "准备动手实践",
            (Step::Practice, true) => "正在动手实践",
            (Step::Invite, false) => "准备告诉你学到的东西",
            (Step::Invite, true) => "正在找合适的时机告诉你学到的东西",
        },
        Next::Judged(Verdict::Clarify) => "卡住了，准备问你",
        Next::Judged(Verdict::Wait { reason }) => match reason {
            WaitReason::AutonomyBudget => "等自主运行预算重置",
            WaitReason::UserReply => "等你回复",
            WaitReason::FollowUp => "在做你提的后续创作",
            WaitReason::Capacity => "研究或实践记录已满，需要操作者处理",
            WaitReason::NewInput => "能做的都做了，等你提到新的想法或情况",
            WaitReason::NoCapability => "没有开启研究或实践",
        },
        Next::Judged(Verdict::Stop { reason }) => match reason {
            StopReason::LearningBudgetSpent => "学习次数已用完，不再推进",
            StopReason::InputClosed => "记不下新的想法了，不再推进",
        },
        Next::Stopped(record) => match record.reason {
            StopReason::LearningBudgetSpent => "已停止：学习次数已用完",
            StopReason::InputClosed => "已停止：记不下新的想法了",
        },
    }
}

/// 一个学习目标的判断与当时的事实。
pub(crate) struct Evaluation {
    pub(crate) goal_id: String,
    pub(crate) goal_revision: u64,
    pub(crate) facts: PursuitFacts,
    pub(crate) verdict: Verdict,
}

pub(crate) struct Pursuit {
    /// 认知服务启动后才可用；之前不做判断。
    pub(crate) cognition: Arc<OnceLock<Arc<dyn CognitionAdmin>>>,
    pub(crate) interests: Arc<dyn InterestAdmin>,
    pub(crate) knowledge: Option<Arc<dyn KnowledgeAdmin>>,
    pub(crate) practice: Option<Arc<dyn PracticeAdmin>>,
    pub(crate) clarify: Option<Arc<dyn ClarifyAdmin>>,
    pub(crate) outreach: Option<Arc<dyn OutreachAdmin>>,
    pub(crate) gate: qq_budget::Gate,
    pub(crate) policy: Arc<dyn PursuitPolicy>,
}

/// 一次读取的各账本快照；同一轮判断都基于它们。各账本不是同一事务快照。
struct Ledgers {
    cognition: CognitiveSnapshot,
    knowledge: Option<KnowledgeSnapshot>,
    practice: Option<PracticeSnapshot>,
    clarify: Option<ClarifySnapshot>,
    outreach: Option<OutreachSnapshot>,
    research_admitted: bool,
    practice_admitted: bool,
    now_ms: u64,
}

impl Pursuit {
    fn admin(&self) -> Option<&Arc<dyn CognitionAdmin>> {
        self.cognition.get()
    }

    fn ledgers(&self) -> Result<Option<Ledgers>, AppError> {
        let Some(admin) = self.admin() else {
            return Ok(None);
        };
        Ok(Some(Ledgers {
            cognition: admin.snapshot().map_err(|_| "推进判断无法读取学习目标")?,
            knowledge: match &self.knowledge {
                Some(knowledge) => Some(knowledge.snapshot()?),
                None => None,
            },
            practice: match &self.practice {
                Some(practice) => Some(practice.snapshot()?),
                None => None,
            },
            clarify: match &self.clarify {
                Some(clarify) => Some(clarify.snapshot().map_err(|error| error.to_string())?),
                None => None,
            },
            outreach: match &self.outreach {
                Some(outreach) => Some(outreach.snapshot()?),
                None => None,
            },
            research_admitted: self.gate.admits(qq_budget::RESEARCH_UNITS)?,
            practice_admitted: self.gate.admits(qq_budget::PRACTICE_UNITS)?,
            now_ms: now_ms()?,
        }))
    }

    /// 这位用户的这些兴趣对应的学习目标接下来的情况，按目标 ID；认知服务尚未启动时为空。
    pub(crate) fn next_for(
        &self,
        interests: &[&InterestRecord],
    ) -> Result<BTreeMap<String, Next>, AppError> {
        let Some(ledgers) = self.ledgers()? else {
            return Ok(BTreeMap::new());
        };
        let mut found = BTreeMap::new();
        for interest in interests {
            let id = eve_interest_plugin::learning_goal_id(crate::qq_agenda::SUBJECT, &interest.id);
            let Some(goal) = ledgers.cognition.state.goals.get(&id) else {
                continue;
            };
            if learning_goal(goal).is_none() {
                continue;
            }
            let next = match goal.status {
                GoalStatus::Waiting => self
                    .evaluate(&ledgers, goal, interest)
                    .map(|evaluation| Next::Judged(evaluation.verdict)),
                GoalStatus::Cancelled => {
                    stop_record(&ledgers.cognition.state, &goal.id).map(Next::Stopped)
                }
                _ => None,
            };
            if let Some(next) = next {
                found.insert(id, next);
            }
        }
        Ok(found)
    }

    /// 所有仍在等待、判断为停止的学习目标。
    fn stops(&self) -> Result<Vec<Evaluation>, AppError> {
        let Some(ledgers) = self.ledgers()? else {
            return Ok(Vec::new());
        };
        let mut owners: BTreeMap<&str, Vec<&Goal>> = BTreeMap::new();
        for goal in ledgers.cognition.state.goals.values() {
            if goal.status == GoalStatus::Waiting
                && let Some(owner) = learning_goal(goal)
            {
                owners.entry(owner).or_default().push(goal);
            }
        }
        let mut stops = Vec::new();
        for (owner, goals) in owners {
            let interests = self.interests.snapshot(&scope(owner))?;
            for goal in goals {
                let Some(interest) = interests
                    .interests
                    .iter()
                    .find(|interest| interest.id == goal.source.reference)
                else {
                    continue;
                };
                if let Some(evaluation) = self.evaluate(&ledgers, goal, interest)
                    && matches!(evaluation.verdict, Verdict::Stop { .. })
                {
                    stops.push(evaluation);
                }
            }
        }
        Ok(stops)
    }

    /// 只判断兴趣仍有效的学习目标；兴趣已撤回时由兴趣同步取消目标，不在这里判断。
    fn evaluate(
        &self,
        ledgers: &Ledgers,
        goal: &Goal,
        interest: &InterestRecord,
    ) -> Option<Evaluation> {
        if goal.status != GoalStatus::Waiting
            || learning_goal(goal).is_none()
            || interest.status != InterestStatus::Active
            || goal.source.reference != interest.id
        {
            return None;
        }
        let facts = facts(ledgers, goal, interest);
        let verdict = self.policy.evaluate(&facts);
        Some(Evaluation {
            goal_id: goal.id.clone(),
            goal_revision: goal.revision,
            facts,
            verdict,
        })
    }
}

fn facts(ledgers: &Ledgers, goal: &Goal, interest: &InterestRecord) -> PursuitFacts {
    let research = ledgers.knowledge.as_ref().map(|knowledge| {
        let runs: Vec<_> = knowledge.runs_for(&goal.id).collect();
        let current = runs
            .iter()
            .filter(|run| run.topic.goal_revision == goal.revision)
            .max_by_key(|run| (run.started_at_ms, run.id.clone()))
            .map(|run| match run.status {
                RunStatus::Running => Outcome::Running,
                RunStatus::Completed => {
                    if knowledge.entries_for(&goal.id).any(|entry| {
                        entry.run_id == run.id && entry.status == KnowledgeStatus::SourceQuoted
                    }) {
                        Outcome::Succeeded
                    } else {
                        Outcome::Unsuccessful
                    }
                }
                RunStatus::Failed(_) => Outcome::Failed,
                RunStatus::Interrupted => Outcome::Interrupted,
            });
        Capability {
            runs_used: runs.len(),
            max_runs: eve_knowledge_api::MAX_RUNS_PER_GOAL,
            ledger_full: knowledge.runs.len() >= eve_knowledge_api::MAX_RUNS,
            current,
            succeeded_ever: knowledge
                .entries_for(&goal.id)
                .any(|entry| entry.status == KnowledgeStatus::SourceQuoted),
            admitted: ledgers.research_admitted,
        }
    });
    let practice = ledgers.practice.as_ref().map(|practice| {
        let runs: Vec<_> = practice.runs_for(&goal.id).collect();
        let current = runs
            .iter()
            .filter(|run| run.task.goal_revision == goal.revision)
            .max_by_key(|run| (run.started_at_ms, run.id.clone()))
            .map(|run| match run.status {
                PracticeStatus::Running => Outcome::Running,
                PracticeStatus::Verified => Outcome::Succeeded,
                PracticeStatus::Unverified => Outcome::Unsuccessful,
                PracticeStatus::NotApplicable => Outcome::NotApplicable,
                PracticeStatus::Failed(_) => Outcome::Failed,
                PracticeStatus::Interrupted => Outcome::Interrupted,
            });
        Capability {
            runs_used: runs.len(),
            max_runs: eve_practice_api::MAX_RUNS_PER_GOAL,
            ledger_full: practice.runs.len() >= eve_practice_api::MAX_RUNS,
            current,
            succeeded_ever: runs
                .iter()
                .any(|run| run.status == PracticeStatus::Verified),
            admitted: ledgers.practice_admitted,
        }
    });
    let clarify = match &ledgers.clarify {
        None => Exchange::Disabled,
        Some(clarify) => match clarify.for_goal(&goal.id, goal.revision) {
            None => Exchange::NotStarted,
            Some(request) => match request.status {
                status if status.is_open() => Exchange::Preparing,
                ClarifyStatus::Delivered => Exchange::AwaitingReply,
                _ => Exchange::Closed,
            },
        },
    };
    let outreach = match &ledgers.outreach {
        None => Exchange::Disabled,
        Some(outreach) => match outreach.for_goal(&goal.id) {
            None => Exchange::NotStarted,
            Some(invitation) if invitation.status.is_open() => Exchange::Preparing,
            Some(invitation)
                if invitation.status == InvitationStatus::Delivered
                    && invitation.feedback().is_none()
                    && (invitation.responding()
                        || (invitation.listening()
                            && invitation.delivered_at_ms.is_some_and(|at| {
                                ledgers.now_ms.saturating_sub(at) <= RESPONSE_WINDOW_MS
                            }))) =>
            {
                Exchange::AwaitingReply
            }
            Some(_) => Exchange::Closed,
        },
    };
    let open_follow_ups = ledgers
        .cognition
        .state
        .goals
        .values()
        .filter(|follow_up| {
            follow_up.source.channel == REQUEST_GOAL_CHANNEL
                && matches!(
                    follow_up.status,
                    GoalStatus::Ready | GoalStatus::Waiting | GoalStatus::Executing
                )
                && follow_up
                    .wait_reason
                    .as_deref()
                    .and_then(RequestMarker::parse)
                    .is_some_and(|marker| marker.learning_goal_id == goal.id)
        })
        .count();
    // 普通陈述至多占用 MAX_STATEMENTS - 1 个位置；目标还没同步到兴趣最新修订时，同步后仍会有新版本。
    let room_for_input = interest.statements.len() < MAX_STATEMENTS - 1
        || synced_interest_revision(goal, interest) != Some(interest.revision);
    PursuitFacts {
        research,
        practice,
        clarify,
        outreach,
        open_follow_ups,
        room_for_input,
    }
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(pursuit: Arc<Pursuit>) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放；宿主随后关闭通道，不假装后台仍正常。
            let result = run(pursuit, activated, stopped).await;
            let _ = finished_sender.send(true);
            result
        });
        Self {
            active,
            stop,
            finished,
            task,
        }
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "学习目标停止后台异常；已停止准入")?
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn run(
    pursuit: Arc<Pursuit>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), AppError> {
    loop {
        if *active.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            changed = active.changed() => if changed.is_err() { return Ok(()); },
        }
    }
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        settle(&pursuit)?;
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            _ = tokio::time::sleep(INTERVAL) => {},
        }
    }
}

/// 把判断为停止的目标标为已取消；目标此后有变化或认知状态正忙时留到下一轮重新判断。
fn settle(pursuit: &Pursuit) -> Result<(), AppError> {
    let Some(admin) = pursuit.admin() else {
        return Ok(());
    };
    for evaluation in pursuit.stops()? {
        let Verdict::Stop { reason } = evaluation.verdict else {
            continue;
        };
        let request = StopRequest {
            goal_id: evaluation.goal_id,
            goal_revision: evaluation.goal_revision,
            reason,
            research_runs: evaluation
                .facts
                .research
                .as_ref()
                .map_or(0, |research| research.runs_used),
            practice_runs: evaluation
                .facts
                .practice
                .as_ref()
                .map_or(0, |practice| practice.runs_used),
            policy_version: pursuit.policy.version().into(),
        };
        match stop_goal(&**admin, &request, now_ms()?) {
            Ok(
                StopOutcome::Stopped
                | StopOutcome::AlreadyClosed
                | StopOutcome::Changed
                | StopOutcome::Deferred,
            ) => {}
            Err(error) => return Err(format!("学习目标停止无法写入认知状态：{error}").into()),
        }
    }
    Ok(())
}
