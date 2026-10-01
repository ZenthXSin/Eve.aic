//! Eve 的最小可运行组合入口：固定身份、一个主模型、会话历史与插件工具。
//! 串行读取输入，不包含辅助模型、语义检索、动态驱动或 Web 控制面。
mod config;
mod error;
mod services;

pub use error::AppFailure;

use eve_agent_prompt::FileAgentPrompt;
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigServiceHandle, LLM_NAMESPACE, LlmRuntimeConfig, runtime_llm_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ResponseMode, ToolBinding};
use eve_llm_openai::OpenAiProvider;
use eve_plugin_api::{PluginId, ServiceId};
use eve_runtime::{
    ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost, SessionRunError,
};
use eve_session_api::{SessionInput, SessionKey};
use eve_session_plugin::SessionPlugin;
use std::{
    ffi::OsString,
    io::{BufRead, Read, Write},
    path::PathBuf,
    sync::Arc,
};

pub type AppError = Box<dyn std::error::Error + Send + Sync>;
pub const HELP: &str = "Eve 核心对话入口
用法：eve [--state-dir 目录] [--agent AGENT.md] [--session 会话] [--user 用户]
主模型：EVE_OPENAI_MODEL；凭据：EVE_OPENAI_API_KEY（仅宿主环境）
可选：EVE_OPENAI_BASE_URL、EVE_OPENAI_REASONING_EFFORT
一行一轮；空行忽略；/help 查看说明；/quit 或输入结束后停止并刷新。
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
}

/// 输入等待与轮次执行串行；成功保存后才向终端输出完整回复。
/// 普通执行失败不重试；保存失败立即停止输入，保留生成内容和 Pending。
pub async fn run_console(
    options: ChatOptions,
    mut input: impl BufRead,
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
        let host = SessionLlmHost::new(host, SessionBinding::builtin()).with_logger(logger);
        let mut summary = ChatSummary::default();
        loop {
            let mut bytes = Vec::new();
            let count = (&mut input)
                .take((MAX_INPUT_BYTES + 3) as u64)
                .read_until(b'\n', &mut bytes)?;
            if count == 0 {
                break;
            }
            let newline = bytes.last() == Some(&b'\n');
            if newline {
                bytes.pop();
            }
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            if bytes.len() > MAX_INPUT_BYTES {
                if !newline {
                    input.skip_until(b'\n')?;
                }
                writeln!(output, "Eve：输入超过 32768 字节，请缩短后重新提交。")?;
                output.flush()?;
                continue;
            }
            let text = String::from_utf8(bytes).map_err(|_| "输入必须为 UTF-8。")?;
            if text.trim().is_empty() {
                continue;
            }
            match text.trim() {
                "/quit" => break,
                "/help" => {
                    writeln!(output, "{HELP}")?;
                    output.flush()?;
                    continue;
                }
                _ => {}
            }
            match host
                .run_turn(SessionInput {
                    key: key.clone(),
                    text,
                })
                .await
            {
                Ok(turn) => {
                    summary.completed_turns += 1;
                    writeln!(output, "Eve：{}", turn.output.text)?;
                }
                Err(SessionRunError::Turn(failure)) => {
                    summary.failed_turns += 1;
                    writeln!(output, "Eve：本轮执行失败：{}。未自动重试。", failure.error)?;
                }
                Err(SessionRunError::Commit {
                    error,
                    output: generated,
                }) => {
                    let displayed = writeln!(output, "Eve：回复已生成但未保存：{}", generated.text)
                        .and_then(|()| output.flush());
                    let primary: AppError = SessionRunError::Commit {
                        error,
                        output: generated,
                    }
                    .into();
                    return Err(match displayed {
                        Ok(()) => primary,
                        Err(error) => AppFailure {
                            primary,
                            secondary: vec![error.into()],
                        }
                        .into(),
                    });
                }
                Err(error) => return Err(error.into()),
            }
            output.flush()?;
        }
        Ok::<_, AppError>(summary)
    }
    .await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    // 三个结果都已执行；同时保留原始输出和各个收尾错误。
    let mut secondary: Vec<AppError> = Vec::new();
    if let Err(error) = stopped {
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
