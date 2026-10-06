//! Eve 的最小可运行组合入口：固定身份、一个主模型、会话历史与插件工具。
//! 输入与受管轮次并发，任务仍串行；不包含额外模型或 Web 控制面。
mod cognition;
mod cognition_action;
mod cognition_action_admission;
mod config;
mod console;
mod error;
mod input;
mod message_evaluation;
mod models;
mod qq_cognition;
mod qq_learning;
mod qq_learning_commands;
mod qq_memory;
mod qq_memory_observer;
mod qq_message_judge;
mod qqbot;
mod segment_advice;
mod segment_commands;
mod services;
mod storage;
mod web_panel;

#[cfg(test)]
#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

pub use cognition::{
    COGNITION_HELP, CognitionOptions, run_cognition, run_cognition_with_planner_factory,
};
pub use console::{ChatOutputError, ChatRunError};
pub use error::AppFailure;
pub use message_evaluation::{
    MESSAGE_EVALUATION_HELP, MessageEvaluationOptions, MessageEvaluationReport,
    message_evaluation_json, run_message_evaluation,
};
pub use qq_message_judge::MessageJudgeMode;
pub use qqbot::{
    QQBOT_HELP, QqBotOptions, run_qqbot, run_qqbot_with_components, run_qqbot_with_learning_policy,
    run_qqbot_with_planner_factory,
};

use eve_agent_prompt::FileAgentPrompt;
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigServiceHandle, LLM_NAMESPACE, LlmRuntimeConfig, model_roles_schema,
    runtime_llm_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CONTROL_PLUGIN_ID, CONTROL_SERVICE_ID, ControlServiceHandle};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{LlmModelResolver, ResponseMode, ToolBinding};
use eve_plugin_api::{PluginDependency, PluginId, ServiceId};
use eve_runtime::{
    ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionControlRunner, SessionLlmHost,
};
use eve_session_api::{SESSION_PLUGIN_ID, SessionKey};
use eve_session_plugin::SessionPlugin;
use std::{
    ffi::OsString,
    io::{BufRead, Write},
    path::PathBuf,
    sync::Arc,
};

pub type AppError = Box<dyn std::error::Error + Send + Sync>;
pub const HELP: &str = "Eve 核心对话入口
用法：eve [--state-dir 目录] [--database-config 文件] [--agent AGENT.md] [--session 会话] [--user 用户] [--segmented]
--database-config 显式选择本地 PostgreSQL；默认文件状态，已有状态目录不自动迁移。
主模型默认 deepseek-v4.1-flash，可用 EVE_OPENAI_MODEL 替换；凭据：EVE_OPENAI_API_KEY
协议默认 chat；EVE_OPENAI_PROTOCOL 可选 chat/responses\n可选：EVE_OPENAI_BASE_URL、EVE_OPENAI_REASONING_EFFORT
EVE_OPENAI_MODEL_ROLE=primary 显式使用 runtime.models 的主模型角色配置；每轮固定选择。
一行一轮；/cancel 取消当前轮；/quit 或 Ctrl+C 取消并退出；/help 查看说明。
--segmented 保存完整回复后按自然段分至多 3 段显示，段间停顿至多 2.5 秒；/cancel 停止显示剩余片段。
/segment 查看本会话分段；on、off、reset、parts 2至5、pace 0至200（%）按 --session/--user 保存，从下一条回复生效。
EOF 处理完已接收输入后退出；最多 16 条待处理输入，取消/退出会清空队列。
输入上限 32768 字节；当前入口使用非流式模式，串行执行和保存。";
const MAX_INPUT_BYTES: usize = 32768;
/// 终端与 QQ 默认一致：最多三段，段前停顿最长 2.5 秒；会话设置最多 5 段、停顿最长 5 秒。
const CONSOLE_SEGMENT_POLICY: eve_segment_api::SegmentPolicy = eve_segment_api::SegmentPolicy {
    defaults: eve_segment_api::SegmentLimits {
        max_segments: 3,
        max_segment_bytes: MAX_INPUT_BYTES,
        max_pause_ms: 2500,
    },
    max_segments: 5,
    max_pause_ms: 5000,
};
const CONSOLE_SEGMENT_CHANNEL: &str = "console";

