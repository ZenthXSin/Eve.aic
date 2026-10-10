//! QQ 宿主的多步计划入口：用户为自己的待办请模型建议计划，确认后由后台按依赖推进。
//!
//! 计划只能引用宿主登记的能力，即操作者启动时已开启的受控研究与实践验证；模型建议不授予权限，
//! 须用户确认才活动。步骤先保存为 Executing，待办才交给对应后台；后台结束后按各自账本的证据
//! 判定步骤效果：研究须得到附逐字引用的知识，实践须在运行环境中实际验证通过。
//! 计划完成只说明每一步的效果条件都被证据满足，不改变待办状态。
use crate::{
    AppError,
    cognition_action_admission::{saved_artifact, verified_reflection},
    qq_cognition::GOAL_CHANNEL,
};
use eve_cognition_api::{CognitionAdmin, Goal, GoalStatus, SourceKind, Visibility};
use eve_cognition_loop_plugin::current_reflection;
use eve_knowledge_api::{KnowledgeAdmin, KnowledgeStatus, RunStatus as ResearchStatus};
use eve_plan_api::*;
use eve_plugin_api::{PluginError, PluginResult};
use eve_practice_api::{PracticeAdmin, PracticeStatus};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_session_api::SessionService;
use ring::digest::{Context, SHA256};
use std::{
    fmt::Write,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

/// 按待办描述在入口页面范围内受控研究一次。
pub(crate) const RESEARCH: &str = "eve.research.v1";
/// 按待办描述做最小产物并在运行环境中实际验证。
pub(crate) const PRACTICE: &str = "eve.practice.v1";
const SUBJECT: &str = "eve";
const USER_GOAL: &str = "user-goal:v1";
/// 研究自身至多 3 分钟；其余是排在其他研究之后的等待。
const RESEARCH_TIMEOUT_MS: u64 = 600_000;
/// 实践自身至多 15 分钟；其余是排在其他实践之后的等待。
const PRACTICE_TIMEOUT_MS: u64 = 1_800_000;
const SHOWN_PLANS: usize = 5;
const SHOWN_GOAL_PLANS: usize = 3;
const DISABLED: &str = "计划入口未启用。";
const HELP: &str = "用法：/plans 列出计划；/plans 待办ID 查看步骤与证据；/plan propose 待办ID 请 Eve 建议计划；/plan confirm 待办ID 确认后开始推进；/plan withdraw 待办ID 撤销。待办 ID 可用 /goals 查看。";
const NOT_FOUND: &str = "当前会话未找到该待办，发送 /goals 查看。";

/// 宿主登记的能力：只登记操作者已开启的后台。两者都是每个待办版本至多一次，不能重试。
pub(crate) fn capabilities(research: bool, practice: bool) -> Vec<CapabilitySpec> {
    let mut list = Vec::new();
    if research {
        list.push(CapabilitySpec {
            id: RESEARCH.into(),
            description: "按待办描述在操作者给出的入口页面范围内只读研究一次，得到附逐字引用来源的知识才有证据。每个待办版本至多一次，max_attempts 须为 1。".into(),
            max_attempts: 1,
            max_timeout_ms: RESEARCH_TIMEOUT_MS,
            requires_input: false,
        });
    }
    if practice {
        list.push(CapabilitySpec {
            id: PRACTICE.into(),
            description: "按待办描述做只含数据文件的最小产物，在操作者提供的运行环境中实际运行验证（自带至多三次修正），会使用同一待办已研究到的知识；实际验证通过才有证据。每个待办版本至多一次，max_attempts 须为 1。".into(),
            max_attempts: 1,
            max_timeout_ms: PRACTICE_TIMEOUT_MS,
            requires_input: false,
        });
    }
    list
}

/// QQ 用户以 /goal 保存的待办的所有者。
fn user_goal(goal: &Goal) -> Option<&str> {
    let Visibility::User(owner) = &goal.visibility else {
        return None;
    };
    (goal.source.kind == SourceKind::User
        && goal.source.channel == GOAL_CHANNEL
        && goal.verification == USER_GOAL)
        .then_some(owner.as_str())
}

/// 活动计划中执行中的步骤所请求的待办与版本。
pub(crate) fn requested(snapshot: &PlanSnapshot, capability: &str) -> Vec<(String, u64)> {
    snapshot
        .plans
        .iter()
        .filter(|plan| plan.status == PlanStatus::Active)
        .filter(|plan| {
            plan.steps.iter().any(|step| {
                step.status == StepStatus::Executing && step.spec.capability == capability
            })
        })
        .map(|plan| (plan.binding.goal_id.clone(), plan.binding.goal_revision))
        .collect()
}

/// 计划步骤正在请求这个仍在等待的待办版本时，返回其所有者；研究与实践后台据此把它纳入处理。
pub(crate) fn requested_goal<'a>(goal: &'a Goal, requested: &[(String, u64)]) -> Option<&'a str> {
    let owner = user_goal(goal)?;
    (goal.status == GoalStatus::Waiting
        && requested
            .iter()
            .any(|(id, revision)| id == &goal.id && *revision == goal.revision))
    .then_some(owner)
}

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn failure_text() -> PluginError {
    PluginError::State("计划记录暂时不可用；未自动重试".into())
}

