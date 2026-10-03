//! 本地内生反思宿主：用户目标保持 Waiting，只对派生草稿安装受限执行能力。
use crate::{AppError, AppFailure, config, core_bootstrap, models, services};
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{
    CognitionLoopPlugin, LoopController, PriorityDrivePolicy, ReflectionArtifact,
    ReflectionPlannerFactory, ReflectionVerifier,
};
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigServiceHandle, LLM_NAMESPACE, LlmRuntimeConfig, model_roles_schema,
    runtime_llm_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CONTROL_PLUGIN_ID, CONTROL_SERVICE_ID, ControlServiceHandle};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatRole, LlmModelResolver, ResponseMode};
use eve_plugin_api::{PluginDependency, PluginId, ServiceId, ServiceRegistry};
use eve_runtime::{
    BudgetedSessionRunner, ContextBinding, ControlGoalExecutor, LlmHost, LlmHostConfig,
    SessionBinding, SessionLlmHost,
};
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionKey, SessionServiceHandle, SessionTurnStatus,
};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const COGNITION_HELP: &str = "Eve 本地内生反思入口
用法：eve-cognition [--state-dir 目录] [--agent AGENT.md] 命令
  add --id ID --text 目标文字 [--user owner]  保存等待中的用户目标，不调用模型
  status                                  查看无正文的状态计数
  show --id ID                            显式查看本地目标和已保存反思草稿
  run [--seconds 30] [--max-executions 1]   无需新输入，推进已有目标的反思草稿
状态目录默认 .eve-cognition；运行窗口 1 至 600 秒，最多执行 1 至 32 项。
仅有可执行反思时才需要 EVE_OPENAI_API_KEY 和主模型配置；沿用 AGENT.md。
每项反思最多一次模型请求、零工具、一次尝试、30 秒；父目标仍等待用户处理。
Ctrl+C 或 SIGTERM 停止派生，取消并等待保存，然后关闭插件。";

const SUBJECT: &str = "eve";
const INPUT_CHANNEL: &str = "cognition.cli";
const INTERNAL_USER: &str = "cognition.internal";
const POLL_MS: u64 = 250;

#[derive(Clone, Debug)]
enum CognitionCommand {
    Add {
        id: String,
        text: String,
        user: String,
    },
    Status,
    Show {
        id: String,
    },
    Run {
        seconds: u64,
        max_executions: u16,
    },
}

#[derive(Clone, Debug)]
pub struct CognitionOptions {
    pub state_directory: PathBuf,
    pub agent_path: PathBuf,
    command: CognitionCommand,
}

