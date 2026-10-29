//! QQ 宿主的请求澄清：自主学习目标在当前修订卡住时（研究没有找到可引用的资料、实践尝试用尽仍未验证，
//! 或运行环境不适用），撰写一个具体问题，在用户下次私聊时先判断时机，合适才随回复附带；
//! 用户之后的下一轮私聊记为回答。回答作为普通交互进入记忆，由兴趣观察照常处理，不直接改写目标。
//!
//! 卡住只按账本中已结束的记录确定性判定；每个目标修订至多请求一次。只附带在用户自己发来的私聊回复中，
//! 不主动私聊；与主动交流共用通道的投递点时，邀请优先判断。
use crate::{AppError, qq_budget, qq_practice};
use eve_clarify_api::{
    ANSWER_WINDOW_MS, Answer, CancelReason, Clarification, ClarificationComposer, ClarifyAdmin,
    ClarifyError, ClarifyFailure, ClarifyPolicy, ClarifyStatus, ComposeRequest, DeliveryResult,
    Fact, FactKind, MAX_ANSWER_BYTES, MAX_FACT_BYTES, MAX_JUDGEMENTS, Stall, StallKind, prefix,
};
use eve_cognition_api::{CognitionAdmin, GoalStatus};
use eve_interest_api::{INTEREST_GOAL_CHANNEL, InterestAdmin, InterestStatus, StatementKind};
use eve_knowledge_api::{KnowledgeAdmin, KnowledgeStatus, RunStatus};
use eve_memory_api::{EvidenceSource, MemoryAdmin, MemoryScope};
use eve_outreach_api::{JudgeRequest, MAX_MOMENT_BYTES, TimingJudge, Verdict};
use eve_plugin_api::{PluginError, PluginResult};
use eve_practice_api::{PracticeAdmin, PracticeStatus};
use eve_qqbot_plugin::{
    QQ_OUTREACH_JUDGE_TIMEOUT, QqCommandHandler, QqCommandInput, QqOutreach, QqOutreachFuture,
    QqOutreachMessage, QqOutreachMoment, QqOutreachPush, QqOutreachResult,
};
use eve_session_api::SessionKey;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

const COMPOSE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_QUOTES: usize = 3;
const DISABLED: &str = "请求澄清未启用。";
const HELP: &str = "用法：/clarify 查看 Eve 卡住时问你的问题；/clarify off 请 Eve 不再附带这类问题，/clarify on 恢复。";
/// 本模块分配的问题 ID 前缀；与主动交流共用通道时据此分流投递结果。
pub(crate) const ID_PREFIX: &str = "clarify-";

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

fn clarify_error(error: ClarifyError) -> AppError {
    error.to_string().into()
}

