//! QQ 只通过明确命令保存待办和查询草稿，内部反思没有主动投递能力。
use crate::{AppError, config, core_bootstrap, models, services};
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{
    CognitionLoopPlugin, EndogenousPlanner, LoopController, PriorityDrivePolicy,
    ReflectionArtifact, ReflectionVerifier,
};
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_config_api::{CONFIG_SERVICE_ID, ConfigServiceHandle};
use eve_control_api::ControlServiceHandle;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatRole, LlmModelResolver};
use eve_plugin_api::{PluginDependency, PluginError, PluginId, PluginResult, ServiceId};
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
const CONTROL_SERVICE: &str = "eve.cognition.control.service";
const CHANNEL: &str = "qq.goal";
const INTERNAL_USER: &str = "cognition.internal";
const HELP: &str = "用法：/goal 待办内容、/goals、/mind [目标ID]。";

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
}
impl Commands {
    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self {
            admin: None,
            sessions: None,
            accepting: Arc::new(AtomicBool::new(false)),
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
            return Ok(format!("待办已保存：{id}。发送 /mind 查看草稿。"));
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
            Ok(_) => Ok(format!(
                "待办已保存：{id}。正在后台整理，稍后发送 /mind 查看草稿。"
            )),
            Err(CognitionError::StaleRevision) => {
                Ok("认知状态正在更新，本条待办未保存；请重新发送 /goal。".into())
            }
            Err(CognitionError::LimitReached) => Ok("认知记录已达容量上限，未保存新待办。".into()),
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
                .map(|g| format!("{}：{:?}", g.id, g.status))
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
        let child = view
            .state
            .goals
            .values()
            .filter(|g| {
                g.source.kind == SourceKind::Inference
                    && g.source.channel == "endogenous"
                    && g.source.reference == parent.id
                    && g.visibility == parent.visibility
            })
            .max_by_key(|g| g.feedback.as_ref().map_or(0, |f| f.at_ms));
        let Some(child) = child else {
            return Ok("待办已保存，反思尚未开始；请稍后发送 /mind。".into());
        };
        if child.status != GoalStatus::Completed
            || !child.feedback.as_ref().is_some_and(|f| f.verification_met)
        {
            return Ok(format!(
                "反思状态：{:?}。原待办仍未完成；不会自动重试不确定的执行。",
                child.status
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
            "反思草稿（建议尚未验证，原待办未完成）：\n{}\n建议下一步：{}\n需要补充信息：{}",
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
        if !matches!(command, "/goal" | "/goals" | "/mind") {
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
            "/goals" if tail.is_empty() => self.goals(&input.session.user_id)?,
            "/mind" if tail.split_whitespace().count() <= 1 => {
                self.mind(&input.session.user_id, (!tail.is_empty()).then_some(tail))?
            }
            _ => HELP.into(),
        };
        Ok(Some(reply))
    }
}

pub(crate) struct Background {
    pub commands: Arc<Commands>,
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.commands.accepting.store(false, Ordering::SeqCst);
        self.stop.send_replace(true);
        self.task.await.map_err(|_| "认知后台任务异常")?
    }
}

pub(crate) async fn start(
    kernel: &Kernel,
    backends: &KernelServices,
    agent: &std::path::Path,
    max: u16,
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
    let host=LlmHost::new(selected.provider,backends.registry.clone(),kernel.clone(),backends.permissions.clone(),ContextBinding{service_id:ServiceId::new(services::CONTEXT)?,expected_owner:PluginId::new(services::OWNER)?},vec![],
        LlmHostConfig{provider_timeout:selected.provider_timeout.min(Duration::from_secs(30)),output_format:"只返回严格JSON对象：summary和next_step为非空字符串，needs_user_input为布尔值。只形成反思草稿，不调用工具，不声称现实目标已经完成。".into(),..bootstrap.host_config})?.with_model_resolver(resolver);
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
        deps(&[services::OWNER, SESSION_PLUGIN_ID])?,
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
    let planner = EndogenousPlanner::new(
        Arc::new(admin.clone()),
        EndogenousOptions {
            scope: scope(SourceKind::User, CHANNEL),
            max_derivations: max,
            timeout_ms: 30_000,
        },
    )?;
    planner.reconcile(now_ms()?)?;
    let plugin = CognitionLoopPlugin::new(
        Arc::new(admin.clone()),
        Arc::new(PriorityDrivePolicy),
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
    kernel.start(&PluginId::new(LOOP_PLUGIN_ID)?).await?;
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
        accepting: Arc::new(AtomicBool::new(true)),
    });
    let (stop, mut stopping) = watch::channel(false);
    let accepting = commands.accepting.clone();
    let task = tokio::spawn(async move {
        let result: Result<(), AppError> = async {
            let mut timer = tokio::time::interval(Duration::from_millis(250));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {biased;_ = stopping.changed()=>break,_=timer.tick()=>{}}
                if *stopping.borrow() {
                    break;
                }
                match planner.reconcile(now_ms()?) {
                    Ok(_) => {}
                    Err(LoopError::Cognition(CognitionError::StaleRevision)) => continue,
                    Err(error) => return Err(error.into()),
                }
                if controller.stats()?.feedback_save_failures > 0 {
                    return Err("反思反馈保存失败".into());
                }
                controller.wake(WakeReason::StateChanged)?;
            }
            Ok(())
        }
        .await;
        accepting.store(false, Ordering::SeqCst);
        let settled = controller.shutdown().await;
        match (result, settled) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e.into()),
        }
    });
    Ok(Background {
        commands,
        stop,
        task,
    })
}