fn binding(goal: &Goal) -> PlanBinding {
    PlanBinding {
        goal_id: goal.id.clone(),
        goal_revision: goal.revision,
        input_sha256: None,
    }
}

/// 交给后台的建议请求；请求记录已经保存。
pub(crate) struct Pending {
    record_id: String,
    requested_at_ms: u64,
    request: ProposalRequest,
}

enum Command<'a> {
    List,
    Show(&'a str),
    Propose(&'a str),
    Confirm(&'a str),
    Withdraw(&'a str),
    Help,
}

fn parse(text: &str) -> Option<Command<'_>> {
    let mut words = text.split_whitespace();
    let name = words.next()?;
    let words: Vec<&str> = words.collect();
    match name {
        "/plans" => Some(match words.as_slice() {
            [] => Command::List,
            [id] => Command::Show(id),
            _ => Command::Help,
        }),
        "/plan" => Some(match words.as_slice() {
            ["propose", id] => Command::Propose(id),
            ["confirm", id] => Command::Confirm(id),
            ["withdraw", id] => Command::Withdraw(id),
            _ => Command::Help,
        }),
        _ => None,
    }
}

pub(crate) struct Commands {
    enabled: Option<Enabled>,
}
struct Enabled {
    plans: Arc<dyn PlanJournal>,
    cognition: Arc<dyn CognitionAdmin>,
    sessions: Arc<dyn SessionService>,
    capabilities: Vec<CapabilitySpec>,
    proposer_version: String,
    requests: mpsc::UnboundedSender<Pending>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { enabled: None })
    }
    pub(crate) fn enabled(
        plans: Arc<dyn PlanJournal>,
        cognition: Arc<dyn CognitionAdmin>,
        sessions: Arc<dyn SessionService>,
        capabilities: Vec<CapabilitySpec>,
        proposer_version: String,
        requests: mpsc::UnboundedSender<Pending>,
    ) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            enabled: Some(Enabled {
                plans,
                cognition,
                sessions,
                capabilities,
                proposer_version,
                requests,
            }),
        })
    }
}

impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = parse(input.text) else {
            return Ok(None);
        };
        let Some(enabled) = &self.enabled else {
            return Ok(Some(DISABLED.into()));
        };
        let user = input.session.user_id.as_str();
        let reply = match command {
            Command::Help => HELP.into(),
            Command::List => enabled.list(user)?,
            Command::Show(id) => enabled.show(user, id)?,
            Command::Propose(id) => enabled.propose(user, id)?,
            Command::Confirm(id) => enabled.confirm(user, id)?,
            Command::Withdraw(id) => enabled.withdraw(user, id)?,
        };
        Ok(Some(reply))
    }
}

impl Enabled {
    /// 当前会话自己的待办；不是本人的或不存在时返回 None。
    fn goal(&self, user: &str, id: &str) -> PluginResult<Option<Goal>> {
        let snapshot = self.cognition.snapshot().map_err(|_| failure_text())?;
        Ok(snapshot
            .state
            .goals
            .get(id)
            .filter(|goal| user_goal(goal) == Some(user))
            .cloned())
    }