/// 后台依赖的服务；研究与实践至少开启一项。
pub(crate) struct Services {
    pub(crate) clarify: Arc<dyn ClarifyAdmin>,
    pub(crate) cognition: Arc<dyn CognitionAdmin>,
    pub(crate) interests: Arc<dyn InterestAdmin>,
    pub(crate) memory: Arc<dyn MemoryAdmin>,
    pub(crate) knowledge: Option<Arc<dyn KnowledgeAdmin>>,
    pub(crate) practice: Option<Arc<dyn PracticeAdmin>>,
    pub(crate) composer: Arc<dyn ClarificationComposer>,
    pub(crate) gate: qq_budget::Gate,
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(services: Services) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放；宿主随后关闭通道，不假装后台仍正常。
            let result = run(services, activated, stopped).await;
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
            .map_err(|_| "请求澄清后台异常；已停止准入")?
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
    services: Services,
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
    let mut asking = true;
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        cancel_stale(&services)?;
        listen(&services)?;
        // 余额不够一次撰写时不开始；卡住的目标留到预算窗口重置后再问。
        if asking
            && services.gate.admits(qq_budget::SINGLE_UNIT)?
            && let Some(candidate) = stalled(&services)?
        {
            match services.clarify.begin(
                &candidate.owner,
                &candidate.goal_id,
                candidate.goal_revision,
                candidate.stall,
                candidate.facts,
                services.composer.version(),
                now_ms()?,
            ) {
                Ok(Some(request)) => compose(&services, &request, &mut stopped).await?,
                Ok(None) => {}
                // 容量满保留原记录：不再新增问题，已有问题照常投递与查看。
                Err(ClarifyError::LimitReached) => asking = false,
                Err(error) => return Err(clarify_error(error)),
            }
            continue;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
}

/// 送达前目标已结束或有了新修订：问题过时，取消。
fn cancel_stale(services: &Services) -> Result<(), AppError> {
    let snapshot = services.clarify.snapshot().map_err(clarify_error)?;
    let cognition = services
        .cognition
        .snapshot()
        .map_err(|_| "请求澄清无法读取学习目标")?;
    for request in snapshot
        .requests
        .iter()
        .filter(|request| request.status == ClarifyStatus::Pending && !request.judging())
    {
        let reason = match cognition.state.goals.get(&request.goal_id) {
            Some(goal) if matches!(goal.status, GoalStatus::Waiting) => {
                if goal.revision == request.goal_revision {
                    continue;
                }
                CancelReason::GoalChanged
            }
            _ => CancelReason::GoalClosed,
        };
        services
            .clarify
            .cancel(&request.id, now_ms()?, reason)
            .map_err(clarify_error)?;
    }
    Ok(())
}

/// 已送达的问题：送达后用户的下一轮私聊记为回答；回答窗口过后记为过期。
fn listen(services: &Services) -> Result<(), AppError> {
    let snapshot = services.clarify.snapshot().map_err(clarify_error)?;
    let now = now_ms()?;
    for request in snapshot.listening() {
        let Some(delivered) = request.delivered_at_ms else {
            continue;
        };
        let memory = services.memory.reader(scope(&request.owner))?.snapshot()?;
        let next = memory
            .evidence
            .iter()
            .filter_map(|evidence| match &evidence.source {
                EvidenceSource::CompletedInteraction {
                    message_id,
                    user_text,
                    ..
                } => Some((evidence, message_id, user_text)),
                EvidenceSource::UserStatement { .. } => None,
            })
            .filter(|(evidence, message_id, user_text)| {
                evidence.at_ms >= delivered
                    && evidence.at_ms - delivered <= ANSWER_WINDOW_MS
                    && !request.carried_by(message_id)
                    && eve_clarify_api::validate_id(message_id).is_ok()
                    && eve_clarify_api::validate_id(&evidence.id).is_ok()
                    && !prefix(user_text, MAX_ANSWER_BYTES).is_empty()
            })
            .min_by_key(|(evidence, _, _)| (evidence.at_ms, evidence.id.clone()));
        if let Some((evidence, message_id, user_text)) = next {
            services
                .clarify
                .record_answer(
                    &request.id,
                    Answer {
                        message_id: message_id.clone(),
                        evidence_id: evidence.id.clone(),
                        at_ms: evidence.at_ms,
                        text: prefix(user_text, MAX_ANSWER_BYTES).into(),
                    },
                )
                .map_err(clarify_error)?;
        } else if now.saturating_sub(delivered) > ANSWER_WINDOW_MS {
            services
                .clarify
                .expire(&request.id, now)
                .map_err(clarify_error)?;
        }
    }
    Ok(())
}

struct Candidate {
    owner: String,
    goal_id: String,
    goal_revision: u64,
    stall: Stall,
    facts: Vec<Fact>,
}

/// 第一个在当前修订卡住、还没有问过的兴趣学习目标。开启实践时只看实践：研究没找到资料时实践仍会尝试，
/// 先等实践的结局；只开启研究时看研究是否找到了可逐字引用的资料。
fn stalled(services: &Services) -> Result<Option<Candidate>, AppError> {
    let cognition = services
        .cognition
        .snapshot()
        .map_err(|_| "请求澄清无法读取学习目标")?;
    let clarify = services.clarify.snapshot().map_err(clarify_error)?;
    let practice = match &services.practice {
        Some(practice) => Some(practice.snapshot()?),
        None => None,
    };
    let knowledge = match &services.knowledge {
        Some(knowledge) => Some(knowledge.snapshot()?),
        None => None,
    };
    let mut goals: Vec<_> = cognition
        .state
        .goals
        .values()
        .filter(|goal| goal.source.channel == INTEREST_GOAL_CHANNEL)
        .filter_map(|goal| qq_practice::eligible_owner(goal).map(|owner| (goal, owner)))
        .filter(|(goal, _)| clarify.for_goal(&goal.id, goal.revision).is_none())
        .collect();
    goals.sort_by_key(|(goal, _)| goal.id.clone());
    for (goal, owner) in goals {
        let mut facts = Vec::new();
        let stall = if let Some(practice) = &practice {
            let Some(run) = practice
                .runs_for(&goal.id)
                .filter(|run| run.task.goal_revision == goal.revision)
                .filter(|run| {
                    matches!(
                        run.status,
                        PracticeStatus::Unverified | PracticeStatus::NotApplicable
                    )
                })
                .max_by_key(|run| (run.finished_at_ms, run.id.clone()))
            else {
                continue;
            };
            let (kind, attempt) = if run.status == PracticeStatus::NotApplicable {
                (
                    StallKind::PracticeNotApplicable,
                    "判断现有运行环境做不了这个目标，没有实际运行".to_string(),
                )
            } else {
                let mut text = format!(
                    "实际做了 {} 次最小产物，在运行环境中都没有验证通过",
                    run.attempts.len()
                );
                if let Some(issue) = run
                    .attempts
                    .iter()
                    .rev()
                    .flat_map(|attempt| attempt.issues.iter())
                    .next()
                {
                    text.push_str("；最后一次的问题：");
                    text.push_str(issue);
                }
                (StallKind::PracticeUnverified, text)
            };
            facts.push(Fact {
                kind: FactKind::Attempt,
                text: prefix(&attempt, MAX_FACT_BYTES).into(),
            });
            facts.push(Fact {
                kind: FactKind::Capability,
                text: prefix(
                    &format!(
                        "运行环境能做的产物：{}（文件布局：{}）",
                        run.runner.domain, run.runner.layout
                    ),
                    MAX_FACT_BYTES,
                )
                .into(),
            });
            Stall {
                kind,
                run_id: run.id.clone(),
            }
        } else if let Some(knowledge) = &knowledge {
            let Some(run) = knowledge
                .runs_for(&goal.id)
                .filter(|run| run.topic.goal_revision == goal.revision)
                .filter(|run| run.status == RunStatus::Completed)
                .filter(|run| {
                    !knowledge.entries_for(&goal.id).any(|entry| {
                        entry.run_id == run.id && entry.status == KnowledgeStatus::SourceQuoted
                    })
                })
                .max_by_key(|run| (run.finished_at_ms, run.id.clone()))
            else {
                continue;
            };
            facts.push(Fact {
                kind: FactKind::Attempt,
                text: "在配置的资料来源中研究过，没有找到可以逐字引用的相关内容".into(),
            });
            Stall {
                kind: StallKind::ResearchEmpty,
                run_id: run.id.clone(),
            }
        } else {
            return Ok(None);
        };
        // 引用兴趣的原话；兴趣已撤回或没有原话时不问。
        let Ok(snapshot) = services.interests.snapshot(&scope(owner)) else {
            continue;
        };
        let Some(interest) = snapshot.interests.iter().find(|interest| {
            interest.id == goal.source.reference && interest.status == InterestStatus::Active
        }) else {
            continue;
        };
        let quotes: Vec<Fact> = interest
            .statements
            .iter()
            .filter(|statement| statement.kind != StatementKind::Withdrawal)
            .take(MAX_QUOTES)
            .map(|statement| Fact {
                kind: FactKind::UserQuote,
                text: prefix(&statement.quote, MAX_FACT_BYTES).into(),
            })
            .filter(|fact| !fact.text.is_empty())
            .collect();
        if quotes.is_empty() {
            continue;
        }
        facts.splice(0..0, quotes);
        return Ok(Some(Candidate {
            owner: owner.into(),
            goal_id: goal.id.clone(),
            goal_revision: goal.revision,
            stall,
            facts,
        }));
    }
    Ok(None)
}

async fn compose(
    services: &Services,
    request: &Clarification,
    stopped: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let input = ComposeRequest {
        request_id: request.id.clone(),
        composer_version: request.composer_version.clone(),
        stall: request.stall.kind,
        facts: request.facts.clone(),
    };
    let result = {
        let work = services.composer.compose(input);
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = stop_requested(stopped) => Err(ClarifyFailure::Cancelled),
            result = tokio::time::timeout(COMPOSE_TIMEOUT, &mut work) => match result {
                Ok(Ok(question)) => Ok(question),
                Ok(Err(ClarifyError::Clarify(failure))) => Err(failure),
                Ok(Err(_)) => Err(ClarifyFailure::InvalidOutput),
                Err(_) => Err(ClarifyFailure::Timeout),
            },
        }
    };
    // 远端请求可能已经发生；取消、超时或失败都记录结局，不重试同一修订。
    services
        .clarify
        .record_composition(&request.id, now_ms()?.max(request.created_at_ms), result)
        .map_err(clarify_error)?;
    Ok(())
}

