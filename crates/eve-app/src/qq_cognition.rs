//! QQ 明确保存待办与用户反馈，后台按目标修订重新反思；没有主动投递能力。
use crate::{AppError, core_bootstrap, models, services};
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{
    CognitionLoopPlugin, LoopController, ReflectionArtifact, ReflectionDrivePolicy,
    ReflectionVerifier, current_reflection,
};
use eve_cognition_plugin::{CognitionController, CognitionPlugin, UserGoalFeedback};
use eve_config_api::{CONFIG_SERVICE_ID, ConfigServiceHandle};
use eve_control_api::ControlServiceHandle;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatRole, ContextAssembler, ContextService, ContextSnapshot, LlmFuture, LlmModelResolver,
    TurnInput,
};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginId,
    PluginManifest, PluginResult, ServiceId,
};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_runtime::{
    BudgetedSessionRunner, ContextBinding, ControlGoalExecutor, LlmHost, LlmHostConfig,
    SessionBinding, SessionLlmHost,
};
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionKey, SessionService, SessionServiceHandle,
    SessionTurnStatus,
};
use ring::digest::{Context, SHA256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) const CONTROL_ID: &str = "eve.cognition.control";
const CONTEXT_ID: &str = "eve.cognition.context";
const CONTEXT_SERVICE: &str = "eve.cognition.context.service";
const CONTROL_SERVICE: &str = "eve.cognition.control.service";
/// QQ 用户以 /goal 保存的待办的来源渠道；计划入口据此核对待办归属。
pub(crate) const GOAL_CHANNEL: &str = "qq.goal";
const CHANNEL: &str = GOAL_CHANNEL;
pub(crate) const INTERNAL_USER: &str = "cognition.internal";
const HELP: &str =
    "用法：/goal 待办内容、/goals、/mind [目标ID]、/goal-feedback 目标ID 版本 反馈内容。";