    fn snapshot(&self) -> PluginResult<PlanSnapshot> {
        self.plans.snapshot().map_err(|_| failure_text())
    }

    fn list(&self, user: &str) -> PluginResult<String> {
        let cognition = self.cognition.snapshot().map_err(|_| failure_text())?;
        let snapshot = self.snapshot()?;
        let owned = |goal_id: &str| {
            cognition
                .state
                .goals
                .get(goal_id)
                .is_some_and(|goal| user_goal(goal) == Some(user))
        };
        let mut plans: Vec<&Plan> = snapshot
            .plans
            .iter()
            .filter(|plan| owned(&plan.binding.goal_id))
            .collect();
        let requested: Vec<&ProposalRecord> = snapshot
            .proposals
            .iter()
            .filter(|record| {
                record.status == ProposalStatus::Requested && owned(&record.binding.goal_id)
            })
            .collect();
        if plans.is_empty() && requested.is_empty() {
            return Ok("还没有计划。发送 /plan propose 待办ID 请 Eve 建议一份计划。".into());
        }
        plans.sort_by_key(|plan| std::cmp::Reverse((plan.is_open(), plan.created_at_ms)));
        let mut reply = format!("你的计划（共 {} 份）：", plans.len());
        for plan in plans.into_iter().take(SHOWN_PLANS) {
            let satisfied = plan
                .steps
                .iter()
                .filter(|step| step.status == StepStatus::Satisfied)
                .count();
            let _ = write!(
                reply,
                "\n- 待办 {}（版本 {}）：{}｜完成 {}/{} 步",
                plan.binding.goal_id,
                plan.binding.goal_revision,
                plan_status(plan.status),
                satisfied,
                plan.steps.len()
            );
        }
        for record in requested {
            let _ = write!(
                reply,
                "\n- 待办 {}（版本 {}）：正在请求计划建议",
                record.binding.goal_id, record.binding.goal_revision
            );
        }
        Ok(reply)
    }

    fn show(&self, user: &str, id: &str) -> PluginResult<String> {
        let Some(goal) = self.goal(user, id)? else {
            return Ok(NOT_FOUND.into());
        };
        let snapshot = self.snapshot()?;
        let mut plans: Vec<&Plan> = snapshot
            .plans
            .iter()
            .filter(|plan| plan.binding.goal_id == goal.id)
            .collect();
        plans.sort_by_key(|plan| std::cmp::Reverse(plan.created_at_ms));
        let mut reply = format!("待办 {}（当前版本 {}）的计划：", goal.id, goal.revision);
        if plans.is_empty() {
            reply.push_str("还没有。");
        }
        for plan in plans.into_iter().take(SHOWN_GOAL_PLANS) {
            let _ = write!(
                reply,
                "\n- 计划（版本 {}）：{}",
                plan.binding.goal_revision,
                plan_status(plan.status)
            );
            if plan.status == PlanStatus::Proposed {
                reply.push_str("，发送 /plan confirm 待办ID 确认后开始推进");
            }
            for (index, step) in plan.steps.iter().enumerate() {
                let _ = write!(
                    reply,
                    "\n  第 {} 步 {}（{}）：{}",
                    index + 1,
                    step.spec.title,
                    capability_name(&step.spec.capability),
                    step_status(step.status)
                );
                if let Some(attempt) = step.attempts.last() {
                    if let Some(evidence) = &attempt.evidence {
                        let _ = write!(
                            reply,
                            "｜证据：{} 摘要 {}",
                            evidence.source_id,
                            &evidence.sha256[..12]
                        );
                    } else if let Some(failure) = attempt.failure {
                        let _ = write!(reply, "｜{}", step_failure(failure));
                    }
                }
            }
        }
        let records: Vec<&ProposalRecord> = snapshot
            .proposals
            .iter()
            .filter(|record| record.binding.goal_id == goal.id)
            .collect();
        for record in records {
            let _ = write!(
                reply,
                "\n建议请求（版本 {}）：{}",
                record.binding.goal_revision,
                match &record.status {
                    ProposalStatus::Requested => "进行中".to_string(),
                    ProposalStatus::Proposed { .. } => "已生成计划".into(),
                    ProposalStatus::Empty => "模型认为现有能力不足以形成计划".into(),
                    ProposalStatus::Failed { failure } => proposal_failure(*failure).into(),
                }
            );
        }
        Ok(reply)
    }