fn plugin_error(error: ClarifyError) -> PluginError {
    PluginError::State(error.to_string())
}

/// 通道的被动投递点：先保存时机判断与投递尝试，再交给通道发送。不主动私聊。
pub(crate) struct ChannelAdapter {
    pub(crate) clarify: Arc<dyn ClarifyAdmin>,
    pub(crate) judge: Arc<dyn TimingJudge>,
    pub(crate) policy: ClarifyPolicy,
    /// 余额不够一次时机判断时，本条消息不判断、不附带问题。
    pub(crate) gate: qq_budget::Gate,
}

fn time() -> PluginResult<u64> {
    now_ms().map_err(|_| PluginError::State("系统时间不可用".into()))
}

fn affordable(gate: &qq_budget::Gate) -> PluginResult<bool> {
    gate.admits(qq_budget::SINGLE_UNIT)
        .map_err(|_| PluginError::State("自主运行预算不可用".into()))
}

impl QqOutreach for ChannelAdapter {
    fn due(&self, session: &SessionKey) -> PluginResult<bool> {
        let snapshot = self.clarify.snapshot().map_err(plugin_error)?;
        Ok(snapshot
            .due_for(&session.user_id, time()?, &self.policy)
            .is_some_and(|request| request.judgements.len() < MAX_JUDGEMENTS)
            && affordable(&self.gate)?)
    }