impl CognitionOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut directory = None;
        let mut agent = None;
        let mut command = None;
        let mut fields = std::collections::BTreeMap::<String, String>::new();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            let arg = arg.into_string().map_err(|_| "命令参数必须为 UTF-8。")?;
            if ["add", "status", "show", "run"].contains(&arg.as_str()) {
                if command.replace(arg).is_some() {
                    return Err("只能指定一个认知命令。".into());
                }
                continue;
            }
            if ![
                "--state-dir",
                "--agent",
                "--id",
                "--text",
                "--user",
                "--seconds",
                "--max-executions",
            ]
            .contains(&arg.as_str())
            {
                return Err("未知认知命令或参数；使用 --help。".into());
            }
            let value = args.next().ok_or("认知参数缺少值。")?;
            if value.is_empty() {
                return Err("认知参数值不能为空。".into());
            }
            let duplicate = match arg.as_str() {
                "--state-dir" => directory.replace(PathBuf::from(value)).is_some(),
                "--agent" => agent.replace(PathBuf::from(value)).is_some(),
                _ => fields
                    .insert(
                        arg,
                        value.into_string().map_err(|_| "参数值必须为 UTF-8。")?,
                    )
                    .is_some(),
            };
            if duplicate {
                return Err("认知参数不得重复。".into());
            }
        }
        let command = match command.as_deref().ok_or("缺少认知命令；使用 --help。")? {
            "add" => {
                let id = fields.remove("--id").ok_or("add 缺少 --id。")?;
                let text = fields.remove("--text").ok_or("add 缺少 --text。")?;
                let user = fields.remove("--user").unwrap_or_else(|| "owner".into());
                validate_id(&id)?;
                validate_text(&text)?;
                validate_id(&user)?;
                CognitionCommand::Add { id, text, user }
            }
            "status" => CognitionCommand::Status,
            "show" => {
                let id = fields.remove("--id").ok_or("show 缺少 --id。")?;
                validate_id(&id)?;
                CognitionCommand::Show { id }
            }
            "run" => {
                let seconds = fields
                    .remove("--seconds")
                    .unwrap_or_else(|| "30".into())
                    .parse::<u64>()
                    .map_err(|_| "运行秒数必须为整数。")?;
                let max_executions = fields
                    .remove("--max-executions")
                    .unwrap_or_else(|| "1".into())
                    .parse::<u16>()
                    .map_err(|_| "执行上限必须为整数。")?;
                if !(1..=600).contains(&seconds) || !(1..=32).contains(&max_executions) {
                    return Err("运行秒数须在 1 至 600，执行上限须在 1 至 32。".into());
                }
                CognitionCommand::Run {
                    seconds,
                    max_executions,
                }
            }
            _ => unreachable!(),
        };
        if !fields.is_empty() {
            return Err("当前认知命令不接受所给参数。".into());
        }
        Ok(Some(Self {
            state_directory: directory.unwrap_or_else(|| ".eve-cognition".into()),
            agent_path: agent.unwrap_or_else(|| "AGENT.md".into()),
            command,
        }))
    }
}

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn scope(kind: SourceKind, channel: &str) -> ExecutionScope {
    ExecutionScope {
        subject_id: SUBJECT.into(),
        access: ReadAccess::Internal,
        sources: vec![AllowedSource {
            kind,
            channel: channel.into(),
        }],
    }
}

fn status(command: &str, snapshot: &CognitiveSnapshot) -> Value {
    let count = |status| {
        snapshot
            .state
            .goals
            .values()
            .filter(|goal| goal.status == status)
            .count()
    };
    json!({"command": command, "revision": snapshot.revision, "goals": {
        "total": snapshot.state.goals.len(), "waiting": count(GoalStatus::Waiting),
        "ready": count(GoalStatus::Ready), "executing": count(GoalStatus::Executing),
        "completed": count(GoalStatus::Completed), "cancelled": count(GoalStatus::Cancelled),
        "blocked": count(GoalStatus::Blocked)
    }})
}

fn add(
    admin: &CognitionController,
    id: String,
    text: String,
    user: String,
) -> Result<Value, AppError> {
    let snapshot = admin.snapshot()?;
    if snapshot.state.goals.contains_key(&id) {
        return Err("该目标 ID 已存在，未覆盖原状态。".into());
    }
    let source = Source {
        kind: SourceKind::User,
        channel: INPUT_CHANNEL.into(),
        reference: id.clone(),
    };
    let visibility = Visibility::User(user);
    let goal = Goal {
        id: id.clone(),
        revision: 0,
        source: source.clone(),
        visibility: visibility.clone(),
        description: text,
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
        wait_reason: Some("等待本地反思草稿与用户处理；尚未完成现实目标。".into()),
        block_reason: None,
        execution: None,
        feedback: None,
    };
    let mut state = snapshot.state;
    state.goals.insert(id.clone(), goal);
    state.events.push(CognitiveEvent {
        id: format!(
            "eve.cli.input.{}",
            snapshot.revision.checked_add(1).ok_or("修订已达上限。")?
        ),
        kind: CognitiveEventKind::ExternalInput,
        source,
        visibility,
        goal_id: Some(id.clone()),
        caused_by: None,
        at_ms: now_ms()?,
        summary: "用户目标已保存，等待反思草稿。".into(),
    });
    let saved = admin.replace(snapshot.revision, state)?;
    let mut report = status("add", &saved);
    report["goal_id"] = json!(id);
    Ok(report)
}