    fn propose(&self, user: &str, id: &str) -> PluginResult<String> {
        let Some(goal) = self.goal(user, id)? else {
            return Ok(NOT_FOUND.into());
        };
        if goal.status != GoalStatus::Waiting {
            return Ok("这个待办不在等待处理，不再建议计划。".into());
        }
        if self.capabilities.is_empty() {
            return Ok("当前没有可供计划使用的能力：需要操作者开启受控研究或实践验证。".into());
        }
        let at_ms = now_ms().map_err(|_| failure_text())?;
        let current = binding(&goal);
        // 旧版本的待确认或活动计划先封存，新版本才能建议新计划。
        for plan in self.snapshot()?.plans {
            if plan.is_open() && plan.binding.goal_id == goal.id && plan.binding != current {
                if plan
                    .steps
                    .iter()
                    .any(|step| step.status == StepStatus::Executing)
                {
                    return Ok("旧版本计划的步骤还在执行，等它结束后再请求。".into());
                }
                self.plans
                    .invalidate(&plan.id, plan.revision, &current, at_ms)
                    .map_err(|_| failure_text())?;
            }
        }
        let snapshot = self.snapshot()?;
        let record_id = proposal_id(SUBJECT, &current).map_err(|_| failure_text())?;
        if let Some(record) = snapshot
            .proposals
            .iter()
            .find(|record| record.id == record_id)
        {
            return Ok(format!(
                "这个版本已经请求过计划建议（{}），每个版本只请求一次；发送 /plans {} 查看。",
                match &record.status {
                    ProposalStatus::Requested => "进行中",
                    ProposalStatus::Proposed { .. } => "已生成计划",
                    ProposalStatus::Empty => "模型认为现有能力不足以形成计划",
                    ProposalStatus::Failed { .. } => "未成功，不重试",
                },
                goal.id
            ));
        }
        if snapshot
            .plans
            .iter()
            .any(|plan| plan.is_open() && plan.binding.goal_id == goal.id)
        {
            return Ok(format!(
                "这个待办已有待确认或进行中的计划；发送 /plans {} 查看，或先 /plan withdraw。",
                goal.id
            ));
        }
        // 上下文在保存请求记录之前准备；这些失败不消耗这个版本的建议机会。
        let draft = self.draft(&goal)?;
        let request = ProposalRequest::new(
            SUBJECT,
            current.clone(),
            goal.description.clone(),
            draft,
            &self.capabilities,
        )
        .map_err(|_| failure_text())?;
        let reserved = match self
            .plans
            .reserve_proposal(&current, &self.proposer_version, at_ms)
        {
            Ok(reserved) => reserved,
            Err(PlanError::LimitReached) => {
                return Ok("计划记录已满；保留原记录，未请求新的建议。".into());
            }
            Err(PlanError::Conflict) => {
                return Ok("这个待办已有待确认、进行中的计划或建议请求。".into());
            }
            Err(_) => return Err(failure_text()),
        };
        if reserved.duplicate {
            return Ok(format!(
                "这个版本已经请求过计划建议；发送 /plans {} 查看。",
                goal.id
            ));
        }
        let pending = Pending {
            record_id: reserved.record.id.clone(),
            requested_at_ms: reserved.record.requested_at_ms,
            request,
        };
        if self.requests.send(pending).is_err() {
            // 后台已经结束：记为取消，不留下进行中的请求。
            let _ = self.plans.finish_proposal(
                &reserved.record.id,
                ProposalOutcome::Failed(ProposalFailure::Cancelled),
                &self.capabilities,
                at_ms,
            );
            return Err(failure_text());
        }
        Ok(format!(
            "已请求计划建议（只建议，不执行任何步骤）。稍后发送 /plans {0} 查看，满意后发送 /plan confirm {0} 开始推进。",
            goal.id
        ))
    }

