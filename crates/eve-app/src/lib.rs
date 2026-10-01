//! Eve 的最小可运行组合入口：固定身份、一个主模型、会话历史与插件工具。
//! 输入与受管轮次并发，任务仍串行；不包含额外模型或 Web 控制面。
mod config;
mod console;
mod error;
mod input;
mod services;

pub use console::ChatRunError;
pub use error::AppFailure;

use eve_agent_prompt::FileAgentPrompt;
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigServiceHandle, LLM_NAMESPACE, LlmRuntimeConfig, runtime_llm_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CONTROL_PLUGIN_ID, CONTROL_SERVICE_ID, ControlServiceHandle};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ResponseMode, ToolBinding};
use eve_llm_openai::OpenAiProvider;
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
用法：eve [--state-dir 目录] [--agent AGENT.md] [--session 会话] [--user 用户]
主模型：EVE_OPENAI_MODEL；凭据：EVE_OPENAI_API_KEY（仅宿主环境）
可选：EVE_OPENAI_BASE_URL、EVE_OPENAI_REASONING_EFFORT
一行一轮；/cancel 取消当前轮；/quit 或 Ctrl+C 取消并退出；/help 查看说明。
EOF 处理完已接收输入后退出；最多 16 条待处理输入，取消/退出会清空队列。
输入上限 32768 字节；当前入口使用非流式模式，串行执行和保存。";
const MAX_INPUT_BYTES: usize = 32768;

#[derive(Clone, Debug)]
pub struct ChatOptions {
    pub state_directory: PathBuf,
    pub agent_path: PathBuf,
    pub session_id: String,
    pub user_id: String,
}
impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            state_directory: ".eve".into(),
            agent_path: "AGENT.md".into(),
            session_id: "default".into(),
            user_id: "owner".into(),
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
            if !matches!(
                arg.to_str(),
                Some("--state-dir" | "--agent" | "--session" | "--user")
            ) {
                return Err("未知启动参数；使用 --help 查看用法。".into());
            }
            let value = args.next().ok_or("启动参数缺少值。")?;
            if value.is_empty() {
                return Err("启动参数值不能为空。".into());
            }
            match arg.to_str() {
                Some("--state-dir") => options.state_directory = value.into(),
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
pub async fn run_console(
    options: ChatOptions,
    input: impl BufRead + Send + 'static,
    mut output: impl Write,
) -> Result<ChatSummary, AppError> {
    let key = SessionKey::new(&options.session_id, &options.user_id)?;
    let prompt = FileAgentPrompt::new(&options.agent_path)?;
    let host_config = LlmHostConfig::default().with_prompt_source(&prompt)?;
    let api_key =
        std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "宿主缺少 EVE_OPENAI_API_KEY。")?;
    let backends = KernelServices {
        state: Arc::new(FileStateStore::open(&options.state_directory)?),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(backends);
    // 包含注册、启动、装配与对话；任一步失败均走同一停止/日志收尾。
    let result = async {
        kernel.register(Box::new(ConfigPlugin::new(ConfigBootstrap::new(
            options.state_directory.join("configuration"),
            vec![runtime_llm_schema(), config::openai_schema()],
        ))?))?;
        kernel.register(Box::new(SessionPlugin::new()?))?;
        kernel.register(Box::new(services::CoreServices::new()?))?;
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
        let request = settings.0.begin_request(config::OPENAI_NAMESPACE, 1)?;
        let provider_config = config::provider_config(&settings.0.read_request(&request)?)?;
        let timeout = provider_config.request_timeout;
        let provider = OpenAiProvider::new(provider_config, &api_key)?;
        let host = LlmHost::new(
            Arc::new(provider),
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
        )?;
        let host = Arc::new(SessionLlmHost::new(host, SessionBinding::builtin()).with_logger(logger));
        kernel.register(Box::new(ControlPlugin::new(
            Arc::new(SessionControlRunner::new(host)),
            [services::OWNER, SESSION_PLUGIN_ID].into_iter().map(|id| {
                Ok(PluginDependency { id: PluginId::new(id)?, requirement: Some("^0.1".into()) })
            }).collect::<eve_plugin_api::PluginResult<Vec<_>>>()?,
        )?))?;
        kernel.start(&PluginId::new(CONTROL_PLUGIN_ID)?).await?;
        let entry = registry.get(&ServiceId::new(CONTROL_SERVICE_ID)?)?.ok_or("控制服务缺失。")?;
        let control = entry.value.downcast::<ControlServiceHandle>().map_err(|_| "控制服务类型错误。")?.0.clone();
        let receiver = input::start(input)?;
        let result = console::drive(control.clone(), key.clone(), receiver, &mut output, tokio::signal::ctrl_c()).await;
        let settled = console::settle(control.as_ref(), &key).await;
        match (result, settled) {
            (result, Ok(None)) => result,
            (Err(primary), Ok(Some(report))) => Err(AppFailure {
                primary, secondary: vec![ChatRunError { report: Box::new(report) }.into()],
            }.into()),
            (Err(primary), Err(error)) => Err(AppFailure { primary, secondary: vec![error] }.into()),
            (Ok(_), Err(error)) => Err(error),
            (Ok(summary), Ok(Some(_))) => Ok(summary),
        }
    }
    .await;
    let stopped = kernel.stop_all().await;
    // 控制插件持有的执行器含 Kernel；停止后卸载以打破组合层引用环。
    let control_id = PluginId::new(CONTROL_PLUGIN_ID).expect("有效内置 ID");
    let removed = if kernel.state(&control_id).is_some() {
        kernel.unregister(&control_id)
    } else { Ok(()) };
    let flushed = kernel.flush_logs();
    // 三个结果都已执行；同时保留原始输出和各个收尾错误。
    let mut secondary: Vec<AppError> = Vec::new();
    if let Err(error) = stopped {
        secondary.push(error.into());
    }
    if let Err(error) = removed {
        secondary.push(error.into());
    }
    if let Err(error) = flushed {
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