async fn show(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
    admin: &CognitionController,
    id: &str,
) -> Result<Value, AppError> {
    let snapshot = admin.snapshot()?;
    let goal = snapshot.state.goals.get(id).ok_or("未找到指定目标。")?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    kernel.start(&PluginId::new(SESSION_PLUGIN_ID)?).await?;
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
        .ok_or("会话服务缺失。")?
        .value
        .downcast::<SessionServiceHandle>()
        .map_err(|_| "会话服务类型错误。")?;
    let mut reflections = Vec::new();
    for child in snapshot.state.goals.values().filter(|child| {
        child.source.kind == SourceKind::Inference
            && child.source.channel == "endogenous"
            && (child.source.reference == id || child.id == id)
    }) {
        let verified = child.status == GoalStatus::Completed
            && child
                .feedback
                .as_ref()
                .is_some_and(|feedback| feedback.verification_met);
        let artifact = if verified {
            let execution = child.execution.as_ref().ok_or("已验证草稿缺少执行记录。")?;
            let user = match &child.visibility {
                Visibility::User(user) => user.as_str(),
                _ => INTERNAL_USER,
            };
            let key = SessionKey::new(&execution.session_id, user)?;
            let text = sessions
                .0
                .snapshot(&key)?
                .and_then(|session| {
                    session
                        .turns
                        .into_iter()
                        .find(|turn| Some(turn.id) == execution.turn_id)
                })
                .and_then(|turn| match turn.status {
                    SessionTurnStatus::Completed { messages } => messages
                        .into_iter()
                        .rev()
                        .find(|message| message.role == ChatRole::Assistant)
                        .and_then(|message| message.text),
                    _ => None,
                })
                .ok_or("已验证草稿的会话结果缺失，保留状态等待检查。")?;
            Some(
                ReflectionArtifact::parse(&text)
                    .map_err(|_| "已验证草稿格式不一致，保留状态等待检查。")?,
            )
        } else {
            None
        };
        reflections.push(json!({"goal": child, "artifact": artifact}));
    }
    Ok(json!({"command": "show", "goal": goal, "reflections": reflections}))
}

async fn interrupted() -> Result<(), AppError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn start_loop(
    kernel: &Kernel,
    backends: &KernelServices,
    options: &CognitionOptions,
    admin: &CognitionController,
    max_executions: u16,
) -> Result<LoopController, AppError> {
    let bootstrap = core_bootstrap(&options.agent_path)?;
    let plugin = ConfigPlugin::new(ConfigBootstrap::new(
        options.state_directory.join("configuration"),
        vec![
            runtime_llm_schema(),
            config::openai_schema(),
            model_roles_schema(),
        ],
    ))?;
    kernel.register(Box::new(plugin))?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    kernel.register(Box::new(services::CoreServices::new()?))?;
    let owner = PluginId::new(services::OWNER)?;
    kernel.start(&owner).await?;
    let settings = backends
        .registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
        .ok_or("配置服务缺失。")?
        .value
        .downcast::<ConfigServiceHandle>()
        .map_err(|_| "配置服务类型错误。")?;
    let request = settings.0.begin_request(LLM_NAMESPACE, 1)?;
    let runtime = LlmRuntimeConfig::try_from(&settings.0.read_request(&request)?)?;
    if runtime.response_mode != "complete" {
        return Err("内生反思当前要求 complete 模式。".into());
    }
    let resolver = Arc::new(models::CoreModelResolver::new(
        settings.0.clone(),
        bootstrap.api_key,
    ));
    let selected = resolver.resolve()?;
    let host = LlmHost::new(selected.provider, backends.registry.clone(), kernel.clone(), backends.permissions.clone(),
        ContextBinding { service_id: ServiceId::new(services::CONTEXT)?, expected_owner: owner }, vec![],
        LlmHostConfig { provider_timeout: selected.provider_timeout.min(Duration::from_secs(30)),
            output_format: "只返回一个严格 JSON 对象，恰有 summary、next_step 两个非空字符串和 needs_user_input 布尔值；两字符串合计不超过8192 UTF-8字节。产物只是本地反思草稿，不声称目标已完成，不调用工具。".into(),
            response_mode: ResponseMode::Complete, ..bootstrap.host_config })?.with_model_resolver(resolver);
    let runner = Arc::new(BudgetedSessionRunner::new(Arc::new(
        SessionLlmHost::new(host, SessionBinding::builtin()).with_logger(backends.logger.clone()),
    )));
    let dependencies = |names: &[&str]| -> Result<Vec<PluginDependency>, AppError> {
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
    kernel.register(Box::new(ControlPlugin::new(
        runner.clone(),
        dependencies(&[services::OWNER, SESSION_PLUGIN_ID])?,
    )?))?;
    kernel.start(&PluginId::new(CONTROL_PLUGIN_ID)?).await?;
    let control = backends
        .registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID)?)?
        .ok_or("控制服务缺失。")?
        .value
        .downcast::<ControlServiceHandle>()
        .map_err(|_| "控制服务类型错误。")?
        .0
        .clone();
    let executor = Arc::new(ControlGoalExecutor::new(control, runner, INTERNAL_USER)?);
    let plugin = CognitionLoopPlugin::new(
        Arc::new(admin.clone()),
        Arc::new(PriorityDrivePolicy),
        Arc::new(ReflectionVerifier),
        executor,
        LoopOptions {
            scope: scope(SourceKind::Inference, "endogenous"),
            poll_interval_ms: 60_000,
            max_executions,
        },
        dependencies(&[COGNITION_PLUGIN_ID, CONTROL_PLUGIN_ID])?,
    )?;
    let controller = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&PluginId::new(LOOP_PLUGIN_ID)?).await?;
    Ok(controller)
}

