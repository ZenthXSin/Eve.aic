use crate::{AppError, core_bootstrap, finish_core, install_core};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginId, ServiceId};
use eve_qqbot_plugin::{
    DEFAULT_QQBOT_APP_ID, QQBOT_PLUGIN_ID, QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin,
    QqBotStatus, QqBotStatusHandle,
};
use eve_training_api::{TRAINING_PLUGIN_ID, TRAINING_SERVICE_ID, TrainingServiceHandle};
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::{ffi::OsString, path::PathBuf, sync::Arc};

pub const QQBOT_HELP: &str = "Eve 官方 QQBot 通道
用法：eve-qqbot [--training] [--state-dir 目录] [--agent 文件] [--node 程序] [--bridge-script 文件] [--bridge-arg 参数]
AppID 默认 1904159860；可通过 QQBOT_APP_ID 覆盖。
必填环境：QQBOT_APP_SECRET、EVE_OPENAI_API_KEY；QQBOT_SANDBOX=true 使用测试环境。
QQ 普通文字排队开始新轮；逐行 /add 内容、/correct 内容、/cancel 控制当前任务。
--training 默认开启主动提问；/train start、/train stop、/train status 按会话启停/查询。
修订先取消并等待；已有工具操作时只澄清，/new 内容明确开始独立任务。
Ctrl+C 或 SIGTERM 取消在途轮次、等待保存并停止桥接子进程。";
/// 密钥只在创建插件时从环境读取，不包含在启动参数和 Debug 中。
#[derive(Clone, Debug)]
pub struct QqBotOptions {
    pub state_directory: PathBuf,
    pub agent_path: PathBuf,
    pub node_program: OsString,
    pub bridge_script: PathBuf,
    pub bridge_args: Vec<OsString>,
    pub training: bool,
}
impl Default for QqBotOptions {
    fn default() -> Self {
        Self {
            state_directory: ".eve".into(),
            agent_path: "AGENT.md".into(),
            node_program: "node".into(),
            bridge_script: "connectors/qqbot/bridge.mjs".into(),
            bridge_args: Vec::new(),
            training: false,
        }
    }
}
impl QqBotOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut options = Self::default();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            if arg == "--training" {
                options.training = true;
                continue;
            }
            let value = args.next().ok_or("QQBot 参数缺少值")?;
            if value.is_empty() {
                return Err("QQBot 参数值不能为空".into());
            }
            match arg.to_str() {
                Some("--state-dir") => options.state_directory = value.into(),
                Some("--agent") => options.agent_path = value.into(),
                Some("--node") => options.node_program = value,
                Some("--bridge-script") => options.bridge_script = value.into(),
                Some("--bridge-arg") => options.bridge_args.push(value),
                _ => return Err("未知 QQBot 参数；使用 --help".into()),
            }
        }
        Ok(Some(options))
    }
}
async fn interrupted() -> Result<(), AppError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
pub async fn run_qqbot(options: QqBotOptions) -> Result<QqBotStatus, AppError> {
    let app_secret = std::env::var("QQBOT_APP_SECRET").map_err(|_| "缺少 QQBOT_APP_SECRET")?;
    let sandbox = match std::env::var("QQBOT_SANDBOX").as_deref() {
        Ok("true") => true,
        Ok("" | "false") | Err(_) => false,
        _ => return Err("QQBOT_SANDBOX 必须为 true 或 false".into()),
    };
    let mut bootstrap = core_bootstrap(&options.agent_path)?;
    let plugin = QqBotPlugin::new(QqBotConfig {
        node_program: options.node_program,
        bridge_script: options.bridge_script,
        bridge_args: options.bridge_args,
        app_id: std::env::var("QQBOT_APP_ID").unwrap_or_else(|_| DEFAULT_QQBOT_APP_ID.into()),
        app_secret,
        sandbox,
    })?
    .with_training()?;
    let backends = KernelServices {
        state: Arc::new(FileStateStore::open(&options.state_directory)?),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(backends);
    let result = async {
        kernel.register(Box::new(TrainingPlugin::new(options.training)?))?;
        kernel.start(&PluginId::new(TRAINING_PLUGIN_ID)?).await?;
        let training = registry
            .get(&ServiceId::new(TRAINING_SERVICE_ID)?)?
            .ok_or("训练服务缺失")?
            .value
            .downcast::<TrainingServiceHandle>()
            .map_err(|_| "训练服务类型错误")?;
        bootstrap.context = Some(Arc::new(TrainingContext(training.0.clone())));
        install_core(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
        )
        .await?;
        kernel.register(Box::new(RelationPlugin::rules()?))?;
        kernel.register(Box::new(MessageRouterPlugin::builtin()?))?;
        kernel.register(Box::new(plugin))?;
        kernel.start(&PluginId::new(QQBOT_PLUGIN_ID)?).await?;
        let handle = registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID)?)?
            .ok_or("QQBot 状态服务缺失")?
            .value
            .downcast::<QqBotStatusHandle>()
            .map_err(|_| "QQBot 状态服务类型错误")?;
        let mut status = handle.status.clone();
        // stdout 仍只保留最终 JSON 摘要，stderr 不含正文和凭据。
        while !status.borrow().ready && !status.borrow().closed {
            tokio::select! {
                result = interrupted() => {
                    result?;
                    handle.request_stop();
                    break;
                },
                result = status.changed() => { result.map_err(|_| "QQBot 就绪通知丢失")?; },
            }
        }
        if status.borrow().ready && !status.borrow().closed {
            eprintln!("EVE_QQBOT_READY");
        }
        tokio::select! {
            result = interrupted() => {
                result?;
                handle.request_stop();
                while !status.borrow().closed {
                    status.changed().await.map_err(|_| "QQBot 收尾通知丢失")?;
                }
            },
            _ = async {
                while !status.borrow().closed {
                    if status.changed().await.is_err() { break; }
                }
            } => {},
        }
        kernel.stop(&PluginId::new(QQBOT_PLUGIN_ID)?).await?;
        let summary = *status.borrow();
        if summary.terminal_error {
            return Err("QQBot 通道异常结束；状态已保留".into());
        }
        Ok(summary)
    }
    .await;
    finish_core(&kernel, result).await
}