fn failure() -> PluginError {
    PluginError::State("认知状态读取或保存失败；未自动重试".into())
}
fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}
fn scope(kind: SourceKind, channel: &str) -> ExecutionScope {
    ExecutionScope {
        subject_id: "eve".into(),
        access: ReadAccess::Internal,
        sources: vec![AllowedSource {
            kind,
            channel: channel.into(),
        }],
    }
}
/// 反思输入的父目标来源：用户待办，以及显式开启兴趣学习时的派生学习目标。
fn reflection_scope(interest_goals: bool) -> ExecutionScope {
    let mut scope = scope(SourceKind::User, CHANNEL);
    if interest_goals {
        scope.sources.push(AllowedSource {
            kind: SourceKind::Inference,
            channel: eve_interest_api::INTEREST_GOAL_CHANNEL.into(),
        });
    }
    scope
}
fn id_for(input: &QqCommandInput<'_>) -> String {
    let mut hash = Context::new(&SHA256);
    for part in [
        input.session.session_id.as_bytes(),
        input.session.user_id.as_bytes(),
        input.message_id.as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!(
        "qq-goal-{}",
        hash.finish()
            .as_ref()
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>()
    )
}

pub(crate) struct Commands {
    admin: Option<CognitionController>,
    sessions: Option<Arc<dyn SessionService>>,
    accepting: Arc<AtomicBool>,
    planning_budget: Option<(LoopController, u16)>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self {
            admin: None,
            sessions: None,
            accepting: Arc::new(AtomicBool::new(false)),
            planning_budget: None,
        })
    }
    fn state(&self, user: &str) -> PluginResult<CognitiveView> {
        self.admin
            .as_ref()
            .ok_or_else(failure)?
            .reader(ReadAccess::User(user.into()))
            .map_err(|_| failure())?
            .snapshot()
            .map_err(|_| failure())
    }
    fn add(&self, input: &QqCommandInput<'_>, text: &str) -> PluginResult<String> {
        if validate_text(text).is_err() {
            return Ok("待办不能为空，且不能超过 8192 UTF-8 字节。".into());
        }
        let admin = self.admin.as_ref().ok_or_else(failure)?;
        let snapshot = admin.snapshot().map_err(|_| failure())?;
        let id = id_for(input);
        if let Some(existing) = snapshot.state.goals.get(&id) {
            if existing.source.channel != CHANNEL
                || existing.source.reference != input.message_id
                || existing.visibility != Visibility::User(input.session.user_id.clone())
                || existing.description != text
            {
                return Err(failure());
            }
            return Ok(format!(
                "待办已保存：{id}，版本 {}。发送 /mind 查看草稿。",
                existing.revision
            ));
        }
        let source = Source {
            kind: SourceKind::User,
            channel: CHANNEL.into(),
            reference: input.message_id.into(),
        };
        let visibility = Visibility::User(input.session.user_id.clone());
        let goal = Goal {
            id: id.clone(),
            revision: 0,
            source: source.clone(),
            visibility: visibility.clone(),
            description: text.into(),
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
            wait_reason: Some("等待反思草稿与用户处理，现实目标尚未完成。".into()),
            block_reason: None,
            execution: None,
            feedback: None,
        };
        let mut state = snapshot.state;
        state.goals.insert(id.clone(), goal);
        state.events.push(CognitiveEvent {
            id: format!("input-{id}"),
            kind: CognitiveEventKind::ExternalInput,
            source,
            visibility,
            goal_id: Some(id.clone()),
            caused_by: None,
            at_ms: now_ms().map_err(|_| failure())?,
            summary: "QQ 用户明确保存待办，等待内部反思。".into(),
        });
        match admin.replace(snapshot.revision, state) {
            Ok(saved) => Ok(format!(
                "待办已保存：{id}，版本 {}。正在后台整理，稍后发送 /mind 查看草稿。",
                saved.state.goals[&id].revision
            )),
            Err(CognitionError::StaleRevision) => {
                Ok("认知状态正在更新，本条待办未保存；请重新发送 /goal。".into())
            }
            Err(CognitionError::LimitReached) => Ok("认知记录已达容量上限，未保存新待办。".into()),
            Err(_) => Err(failure()),
        }
    }
    fn feedback(&self, input: &QqCommandInput<'_>, tail: &str) -> PluginResult<String> {
        let Some((goal_id, remaining)) = tail.split_once(char::is_whitespace) else {
            return Ok(HELP.into());
        };
        let Some((revision, text)) = remaining.trim_start().split_once(char::is_whitespace) else {
            return Ok(HELP.into());
        };
        let Ok(expected_goal_revision) = revision.parse::<u64>() else {
            return Ok(HELP.into());
        };
        let feedback_id = id_for(input).replacen("qq-goal-", "qq-goal-feedback-", 1);
        let request = GoalFeedbackInput {
            goal_id: goal_id.into(),
            expected_goal_revision,
            feedback_id,
            text: text.trim().into(),
            at_ms: now_ms().map_err(|_| failure())?,
        };
        if request.validate().is_err() {
            return Ok(format!(
                "{HELP} 版本须为正整数，反馈为 1 至 4096 UTF-8 字节。"
            ));
        }
        let admin = self.admin.as_ref().ok_or_else(failure)?;
        let service = UserGoalFeedback::new(
            Arc::new(admin.clone()),
            "eve".into(),
            input.session.user_id.clone(),
            CHANNEL.into(),
            "qq.goal.feedback".into(),
        )
        .map_err(|_| failure())?;
        match service.submit(request) {
            Ok(report) => {
                let budget_exhausted = self.planning_budget.as_ref().is_some_and(|(controller, max)| {
                    controller.stats().is_ok_and(|stats| stats.submitted >= u64::from(*max))
                });
                let next = if report.duplicate {
                    "本条已处理，不重复更新或规划。"
                } else if budget_exhausted {
                    "本次启动的执行名额已用尽，新反馈仍已保存；后续启动再评估，不自动重试旧执行。"
                } else {
                    "后台将按新版本重新评估；旧草稿保留为历史，原待办仍未完成。"
                };
                Ok(format!("反馈已保存：{}，版本 {}。{} 发送 /mind 目标ID 查看当前状态。", report.goal_id, report.goal_revision, next))
            }
            Err(CognitionError::AccessDenied) => Ok("当前会话未找到该待办，发送 /goals 查看。".into()),
            Err(CognitionError::StaleRevision) => Ok("目标版本已变化或认知状态正在更新，本条反馈未保存；请用 /goals 查看当前版本后重新发送。".into()),
            Err(CognitionError::InvalidTransition) => Ok("该待办已结束、失效或不接受新反馈，原状态保留。".into()),
            Err(CognitionError::InvalidInput) => Ok(format!("{HELP} 反馈结构无效或消息标识冲突，未覆盖原记录。")),
            Err(CognitionError::LimitReached) => Ok("认知记录已达容量上限，反馈未保存。".into()),
            Err(_) => Err(failure()),
        }
    }
    fn goals(&self, user: &str) -> PluginResult<String> {
        let view = self.state(user)?;
        let goals: Vec<_> = view
            .state
            .goals
            .values()
            .filter(|g| {
                g.source.kind == SourceKind::User
                    && g.source.channel == CHANNEL
                    && g.visibility == Visibility::User(user.into())
            })
            .take(10)
            .collect();
        if goals.is_empty() {
            return Ok("当前会话还没有待办，发送 /goal 内容 添加。".into());
        }
        Ok(format!(
            "当前会话待办（最多 10 项）：\n{}\n发送 /mind 目标ID 查看草稿。",
            goals
                .iter()
                .map(|g| format!("{}：{:?}，版本 {}", g.id, g.status, g.revision))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
    fn mind(&self, user: &str, requested: Option<&str>) -> PluginResult<String> {
        let view = self.state(user)?;
        let owned = |goal: &&Goal| {
            goal.source.kind == SourceKind::User
                && goal.source.channel == CHANNEL
                && goal.visibility == Visibility::User(user.into())
        };
        let parent = if let Some(id) = requested {
            view.state.goals.get(id).filter(owned)
        } else {
            view.state
                .events
                .iter()
                .rev()
                .filter(|event| event.kind == CognitiveEventKind::ExternalInput)
                .filter_map(|event| event.goal_id.as_ref())
                .find_map(|id| view.state.goals.get(id).filter(owned))
        };
        let Some(parent) = parent else {
            return Ok("当前会话未找到该待办，发送 /goals 查看。".into());
        };
        let child =
            current_reflection(&view.state, &view.subject_id, parent).map_err(|_| failure())?;
        let Some(child) = child else {
            return Ok(format!(
                "待办已保存，当前版本 {} 的反思尚未开始；旧草稿不作为当前建议。请稍后发送 /mind。",
                parent.revision
            ));
        };
        if child.status != GoalStatus::Completed
            || !child.feedback.as_ref().is_some_and(|f| f.verification_met)
        {
            return Ok(format!(
                "反思状态：{:?}，目标版本 {}。原待办仍未完成；旧草稿不作为当前建议，不会自动重试不确定的执行。",
                child.status, parent.revision
            ));
        }
        let execution = child.execution.as_ref().ok_or_else(failure)?;
        let session = SessionKey::new(&execution.session_id, user).map_err(|_| failure())?;
        let text = self
            .sessions
            .as_ref()
            .ok_or_else(failure)?
            .snapshot(&session)
            .map_err(|_| failure())?
            .and_then(|s| {
                s.turns
                    .into_iter()
                    .find(|turn| Some(turn.id) == execution.turn_id)
            })
            .and_then(|turn| match turn.status {
                SessionTurnStatus::Completed { messages } => messages
                    .into_iter()
                    .rev()
                    .find(|m| m.role == ChatRole::Assistant)
                    .and_then(|m| m.text),
                _ => None,
            })
            .ok_or_else(failure)?;
        let artifact = ReflectionArtifact::parse(&text).map_err(|_| failure())?;
        Ok(format!(
            "反思草稿（建议尚未验证，原待办未完成）：\n目标版本：{}\n{}\n建议下一步：{}\n需要补充信息：{}",
            parent.revision,
            artifact.summary,
            artifact.next_step,
            if artifact.needs_user_input {
                "是"
            } else {
                "否"
            }
        ))
    }
}
impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let text = input.text.trim();
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        let (command, tail) = text.split_at(end);
        let tail = tail.trim();
        if !matches!(command, "/goal" | "/goals" | "/mind" | "/goal-feedback") {
            return Ok(None);
        }
        if self.admin.is_none() {
            return Ok(Some("内生反思未启用。".into()));
        }
        if !self.accepting.load(Ordering::SeqCst) {
            return Ok(Some("内生反思正在停止，请稍后重试。".into()));
        }
        let reply = match command {
            "/goal" if !tail.is_empty() => self.add(&input, tail)?,
            "/goal-feedback" => self.feedback(&input, tail)?,
            "/goals" if tail.is_empty() => self.goals(&input.session.user_id)?,
            "/mind" if tail.split_whitespace().count() <= 1 => {
                self.mind(&input.session.user_id, (!tail.is_empty()).then_some(tail))?
            }
            _ => HELP.into(),
        };
        Ok(Some(reply))
    }
}