fn loop_report(stats: LoopStats) -> Value {
    json!({"submitted": stats.submitted, "completed": stats.completed, "cancelled": stats.cancelled,
        "blocked": stats.blocked, "model_requests": stats.model_requests, "admitted_tool_calls": stats.admitted_tool_calls,
        "started_tools": stats.started_tools, "feedback_save_failures": stats.feedback_save_failures,
        "notification_failures": stats.notification_failures, "wakes": stats.wakes,
        "evaluations": stats.evaluations, "idle_ticks": stats.idle_ticks, "active": stats.active})
}

fn combine<T>(result: Result<T, AppError>, extra: Vec<AppError>) -> Result<T, AppError> {
    let mut errors = extra.into_iter();
    let Some(first) = errors.next() else {
        return result;
    };
    let (primary, secondary) = match result {
        Ok(_) => (first, errors.collect()),
        Err(primary) => (primary, std::iter::once(first).chain(errors).collect()),
    };
    Err(AppFailure { primary, secondary }.into())
}

async fn run_window(
    kernel: &Kernel,
    backends: &KernelServices,
    admin: &CognitionController,
    options: &CognitionOptions,
    seconds: u64,
    max_executions: u16,
    factory: &dyn EndogenousPlannerFactory,
) -> Result<Value, AppError> {
    let planner_options = EndogenousOptions {
        scope: scope(SourceKind::User, INPUT_CHANNEL),
        max_derivations: max_executions,
        timeout_ms: 30_000,
    };
    planner_options.validate()?;
    let planner = factory.create(Arc::new(admin.clone()), planner_options)?;
    let began = tokio::time::Instant::now();
    let deadline = began + Duration::from_secs(seconds);
    let stop = interrupted();
    tokio::pin!(stop);
    let mut timer = tokio::time::interval(Duration::from_millis(POLL_MS));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut controller: Option<LoopController> = None;
    let mut was_interrupted = false;
    let result: Result<(), AppError> = async {
        loop {
            tokio::select! {
                biased;
                signal = &mut stop => { signal?; was_interrupted = true; break; }
                _ = tokio::time::sleep_until(deadline) => break,
                _ = timer.tick() => {}
            }
            match planner.reconcile(now_ms()?) {
                Ok(_) => {}
                Err(LoopError::Cognition(CognitionError::StaleRevision)) => continue,
                Err(error) => return Err(error.into()),
            }
            if let Some(handle) = &controller {
                let stats = handle.stats()?;
                if stats.feedback_save_failures > 0 {
                    return Err("反思反馈保存失败；已保留原状态。".into());
                }
                if stats.submitted >= u64::from(max_executions) {
                    continue;
                }
            }
            let current_time = now_ms()?;
            if controller.is_none()
                && admin.snapshot()?.state.goals.values().any(|goal| {
                    goal.is_ready(current_time)
                        && goal.source.kind == SourceKind::Inference
                        && goal.source.channel == "endogenous"
                        && goal.verification == "reflection:v1"
                })
            {
                controller =
                    Some(start_loop(kernel, backends, options, admin, max_executions).await?);
            }
            if let Some(handle) = &controller {
                handle.wake(WakeReason::StateChanged)?;
            }
        }
        Ok(())
    }
    .await;
    // ticker 已退出；关闭循环准入并等待取消保存后才允许 Kernel 停止。
    let mut errors = Vec::new();
    let stats = if let Some(handle) = controller {
        if let Err(error) = handle.shutdown().await {
            errors.push(Box::new(error) as AppError);
        }
        match handle.stats() {
            Ok(stats) => stats,
            Err(error) => {
                errors.push(Box::new(error));
                LoopStats::default()
            }
        }
    } else {
        LoopStats::default()
    };
    combine(result, errors)?;
    let mut report = status("run", &admin.snapshot()?);
    report["elapsed_ms"] = json!(began.elapsed().as_millis());
    report["interrupted"] = json!(was_interrupted);
    report["loop"] = loop_report(stats);
    Ok(report)
}