    /// 当前版本已验证的反思草稿；没有时省略。
    fn draft(&self, goal: &Goal) -> PluginResult<Option<ProposalDraft>> {
        let snapshot = self.cognition.snapshot().map_err(|_| failure_text())?;
        let reflection =
            current_reflection(&snapshot.state, SUBJECT, goal).map_err(|_| failure_text())?;
        let Some(reflection) =
            reflection.filter(|reflection| verified_reflection(reflection, goal))
        else {
            return Ok(None);
        };
        let artifact =
            saved_artifact(self.sessions.as_ref(), reflection).map_err(|_| failure_text())?;
        Ok(Some(ProposalDraft {
            summary: artifact.summary,
            next_step: artifact.next_step,
        }))
    }

    fn open_plan(&self, goal_id: &str) -> PluginResult<Option<Plan>> {
        Ok(self
            .snapshot()?
            .plans
            .into_iter()
            .find(|plan| plan.is_open() && plan.binding.goal_id == goal_id))
    }

    fn confirm(&self, user: &str, id: &str) -> PluginResult<String> {
        let Some(goal) = self.goal(user, id)? else {
            return Ok(NOT_FOUND.into());
        };
        let Some(plan) = self
            .open_plan(&goal.id)?
            .filter(|plan| plan.status == PlanStatus::Proposed)
        else {
            return Ok(format!(
                "这个待办没有待确认的计划；发送 /plans {} 查看。",
                goal.id
            ));
        };
        if goal.status != GoalStatus::Waiting {
            return Ok("这个待办不在等待处理，不再推进计划。".into());
        }
        let at_ms = now_ms().map_err(|_| failure_text())?;
        let plan = self
            .plans
            .confirm(&plan.id, plan.revision, &binding(&goal), at_ms)
            .map_err(|_| failure_text())?;
        Ok(if plan.status == PlanStatus::Active {
            let first = plan.ready_steps().first().and_then(|id| plan.step(id));
            format!(
                "已确认，开始按计划推进{}。发送 /plans {} 查看进度。",
                first.map_or(String::new(), |step| format!("：先{}", step.spec.title)),
                goal.id
            )
        } else {
            "待办已经有了新版本，这份建议已过时，不会执行；可以再 /plan propose 请求新的建议。"
                .into()
        })
    }

    fn withdraw(&self, user: &str, id: &str) -> PluginResult<String> {
        let Some(goal) = self.goal(user, id)? else {
            return Ok(NOT_FOUND.into());
        };
        let Some(plan) = self.open_plan(&goal.id)? else {
            return Ok("这个待办没有待确认或进行中的计划。".into());
        };
        let at_ms = now_ms().map_err(|_| failure_text())?;
        match self.plans.withdraw(&plan.id, plan.revision, at_ms) {
            Ok(_) => Ok("已撤销计划；未开始的步骤不再执行，已有的证据保留。".into()),
            Err(PlanError::NotReady) => {
                Ok("有步骤正在执行，撤销会掩盖可能已经发生的结果；等这一步结束后再撤销。".into())
            }
            Err(_) => Err(failure_text()),
        }
    }
}

fn plan_status(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Proposed => "待确认",
        PlanStatus::Active => "进行中",
        PlanStatus::Completed => "已完成（每一步的效果都由证据满足）",
        PlanStatus::Blocked => "已停止（有步骤失败或中断）",
        PlanStatus::Stale => "已过时（待办有了新版本）",
        PlanStatus::Withdrawn => "已撤销",
    }
}

fn step_status(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "未开始",
        StepStatus::Executing => "执行中",
        StepStatus::Satisfied => "已满足",
        StepStatus::Failed => "未达成",
        StepStatus::Blocked => "已停止",
        StepStatus::Invalidated => "已失效",
    }
}

fn step_failure(failure: StepFailure) -> &'static str {
    match failure {
        StepFailure::CapabilityFailed => "能力没有给出结果",
        StepFailure::EffectNotMet => "实际结果未达成效果条件",
        StepFailure::Timeout => "超过期限",
        StepFailure::Cancelled => "已取消",
        StepFailure::Interrupted => "进程退出时中断，不重放",
        StepFailure::BindingChanged => "待办已变化",
    }
}