/// 反思只接收目标输入及 AGENT 身份，不读取聊天训练、偏好或聊天历史。
struct ReflectionContext;
impl ContextAssembler for ReflectionContext {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "qq-reflection-1".into(),
                profile: String::new(),
                memories: vec![],
                history: vec![],
            })
        })
    }
}
struct ContextPlugin(PluginManifest);
impl Plugin for ContextPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.0
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            context.provide_service(
                ServiceId::new(CONTEXT_SERVICE)?,
                ContextService(Arc::new(ReflectionContext)),
            )?;
            Ok(None)
        })
    }
}
/// QQ 通道尚未启动时拒绝议程；只允许当前 QQ 待办及已授权学习目标的子目标进入执行。
struct QqPolicy {
    enabled: Arc<AtomicBool>,
    admin: CognitionController,
    parents: ExecutionScope,
}
impl DrivePolicy for QqPolicy {
    fn rank(&self, goals: &[Goal], now_ms: u64) -> LoopResult<Vec<RankedGoal>> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Ok(vec![]);
        }
        let snapshot = self.admin.snapshot()?;
        self.rank_with_state(&snapshot, goals, now_ms)
    }

    fn rank_with_state(
        &self,
        snapshot: &CognitiveSnapshot,
        goals: &[Goal],
        now_ms: u64,
    ) -> LoopResult<Vec<RankedGoal>> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Ok(vec![]);
        }
        // QQ 待办必须属于明确用户；宿主的 Internal 读取范围不扩大此准入条件。
        let user_goals: Vec<_> = goals
            .iter()
            .filter(|goal| {
                matches!(goal.visibility, Visibility::User(_))
                    && snapshot
                        .state
                        .goals
                        .get(&goal.source.reference)
                        .is_some_and(|parent| parent.visibility == goal.visibility)
            })
            .cloned()
            .collect();
        ReflectionDrivePolicy::new(self.parents.clone())?.rank_with_state(
            snapshot,
            &user_goals,
            now_ms,
        )
    }
}