pub async fn run_cognition(options: CognitionOptions) -> Result<Value, AppError> {
    run_cognition_with_planner_factory(options, Arc::new(ReflectionPlannerFactory)).await
}

/// 受信 Rust 宿主的规划器注入入口；CLI 默认使用内置反思策略。
/// 工厂仅在 run 中、认知状态恢复后调用；错误仍经过同一插件停止和日志收尾。
pub async fn run_cognition_with_planner_factory(
    options: CognitionOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
) -> Result<Value, AppError> {
    let backends = KernelServices {
        state: Arc::new(FileStateStore::open(&options.state_directory)?),
        ..KernelServices::default()
    };
    let kernel = Kernel::with_services(KernelServices {
        events: backends.events.clone(),
        registry: backends.registry.clone(),
        state: backends.state.clone(),
        permissions: backends.permissions.clone(),
        tasks: backends.tasks.clone(),
        logger: backends.logger.clone(),
    });
    let plugin = CognitionPlugin::new(SUBJECT)?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    let result = async {
        kernel.start(&PluginId::new(COGNITION_PLUGIN_ID)?).await?;
        match options.command.clone() {
            CognitionCommand::Add { id, text, user } => add(&admin, id, text, user),
            CognitionCommand::Status => Ok(status("status", &admin.snapshot()?)),
            CognitionCommand::Show { id } => show(&kernel, &backends.registry, &admin, &id).await,
            CognitionCommand::Run {
                seconds,
                max_executions,
            } => {
                run_window(
                    &kernel,
                    &backends,
                    &admin,
                    &options,
                    seconds,
                    max_executions,
                    factory.as_ref(),
                )
                .await
            }
        }
    }
    .await;
    let mut errors = Vec::new();
    if let Err(error) = kernel.stop_all().await {
        errors.push(Box::new(error) as AppError);
    }
    for name in [LOOP_PLUGIN_ID, CONTROL_PLUGIN_ID] {
        let id = PluginId::new(name)?;
        if kernel.state(&id).is_some()
            && let Err(error) = kernel.unregister(&id)
        {
            errors.push(Box::new(error));
        }
    }
    if let Err(error) = kernel.flush_logs() {
        errors.push(Box::new(error));
    }
    combine(result, errors)
}