fn proposal_failure(failure: ProposalFailure) -> &'static str {
    match failure {
        ProposalFailure::Provider => "模型请求失败，不重试",
        ProposalFailure::Timeout => "模型请求超时，不重试",
        ProposalFailure::Cancelled => "已取消",
        ProposalFailure::InvalidOutput => "建议不合规，没有保存",
        ProposalFailure::Interrupted => "进程退出时中断，不重放",
        ProposalFailure::BindingChanged => "请求期间待办已变化",
    }
}

fn capability_name(id: &str) -> &str {
    match id {
        RESEARCH => "受控研究",
        PRACTICE => "实践验证",
        other => other,
    }
}

/// 计划后台依赖的服务。
pub(crate) struct Services {
    pub(crate) plans: Arc<dyn PlanJournal>,
    pub(crate) cognition: Arc<dyn CognitionAdmin>,
    pub(crate) knowledge: Option<Arc<dyn KnowledgeAdmin>>,
    pub(crate) practice: Option<Arc<dyn PracticeAdmin>>,
    pub(crate) proposer: Arc<dyn PlanProposer>,
    pub(crate) capabilities: Vec<CapabilitySpec>,
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    requests: mpsc::UnboundedSender<Pending>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(services: Services) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let (requests, received) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放；宿主随后关闭通道，不假装后台仍正常。
            let result = run(services, received, activated, stopped).await;
            let _ = finished_sender.send(true);
            result
        });
        Self {
            active,
            stop,
            finished,
            requests,
            task,
        }
    }
    pub(crate) fn requests(&self) -> mpsc::UnboundedSender<Pending> {
        self.requests.clone()
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
        self.task.await.map_err(|_| "计划后台异常；已停止准入")?
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
    mut received: mpsc::UnboundedReceiver<Pending>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), AppError> {
    let result = async {
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
            if let Ok(pending) = received.try_recv() {
                propose(&services, pending, &mut stopped).await?;
                continue;
            }
            settle(&services, now_ms()?)?;
            tokio::select! {
                biased;
                _ = stop_requested(&mut stopped) => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
        }
    }
    .await;
    // 已保存但还没请求的建议记为取消，不留下进行中的记录。
    received.close();
    while let Ok(pending) = received.try_recv() {
        let at = now_ms()?.max(pending.requested_at_ms);
        services.plans.finish_proposal(
            &pending.record_id,
            ProposalOutcome::Failed(ProposalFailure::Cancelled),
            &services.capabilities,
            at,
        )?;
    }
    result
}

/// 发起一次已保存记录的建议请求；请求期间待办变化时不保存为计划。
async fn propose(
    services: &Services,
    pending: Pending,
    stopped: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let binding = pending.request.binding.clone();
    let result = {
        let request = services.proposer.propose(pending.request);
        tokio::pin!(request);
        tokio::select! {
            biased;
            _ = stop_requested(stopped) => Err(ProposalFailure::Cancelled),
            result = &mut request => result,
        }
    };
    let outcome = match result {
        Ok(steps) if steps.is_empty() => ProposalOutcome::Empty,
        Ok(steps) => {
            let snapshot = services.cognition.snapshot()?;
            let current = snapshot
                .state
                .goals
                .get(&binding.goal_id)
                .filter(|goal| goal.status == GoalStatus::Waiting)
                .map(self::binding);
            if current.as_ref() == Some(&binding) {
                ProposalOutcome::Steps(steps)
            } else {
                ProposalOutcome::Failed(ProposalFailure::BindingChanged)
            }
        }
        Err(failure) => ProposalOutcome::Failed(failure),
    };
    let at = now_ms()?.max(pending.requested_at_ms);
    services
        .plans
        .finish_proposal(&pending.record_id, outcome, &services.capabilities, at)?;
    Ok(())
}