    fn judge(&self, moment: QqOutreachMoment) -> QqOutreachFuture {
        let clarify = self.clarify.clone();
        let judge = self.judge.clone();
        let policy = self.policy;
        let gate = self.gate.clone();
        Box::pin(async move {
            let now = time()?;
            let snapshot = clarify.snapshot().map_err(plugin_error)?;
            let Some(request) = snapshot
                .judgeable_for(&moment.session.user_id, &moment.message_id, now, &policy)
                .cloned()
            else {
                return Ok(false);
            };
            if !affordable(&gate)? {
                return Ok(false);
            }
            // 先保存判断再请求判断器；同一条消息只判断一次，中断不重放。
            clarify
                .begin_judgement(&request.id, &moment.message_id, now)
                .map_err(plugin_error)?;
            let input = JudgeRequest {
                invitation_id: request.id.clone(),
                judge_version: judge.version().into(),
                invitation: prefix(
                    request.question.as_deref().unwrap_or_default(),
                    MAX_MOMENT_BYTES,
                )
                .into(),
                user_message: prefix(&moment.user_text, MAX_MOMENT_BYTES).into(),
                reply: prefix(&moment.reply_text, MAX_MOMENT_BYTES).into(),
                feedback: vec![],
            };
            let result = match tokio::time::timeout(QQ_OUTREACH_JUDGE_TIMEOUT, judge.judge(input))
                .await
            {
                Ok(Ok(verdict)) => Ok(verdict == Verdict::Invite),
                Ok(Err(eve_outreach_api::OutreachError::Outreach(failure))) => Err(match failure {
                    eve_outreach_api::OutreachFailure::Timeout => ClarifyFailure::Timeout,
                    eve_outreach_api::OutreachFailure::Cancelled => ClarifyFailure::Cancelled,
                    eve_outreach_api::OutreachFailure::InvalidOutput => {
                        ClarifyFailure::InvalidOutput
                    }
                    _ => ClarifyFailure::Provider,
                }),
                Ok(Err(_)) => Err(ClarifyFailure::InvalidOutput),
                Err(_) => Err(ClarifyFailure::Timeout),
            };
            let finished = time().unwrap_or(now).max(now);
            clarify
                .record_judgement(&request.id, &moment.message_id, finished, result)
                .map_err(plugin_error)?;
            Ok(result == Ok(true))
        })
    }