pub(crate) struct Background {
    pub commands: Arc<Commands>,
    enabled: Arc<AtomicBool>,
    controller: LoopController,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn activate(&self) {
        self.enabled.store(true, Ordering::SeqCst);
        self.commands.accepting.store(true, Ordering::SeqCst);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    /// 兴趣学习派生器需要写入自身来源的学习目标；只交给同一受信宿主。
    pub(crate) fn admin(&self) -> Result<CognitionController, AppError> {
        Ok(self.commands.admin.clone().ok_or("认知管理句柄缺失")?)
    }
    /// 本机面板只取得绑定 Internal 的读取句柄，不持有可写的管理能力。
    pub(crate) fn reader(&self) -> Result<Arc<dyn CognitionReader>, AppError> {
        let admin = self.commands.admin.as_ref().ok_or("认知管理句柄缺失")?;
        Ok(admin.reader(ReadAccess::Internal)?)
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.commands.accepting.store(false, Ordering::SeqCst);
        self.enabled.store(false, Ordering::SeqCst);
        self.stop.send_replace(true);
        // 即使规划任务 panic，仍先取消、等待循环，再允许 Kernel 关闭。
        let settled = self.controller.shutdown().await;
        let joined = self
            .task
            .await
            .map_err(|_| -> AppError { "认知后台任务异常".into() });
        match (joined, settled) {
            (Ok(Ok(())), Ok(())) => Ok(()),
            (Err(error) | Ok(Err(error)), _) => Err(error),
            (_, Err(error)) => Err(error.into()),
        }
    }
}

pub(crate) async fn start(
    kernel: &Kernel,
    backends: &KernelServices,
    agent: &std::path::Path,
    max: u16,
    factory: Arc<dyn EndogenousPlannerFactory>,
    interest_goals: bool,
) -> Result<Background, AppError> {
    let plugin = CognitionPlugin::new("eve")?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&PluginId::new(COGNITION_PLUGIN_ID)?).await?;
    let settings = backends
        .registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
        .ok_or("配置服务缺失")?
        .value
        .downcast::<ConfigServiceHandle>()
        .map_err(|_| "配置类型错误")?;
    let bootstrap = core_bootstrap(agent)?;
    let resolver = Arc::new(models::CoreModelResolver::new(
        settings.0.clone(),
        bootstrap.api_key,
    ));
    let selected = resolver.resolve()?;
    kernel.register(Box::new(ContextPlugin(PluginManifest::new(
        CONTEXT_ID,
        env!("CARGO_PKG_VERSION"),
    )?)))?;
    kernel.start(&PluginId::new(CONTEXT_ID)?).await?;
    let host = LlmHost::new(
        selected.provider,
        backends.registry.clone(),
        kernel.clone(),
        backends.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT_SERVICE)?,
            expected_owner: PluginId::new(CONTEXT_ID)?,
        },
        vec![],
        LlmHostConfig {
            provider_timeout: selected.provider_timeout.min(Duration::from_secs(30)),
            output_format: "只返回严格JSON对象：summary和next_step为非空字符串，needs_user_input为布尔值。只形成反思草稿，不调用工具，不声称现实目标已经完成。".into(),
            ..bootstrap.host_config
        },
    )?.with_model_resolver(resolver);
    let runner = Arc::new(BudgetedSessionRunner::new(Arc::new(
        SessionLlmHost::new(host, SessionBinding::builtin()).with_logger(backends.logger.clone()),
    )));
    let deps = |names: &[&str]| -> Result<Vec<PluginDependency>, AppError> {
        names
            .iter()
            .map(|name| {
                Ok(PluginDependency {
                    id: PluginId::new(*name)?,
                    requirement: Some("^0.1".into()),
                })
            })
            .collect()
    };
    kernel.register(Box::new(ControlPlugin::with_identity(
        CONTROL_ID,
        CONTROL_SERVICE,
        runner.clone(),
        deps(&[services::OWNER, CONTEXT_ID, SESSION_PLUGIN_ID])?,
    )?))?;
    kernel.start(&PluginId::new(CONTROL_ID)?).await?;
    let control = backends
        .registry
        .get(&ServiceId::new(CONTROL_SERVICE)?)?
        .ok_or("内部控制服务缺失")?
        .value
        .downcast::<ControlServiceHandle>()
        .map_err(|_| "内部控制服务类型错误")?
        .0
        .clone();
    let executor = Arc::new(ControlGoalExecutor::new(control, runner, INTERNAL_USER)?);
    let planner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        factory.create(
            Arc::new(admin.clone()),
            EndogenousOptions {
                scope: reflection_scope(interest_goals),
                max_derivations: max,
                timeout_ms: 30_000,
            },
        )
    }))
    .map_err(|_| "认知规划器创建异常")??;
    let enabled = Arc::new(AtomicBool::new(false));
    let plugin = CognitionLoopPlugin::new(
        Arc::new(admin.clone()),
        Arc::new(QqPolicy {
            enabled: enabled.clone(),
            admin: admin.clone(),
            parents: reflection_scope(interest_goals),
        }),
        Arc::new(ReflectionVerifier),
        executor,
        LoopOptions {
            scope: scope(SourceKind::Inference, "endogenous"),
            poll_interval_ms: 60_000,
            max_executions: max,
        },
        deps(&[COGNITION_PLUGIN_ID, CONTROL_ID])?,
    )?;
    let controller = plugin.controller();
    kernel.register(Box::new(plugin))?;
    let sessions = backends
        .registry
        .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
        .ok_or("会话服务缺失")?
        .value
        .downcast::<SessionServiceHandle>()
        .map_err(|_| "会话类型错误")?
        .0
        .clone();
    let commands = Arc::new(Commands {
        admin: Some(admin),
        sessions: Some(sessions),
        accepting: Arc::new(AtomicBool::new(false)),
        planning_budget: Some((controller.clone(), max)),
    });
    let (stop, mut stopping) = watch::channel(false);
    let accepting = commands.accepting.clone();
    let active = enabled.clone();
    let (done, finished) = watch::channel(false);
    kernel.start(&PluginId::new(LOOP_PLUGIN_ID)?).await?;
    let worker = controller.clone();
    let task = tokio::spawn(async move {
        let result: Result<(), AppError> = async {
            let mut timer = tokio::time::interval(Duration::from_millis(250));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {biased;_ = stopping.changed()=>break,_=timer.tick()=>{}}
                if *stopping.borrow() {
                    break;
                }
                if !active.load(Ordering::SeqCst) {
                    continue;
                }
                let time = now_ms()?;
                let planned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    planner.reconcile(time)
                }))
                .map_err(|_| "认知规划器异常")?;
                match planned {
                    Ok(_) => {}
                    Err(LoopError::Cognition(CognitionError::StaleRevision)) => continue,
                    Err(error) => return Err(error.into()),
                }
                // 收尾会并发关闭循环准入；本轮已无须唤醒，不能把它当作循环故障。
                let stats = match worker.stats() {
                    Err(LoopError::Unavailable) if *stopping.borrow() => break,
                    stats => stats?,
                };
                if stats.feedback_save_failures > 0 {
                    return Err("反思反馈保存失败".into());
                }
                match worker.wake(WakeReason::StateChanged) {
                    Err(LoopError::Unavailable) if *stopping.borrow() => break,
                    woken => woken?,
                };
            }
            Ok(())
        }
        .await;
        accepting.store(false, Ordering::SeqCst);
        let settled = worker.shutdown().await;
        done.send_replace(true);
        // 循环自身先失败时，规划任务随后只会看到“不可用”；优先报告循环的真实错误。
        match (result, settled) {
            (Ok(()), Ok(())) => Ok(()),
            (_, Err(e)) => Err(e.into()),
            (Err(e), Ok(())) => Err(e),
        }
    });
    Ok(Background {
        commands,
        enabled,
        controller,
        stop,
        finished,
        task,
    })
}