/// 推进活动计划：待办有新版本时封存；执行中的步骤按账本证据结束；否则开始下一个就绪步骤。
/// 每次变更都用计划修订做 CAS；冲突时下一轮重新读取。
fn settle(services: &Services, at_ms: u64) -> Result<(), AppError> {
    let snapshot = services.plans.snapshot()?;
    let cognition = services.cognition.snapshot()?;
    for plan in snapshot
        .plans
        .iter()
        .filter(|plan| plan.status == PlanStatus::Active)
    {
        let goal = cognition
            .state
            .goals
            .get(&plan.binding.goal_id)
            .filter(|goal| user_goal(goal).is_some());
        let executing = plan
            .steps
            .iter()
            .find(|step| step.status == StepStatus::Executing);
        let result = match goal {
            Some(goal) if goal.revision != plan.binding.goal_revision => match executing {
                // 执行中的步骤记为待办已变化，计划随之封存；之后的结果不计为满足。
                Some(step) => services
                    .plans
                    .finish_step(
                        &plan.id,
                        &step.spec.id,
                        plan.revision,
                        StepOutcome::Failed {
                            failure: StepFailure::BindingChanged,
                            at_ms: at_ms.max(started(step)),
                        },
                    )
                    .map(|_| ()),
                None => services
                    .plans
                    .invalidate(&plan.id, plan.revision, &binding(goal), at_ms)
                    .map(|_| ()),
            },
            // 待办不再等待：不开始新步骤，执行中的步骤结束时照常判定。
            Some(goal) if goal.status != GoalStatus::Waiting && executing.is_none() => continue,
            None => continue,
            Some(_) => match executing {
                Some(step) => match outcome(services, plan, step, at_ms)? {
                    Some(outcome) => services
                        .plans
                        .finish_step(&plan.id, &step.spec.id, plan.revision, outcome)
                        .map(|_| ()),
                    None => continue,
                },
                None => {
                    let Some(step_id) = plan.ready_steps().first().map(|id| id.to_string()) else {
                        continue;
                    };
                    let begun = services
                        .plans
                        .begin_step(&plan.id, &step_id, plan.revision, at_ms);
                    match begun {
                        // 宿主这次没有登记该能力：不交给任何后台，直接记为能力失败。
                        Ok(begun)
                            if !services.capabilities.iter().any(|capability| {
                                begun
                                    .step(&step_id)
                                    .is_some_and(|step| step.spec.capability == capability.id)
                            }) =>
                        {
                            services
                                .plans
                                .finish_step(
                                    &begun.id,
                                    &step_id,
                                    begun.revision,
                                    StepOutcome::Failed {
                                        failure: StepFailure::CapabilityFailed,
                                        at_ms,
                                    },
                                )
                                .map(|_| ())
                        }
                        result => result.map(|_| ()),
                    }
                }
            },
        };
        match result {
            Ok(()) | Err(PlanError::Conflict) | Err(PlanError::NotReady) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn started(step: &StepState) -> u64 {
    step.attempts
        .last()
        .map_or(0, |attempt| attempt.started_at_ms)
}

/// 执行中步骤的结局：后台账本已有结论时给出证据或失败；仍在进行时超过期限记为超时，否则 None。
fn outcome(
    services: &Services,
    plan: &Plan,
    step: &StepState,
    at_ms: u64,
) -> Result<Option<StepOutcome>, AppError> {
    let begun = started(step);
    let at_ms = at_ms.max(begun);
    let failed = |failure| Some(StepOutcome::Failed { failure, at_ms });
    let revision = plan.binding.goal_revision;
    let goal_id = plan.binding.goal_id.as_str();
    let settled = match step.spec.capability.as_str() {
        RESEARCH => match &services.knowledge {
            None => failed(StepFailure::CapabilityFailed),
            Some(knowledge) => {
                let snapshot = knowledge.snapshot()?;
                let run = snapshot
                    .runs_for(goal_id)
                    .find(|run| run.topic.goal_revision == revision);
                let exhausted =
                    snapshot.runs_for(goal_id).count() >= eve_knowledge_api::MAX_RUNS_PER_GOAL;
                match run.map(|run| (run, &run.status)) {
                    // 这个待办的研究次数已用完，后台不会再接：不必等到期限。
                    None if exhausted => failed(StepFailure::CapabilityFailed),
                    None | Some((_, ResearchStatus::Running)) => None,
                    Some((run, ResearchStatus::Completed)) => {
                        let entries: Vec<_> = snapshot
                            .entries_for(goal_id)
                            .filter(|entry| {
                                entry.run_id == run.id
                                    && entry.status == KnowledgeStatus::SourceQuoted
                            })
                            .collect();
                        if entries.is_empty() {
                            failed(StepFailure::EffectNotMet)
                        } else {
                            let mut parts = vec![run.id.as_str()];
                            let mut bytes = 0u64;
                            for entry in &entries {
                                parts.push(&entry.id);
                                parts.push(&entry.statement);
                                bytes += entry.statement.len() as u64;
                                if let Some(source) = &entry.source {
                                    parts.push(&source.quote);
                                    bytes += source.quote.len() as u64;
                                }
                            }
                            Some(evidence(RESEARCH, &run.id, &parts, bytes, at_ms))
                        }
                    }
                    Some(_) => failed(StepFailure::CapabilityFailed),
                }
            }
        },
        PRACTICE => match &services.practice {
            None => failed(StepFailure::CapabilityFailed),
            Some(practice) => {
                let snapshot = practice.snapshot()?;
                let run = snapshot
                    .runs_for(goal_id)
                    .find(|run| run.task.goal_revision == revision);
                let exhausted =
                    snapshot.runs_for(goal_id).count() >= eve_practice_api::MAX_RUNS_PER_GOAL;
                match run {
                    // 这个待办的实践次数已用完，后台不会再接：不必等到期限。
                    None if exhausted => failed(StepFailure::CapabilityFailed),
                    None => None,
                    Some(run) => match run.status {
                        PracticeStatus::Running => None,
                        PracticeStatus::Verified => {
                            let draft = run
                                .verified_attempt()
                                .and_then(|attempt| attempt.draft.as_ref());
                            match draft {
                                Some(draft) => {
                                    let mut parts = vec![run.id.as_str()];
                                    let mut bytes = 0u64;
                                    for file in &draft.files {
                                        parts.push(&file.path);
                                        parts.push(&file.content);
                                        bytes += file.content.len() as u64;
                                    }
                                    Some(evidence(PRACTICE, &run.id, &parts, bytes, at_ms))
                                }
                                None => failed(StepFailure::CapabilityFailed),
                            }
                        }
                        PracticeStatus::Unverified => failed(StepFailure::EffectNotMet),
                        _ => failed(StepFailure::CapabilityFailed),
                    },
                }
            }
        },
        _ => failed(StepFailure::CapabilityFailed),
    };
    Ok(match settled {
        Some(outcome) => Some(outcome),
        None if at_ms >= begun.saturating_add(step.spec.timeout_ms) => failed(StepFailure::Timeout),
        None => None,
    })
}

/// 证据摘要覆盖账本中的记录 ID 与实际内容；源 ID 是账本记录 ID，不是路径或凭据。
fn evidence(capability: &str, source: &str, parts: &[&str], bytes: u64, at_ms: u64) -> StepOutcome {
    let mut context = Context::new(&SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let sha256 = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    StepOutcome::Evidence(StepEvidence {
        capability: capability.into(),
        source_id: source.into(),
        sha256,
        bytes,
        verified_at_ms: at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_goal_ids_and_reject_extra_words() {
        assert!(matches!(parse("/plans"), Some(Command::List)));
        assert!(matches!(parse("/plans g1"), Some(Command::Show("g1"))));
        assert!(matches!(
            parse("/plan propose g1"),
            Some(Command::Propose("g1"))
        ));
        assert!(matches!(
            parse("/plan confirm g1"),
            Some(Command::Confirm("g1"))
        ));
        assert!(matches!(
            parse("/plan withdraw g1"),
            Some(Command::Withdraw("g1"))
        ));
        assert!(matches!(parse("/plan g1"), Some(Command::Help)));
        assert!(matches!(parse("/plans a b"), Some(Command::Help)));
        assert!(parse("/planner").is_none());
    }

    #[test]
    fn only_enabled_backgrounds_become_capabilities_with_one_attempt() {
        assert!(capabilities(false, false).is_empty());
        let both = capabilities(true, true);
        assert_eq!(
            both.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            [RESEARCH, PRACTICE]
        );
        assert!(both.iter().all(|item| item.max_attempts == 1
            && item.max_timeout_ms <= MAX_STEP_TIMEOUT_MS
            && !item.requires_input));
        assert_eq!(capabilities(false, true)[0].id, PRACTICE);
    }
}