    fn attach(
        &self,
        session: &SessionKey,
        message_id: &str,
    ) -> PluginResult<Option<QqOutreachMessage>> {
        let snapshot = self.clarify.snapshot().map_err(plugin_error)?;
        let Some(request) = snapshot.due_for(&session.user_id, time()?, &self.policy) else {
            return Ok(None);
        };
        let latest_ask = request.judgements.last().is_some_and(|judgement| {
            judgement.message_id == message_id
                && judgement.outcome == Some(eve_clarify_api::JudgeOutcome::Ask)
        });
        if !latest_ask {
            return Ok(None);
        }
        let claimed = self
            .clarify
            .claim(&request.id, message_id, time()?)
            .map_err(plugin_error)?;
        Ok(claimed.question.map(|text| QqOutreachMessage {
            id: claimed.id,
            text,
        }))
    }

    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        Ok(None)
    }

    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        let result = match result {
            QqOutreachResult::Sent {
                platform_message_id,
            } => DeliveryResult::Sent {
                platform_message_id: platform_message_id
                    .filter(|id| eve_clarify_api::validate_id(id).is_ok()),
            },
            QqOutreachResult::Failed {
                http_status,
                biz_code,
            } => DeliveryResult::Failed {
                http_status,
                biz_code,
            },
            QqOutreachResult::Unknown => DeliveryResult::Unknown,
            QqOutreachResult::NotSent => DeliveryResult::NotSent,
        };
        self.clarify
            .record_delivery(id, time()?, result)
            .map_err(plugin_error)?;
        Ok(())
    }
}