#[cfg(test)]
mod agenda_policy_tests {
    use super::*;
    use eve_cognition_loop_plugin::EndogenousPlanner;

    fn parent(id: &str, visibility: Visibility) -> Goal {
        Goal {
            id: id.into(),
            revision: 0,
            source: Source {
                kind: SourceKind::User,
                channel: CHANNEL.into(),
                reference: id.into(),
            },
            visibility,
            description: "整理待办的下一步".into(),
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
            wait_reason: Some("等待反思".into()),
            block_reason: None,
            execution: None,
            feedback: None,
        }
    }

    #[tokio::test]
    async fn qq_agenda_preserves_user_only_admission_and_supplied_snapshot() {
        let kernel = Kernel::with_services(KernelServices::default());
        let plugin = CognitionPlugin::new("eve").unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .start(&PluginId::new(COGNITION_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let initial = admin.snapshot().unwrap();
        let mut state = initial.state;
        for goal in [
            parent("public", Visibility::Public),
            parent("internal", Visibility::Internal),
            parent("user", Visibility::User("qq-user".into())),
        ] {
            state.goals.insert(goal.id.clone(), goal);
        }
        admin.replace(initial.revision, state).unwrap();
        let planner = EndogenousPlanner::new(
            Arc::new(admin.clone()),
            EndogenousOptions {
                scope: scope(SourceKind::User, CHANNEL),
                max_derivations: 3,
                timeout_ms: 30_000,
            },
        )
        .unwrap();
        for _ in 0..3 {
            assert_eq!(planner.reconcile(1_000).unwrap().created_goal_ids.len(), 1);
        }
        let snapshot = admin.snapshot().unwrap();
        let goals: Vec<_> = snapshot
            .state
            .goals
            .values()
            .filter(|goal| goal.status == GoalStatus::Ready)
            .cloned()
            .collect();
        assert_eq!(goals.len(), 3);
        // 关闭管理句柄后仍须只使用传入快照，不能再读取一次插件状态。
        kernel.stop_all().await.unwrap();
        assert!(admin.snapshot().is_err());
        let enabled = Arc::new(AtomicBool::new(true));
        let policy = QqPolicy {
            enabled: enabled.clone(),
            admin,
            parents: reflection_scope(false),
        };
        let ranked = policy.rank_with_state(&snapshot, &goals, 1_000).unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(
            snapshot.state.goals[&ranked[0].goal_id].source.reference,
            "user"
        );
        enabled.store(false, Ordering::SeqCst);
        assert!(
            policy
                .rank_with_state(&snapshot, &goals, 1_000)
                .unwrap()
                .is_empty()
        );
    }
}