#[derive(Clone, Debug)]
pub struct ChatOptions {
    pub state_directory: PathBuf,
    pub database_config: Option<PathBuf>,
    pub agent_path: PathBuf,
    pub session_id: String,
    pub user_id: String,
    pub segmented: bool,
}
impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            state_directory: ".eve".into(),
            database_config: None,
            agent_path: "AGENT.md".into(),
            session_id: "default".into(),
            user_id: "owner".into(),
            segmented: false,
        }
    }
}
impl ChatOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut options = Self::default();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            if arg == "--segmented" {
                options.segmented = true;
                continue;
            }
            if !matches!(
                arg.to_str(),
                Some("--state-dir" | "--database-config" | "--agent" | "--session" | "--user")
            ) {
                return Err("未知启动参数；使用 --help 查看用法。".into());
            }
            let value = args.next().ok_or("启动参数缺少值。")?;
            if value.is_empty() {
                return Err("启动参数值不能为空。".into());
            }
            match arg.to_str() {
                Some("--state-dir") => options.state_directory = value.into(),
                Some("--database-config") => options.database_config = Some(value.into()),
                Some("--agent") => options.agent_path = value.into(),
                Some("--session") => {
                    options.session_id =
                        value.into_string().map_err(|_| "会话标识必须为 UTF-8。")?
                }
                Some("--user") => {
                    options.user_id = value.into_string().map_err(|_| "用户标识必须为 UTF-8。")?
                }
                _ => unreachable!(),
            }
        }
        SessionKey::new(&options.session_id, &options.user_id)?;
        Ok(Some(options))
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChatSummary {
    pub completed_turns: usize,
    pub failed_turns: usize,
    pub cancelled_turns: usize,
}

/// 独立输入线程和受管轮次并发；成功保存后才向终端输出完整回复。
/// 普通执行失败不重试；保存失败立即停止输入，保留生成内容和 Pending。
/// 回复写入/刷新失败返回 ChatOutputError，保留完整报告和原提交状态。
pub async fn run_console(
    options: ChatOptions,
    input: impl BufRead + Send + 'static,
    mut output: impl Write,
) -> Result<ChatSummary, AppError> {
    let key = SessionKey::new(&options.session_id, &options.user_id)?;
    let bootstrap = core_bootstrap(&options.agent_path)?;
    let backends = KernelServices {
        state: storage::open_state_store(
            &options.state_directory,
            options.database_config.as_deref(),
        )?,
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(backends);
    // 包含注册、启动、装配与对话；任一步失败均走同一停止/日志收尾。
    let result = async {
        // 分段设置先于模型装配加载；损坏或版本不兼容时拒绝启动并保留原字节。
        let preferences: Option<Arc<dyn eve_segment_api::SegmentPreferences>> = if options.segmented
        {
            let plugin = eve_segment_plugin::SegmentPreferencePlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel
                .start(&PluginId::new(
                    eve_segment_plugin::SEGMENT_PREFERENCES_PLUGIN_ID,
                )?)
                .await?;
            Some(Arc::new(controller))
        } else {
            None
        };
        let control = install_core(
            &kernel,
            registry,
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
        )
        .await?;
        let receiver = input::start(input)?;
        let result = console::drive(
            control.clone(),
            key.clone(),
            receiver,
            &mut output,
            tokio::signal::ctrl_c(),
            preferences.map(|preferences| console::Segmenter {
                planner: Arc::new(eve_segment_plugin::ParagraphPlanner::default()),
                policy: CONSOLE_SEGMENT_POLICY,
                preferences,
                // 与 QQ 作用域分开：同一状态目录里的终端会话不会读到 QQ 用户的设置。
                scope: eve_segment_api::SegmentScope {
                    channel: CONSOLE_SEGMENT_CHANNEL.into(),
                    session_id: options.session_id.clone(),
                    user_id: options.user_id.clone(),
                },
            }),
        )
        .await;
        let settled = console::settle(control.as_ref(), &key).await;
        match (result, settled) {
            (result, Ok(None)) => result,
            (Err(primary), Ok(Some(report))) => Err(AppFailure {
                primary,
                secondary: vec![
                    ChatRunError {
                        report: Box::new(report),
                    }
                    .into(),
                ],
            }
            .into()),
            (Err(primary), Err(error)) => Err(AppFailure {
                primary,
                secondary: vec![error],
            }
            .into()),
            (Ok(_), Err(error)) => Err(error),
            (Ok(summary), Ok(Some(_))) => Ok(summary),
        }
    }
    .await;
    finish_core(&kernel, result).await
}

pub(crate) struct CoreBootstrap {
    host_config: LlmHostConfig,
    api_key: String,
    context: Option<Arc<dyn eve_llm_api::ContextAssembler>>,
}

pub(crate) fn core_bootstrap(agent_path: &std::path::Path) -> Result<CoreBootstrap, AppError> {
    let prompt = FileAgentPrompt::new(agent_path)?;
    let host_config = LlmHostConfig::default().with_prompt_source(&prompt)?;
    let api_key =
        std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "宿主缺少 EVE_OPENAI_API_KEY。")?;
    Ok(CoreBootstrap {
        host_config,
        api_key,
        context: None,
    })
}