/// 多个投递来源共用通道的投递点：按顺序判断第一个此刻可判断的来源，附带只取对这条消息判断为合适的来源；
/// 投递结果按 ID 前缀交回分配它的来源。
pub(crate) struct Combined(pub(crate) Vec<(&'static str, Arc<dyn QqOutreach>)>);
impl QqOutreach for Combined {
    fn group_chime(
        &self,
        id: &str,
    ) -> PluginResult<Option<eve_qqbot_plugin::QqGroupChimeDelivery>> {
        match self.0.iter().find(|(prefix, _)| id.starts_with(prefix)) {
            Some((_, source)) => source.group_chime(id),
            None => Ok(None),
        }
    }
    fn due(&self, session: &SessionKey) -> PluginResult<bool> {
        for (_, source) in &self.0 {
            if source.due(session)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn judge(&self, moment: QqOutreachMoment) -> QqOutreachFuture {
        for (_, source) in &self.0 {
            match source.due(&moment.session) {
                Ok(true) => return source.judge(moment),
                Ok(false) => {}
                Err(error) => return Box::pin(async move { Err(error) }),
            }
        }
        Box::pin(async { Ok(false) })
    }

    fn attach(
        &self,
        session: &SessionKey,
        message_id: &str,
    ) -> PluginResult<Option<QqOutreachMessage>> {
        for (_, source) in &self.0 {
            if let Some(message) = source.attach(session, message_id)? {
                return Ok(Some(message));
            }
        }
        Ok(None)
    }

    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        for (_, source) in &self.0 {
            if let Some(push) = source.next_push()? {
                return Ok(Some(push));
            }
        }
        Ok(None)
    }

    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        let (_, source) = self
            .0
            .iter()
            .find(|(prefix, _)| id.starts_with(prefix))
            .ok_or_else(|| PluginError::State("投递结果没有对应的来源".into()))?;
        source.delivered(id, result)
    }
}

/// /clarify：只读写当前会话自己的问题；不显示其他用户的内容。
pub(crate) struct Commands {
    enabled: Option<Arc<dyn ClarifyAdmin>>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { enabled: None })
    }
    pub(crate) fn enabled(clarify: Arc<dyn ClarifyAdmin>) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            enabled: Some(clarify),
        })
    }
}

fn status_text(request: &Clarification) -> &'static str {
    match request.status {
        ClarifyStatus::Composing => "正在撰写",
        ClarifyStatus::Pending => "等待合适的时机",
        ClarifyStatus::Delivering => "正在发送",
        ClarifyStatus::Delivered => "已送达，等你回复",
        ClarifyStatus::Answered => "你已回复",
        ClarifyStatus::Expired => "送达后没有回复，不再等待",
        ClarifyStatus::Unknown => "送达与否不确定，不重发",
        ClarifyStatus::Cancelled(CancelReason::GoalChanged) => "目标已更新，不再问",
        ClarifyStatus::Cancelled(CancelReason::GoalClosed) => "学习目标已结束，不再问",
        ClarifyStatus::Failed(ClarifyFailure::Exhausted) => "一直没有合适的时机，不再问",
        ClarifyStatus::Failed(_) => "撰写失败，不重试",
        ClarifyStatus::Interrupted => "撰写时中断，不重放",
    }
}

fn stall_text(kind: StallKind) -> &'static str {
    match kind {
        StallKind::ResearchEmpty => "研究没有找到可引用的资料",
        StallKind::PracticeUnverified => "实践尝试用尽仍未验证",
        StallKind::PracticeNotApplicable => "现有运行环境做不了",
    }
}

impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let text = input.text.trim();
        let Some(rest) = text.strip_prefix("/clarify") else {
            return Ok(None);
        };
        if !rest.is_empty() && !rest.starts_with(' ') {
            return Ok(None);
        }
        let Some(clarify) = &self.enabled else {
            return Ok(Some(DISABLED.into()));
        };
        let owner = &input.session.user_id;
        match rest.trim() {
            "" => {}
            "off" | "on" => {
                let quiet = rest.trim() == "off";
                clarify
                    .set_quiet(owner, quiet, time()?)
                    .map_err(plugin_error)?;
                return Ok(Some(
                    if quiet {
                        "好的，之后遇到卡住的地方不再附带问题来问你；/clarify on 恢复。"
                    } else {
                        "好的，之后遇到卡住的地方可能会在聊天时顺便问你。"
                    }
                    .into(),
                ));
            }
            _ => return Ok(Some(HELP.into())),
        }
        let snapshot = clarify.snapshot().map_err(plugin_error)?;
        let mut owned: Vec<&Clarification> = snapshot.for_owner(owner).collect();
        if owned.is_empty() {
            let mut reply = String::from("Eve 还没有因为卡住而问过你问题。");
            if snapshot.quiet(owner) {
                reply.push_str(" 你已要求不再附带这类问题；/clarify on 恢复。");
            }
            return Ok(Some(reply));
        }
        owned.sort_by_key(|request| std::cmp::Reverse((request.created_at_ms, request.id.clone())));
        let mut lines = Vec::new();
        for request in owned.into_iter().take(5) {
            let mut line = format!(
                "{}（目标 {} 第 {} 版，{}）：{}",
                request.id,
                request.goal_id,
                request.goal_revision,
                stall_text(request.stall.kind),
                status_text(request)
            );
            if let Some(question) = &request.question {
                line.push_str(&format!("\n问题：{question}"));
            }
            if let Some(answer) = &request.answer {
                line.push_str(&format!("\n你的回复：{}", answer.text));
            }
            lines.push(line);
        }
        if snapshot.quiet(owner) {
            lines.push("你已要求不再附带这类问题；/clarify on 恢复。".into());
        }
        Ok(Some(lines.join("\n\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录调用的投递来源替身：`due` 与 `attach` 的结果由测试设定。
    struct Source {
        due: bool,
        attach: Option<&'static str>,
        calls: Mutex<Vec<String>>,
    }
    impl Source {
        fn new(due: bool, attach: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                due,
                attach,
                calls: Mutex::new(vec![]),
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl QqOutreach for Source {
        fn due(&self, _: &SessionKey) -> PluginResult<bool> {
            Ok(self.due)
        }
        fn judge(&self, moment: QqOutreachMoment) -> QqOutreachFuture {
            self.calls
                .lock()
                .unwrap()
                .push(format!("judge:{}", moment.message_id));
            Box::pin(async { Ok(true) })
        }
        fn attach(&self, _: &SessionKey, _: &str) -> PluginResult<Option<QqOutreachMessage>> {
            Ok(self.attach.map(|id| QqOutreachMessage {
                id: id.into(),
                text: "问题？".into(),
            }))
        }
        fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
            Ok(None)
        }
        fn delivered(&self, id: &str, _: QqOutreachResult) -> PluginResult<()> {
            self.calls.lock().unwrap().push(format!("delivered:{id}"));
            Ok(())
        }
    }

    fn session() -> SessionKey {
        SessionKey::new("user-1", "user-1").unwrap()
    }

    #[tokio::test]
    async fn combined_sources_judge_the_first_due_and_route_results_by_id_prefix() {
        let invitations = Source::new(false, None);
        let questions = Source::new(true, Some("clarify-1"));
        let combined = Combined(vec![
            ("invite-", invitations.clone() as Arc<dyn QqOutreach>),
            (ID_PREFIX, questions.clone() as Arc<dyn QqOutreach>),
        ]);
        assert!(combined.due(&session()).unwrap());
        let moment = QqOutreachMoment {
            session: session(),
            message_id: "m1".into(),
            user_text: "你好".into(),
            reply_text: "你好".into(),
        };
        assert!(combined.judge(moment).await.unwrap());
        assert_eq!(
            invitations.calls(),
            Vec::<String>::new(),
            "没有到期的来源不判断"
        );
        let message = combined.attach(&session(), "m1").unwrap().unwrap();
        assert_eq!(message.id, "clarify-1");
        combined
            .delivered("clarify-1", QqOutreachResult::NotSent)
            .unwrap();
        assert_eq!(questions.calls(), ["judge:m1", "delivered:clarify-1"]);
        assert!(
            combined
                .delivered("other-1", QqOutreachResult::NotSent)
                .is_err(),
            "没有对应来源的结果不能静默丢弃"
        );
    }
}