pub(crate) async fn install_core(
    kernel: &Kernel,
    registry: Arc<dyn eve_plugin_api::ServiceRegistry>,
    permissions: Arc<dyn eve_plugin_api::PermissionChecker>,
    logger: Arc<dyn eve_plugin_api::Logger>,
    state_directory: &std::path::Path,
    bootstrap: CoreBootstrap,
) -> Result<Arc<dyn eve_control_api::ControlService>, AppError> {
    let config_plugin = ConfigPlugin::new(ConfigBootstrap::new(
        state_directory.join("configuration"),
        vec![
            runtime_llm_schema(),
            config::openai_schema(),
            qq_message_judge::provider_schema(),
            model_roles_schema(),
            eve_message_api::message_schema(),
        ],
    ))?;
    install_core_with_config(
        kernel,
        registry,
        permissions,
        logger,
        bootstrap,
        config_plugin,
    )
    .await
}

async fn install_core_with_config(
    kernel: &Kernel,
    registry: Arc<dyn eve_plugin_api::ServiceRegistry>,
    permissions: Arc<dyn eve_plugin_api::PermissionChecker>,
    logger: Arc<dyn eve_plugin_api::Logger>,
    bootstrap: CoreBootstrap,
    config_plugin: ConfigPlugin,
) -> Result<Arc<dyn eve_control_api::ControlService>, AppError> {
    let CoreBootstrap {
        host_config,
        api_key,
        context,
    } = bootstrap;
    kernel.register(Box::new(config_plugin))?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    let mut core_services = services::CoreServices::new()?;
    if let Some(context) = context {
        core_services = core_services.with_context(context);
    }
    kernel.register(Box::new(core_services))?;
    let owner = PluginId::new(services::OWNER)?;
    kernel.start(&owner).await?;
    let entry = registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
        .ok_or("配置服务缺失。")?;
    let settings = entry
        .value
        .downcast::<ConfigServiceHandle>()
        .map_err(|_| "配置服务类型错误。")?;
    let request = settings.0.begin_request(LLM_NAMESPACE, 1)?;
    let runtime = LlmRuntimeConfig::try_from(&settings.0.read_request(&request)?)?;
    if runtime.response_mode != "complete" {
        return Err(
            "核心入口当前使用非流式模式，请将 runtime.llm.response_mode 配为 complete。".into(),
        );
    }
    let resolver = Arc::new(models::CoreModelResolver::new(settings.0.clone(), api_key));
    // 保留启动预检；每轮再捕获最新配置，失败发生在 Session begin 之前。
    let initial = resolver.resolve()?;
    let timeout = initial.provider_timeout;
    let host = LlmHost::new(
        initial.provider,
        registry.clone(),
        kernel.clone(),
        permissions,
        ContextBinding {
            service_id: ServiceId::new(services::CONTEXT)?,
            expected_owner: owner.clone(),
        },
        vec![ToolBinding {
            name: "echo".into(),
            service_id: ServiceId::new(services::TOOL)?,
            expected_owner: owner,
        }],
        LlmHostConfig {
            provider_timeout: timeout,
            max_parallel_tool_calls: runtime.max_parallel_tool_calls,
            response_mode: ResponseMode::Complete,
            ..host_config
        },
    )?
    .with_model_resolver(resolver);
    let host = Arc::new(SessionLlmHost::new(host, SessionBinding::builtin()).with_logger(logger));
    kernel.register(Box::new(ControlPlugin::new(
        Arc::new(SessionControlRunner::new(host)),
        [services::OWNER, SESSION_PLUGIN_ID]
            .into_iter()
            .map(|id| {
                Ok(PluginDependency {
                    id: PluginId::new(id)?,
                    requirement: Some("^0.1".into()),
                })
            })
            .collect::<eve_plugin_api::PluginResult<Vec<_>>>()?,
    )?))?;
    kernel.start(&PluginId::new(CONTROL_PLUGIN_ID)?).await?;
    let entry = registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID)?)?
        .ok_or("控制服务缺失。")?;
    let control = entry
        .value
        .downcast::<ControlServiceHandle>()
        .map_err(|_| "控制服务类型错误。")?
        .0
        .clone();
    Ok(control)
}

pub(crate) async fn finish_core<T>(
    kernel: &Kernel,
    result: Result<T, AppError>,
) -> Result<T, AppError> {
    let stopped = kernel.stop_all().await;
    // 循环与两个控制执行器都可能持有 Kernel；先停止，再卸载以解除引用环。
    let mut secondary: Vec<AppError> = Vec::new();
    if let Err(error) = stopped {
        secondary.push(error.into());
    }
    for name in [
        eve_cognition_loop_api::LOOP_PLUGIN_ID,
        qq_cognition::CONTROL_ID,
        CONTROL_PLUGIN_ID,
    ] {
        let id = PluginId::new(name).expect("有效内置 ID");
        if kernel.state(&id).is_some()
            && let Err(error) = kernel.unregister(&id)
        {
            secondary.push(error.into());
        }
    }
    if let Err(error) = kernel.flush_logs() {
        secondary.push(error.into());
    }
    if !secondary.is_empty() {
        let primary = match result {
            Err(error) => error,
            Ok(_) => secondary.remove(0),
        };
        return Err(AppFailure { primary, secondary }.into());
    }
    result
}
