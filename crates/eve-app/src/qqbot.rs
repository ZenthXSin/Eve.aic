use crate::{AppError, core_bootstrap, finish_core, install_core};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginId, ServiceId};
use eve_qqbot_plugin::{
    DEFAULT_QQBOT_APP_ID, QQBOT_PLUGIN_ID, QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin,
    QqBotStatus, QqBotStatusHandle,
};
use std::{ffi::OsString, path::PathBuf, sync::Arc};

pub const QQBOT_HELP: &str = "Eve 官方 QQBot 通道
用法：eve-qqbot [--state-dir 目录] [--agent 文件] [--node 程序] [--bridge-script 文件] [--bridge-arg 参数]
AppID 默认 1904159860；可通过 QQBOT_APP_ID 覆盖。
必填环境：QQBOT_APP_SECRET、EVE_OPENAI_API_KEY；QQBOT_SANDBOX=true 使用测试环境。
Ctrl+C 或 SIGTERM 取消在途轮次、等待保存并停止桥接子进程。";
/// 密钥只在创建插件时从环境读取，不包含在启动参数和 Debug 中。
#[derive(Clone, Debug)]
pub struct QqBotOptions {
    pub state_directory: PathBuf,
    pub agent_path: PathBuf,
    pub node_program: OsString,
    pub bridge_script: PathBuf,
    pub bridge_args: Vec<OsString>,
}
impl Default for QqBotOptions {
    fn default() -> Self {
        Self {
            state_directory: ".eve".into(),
            agent_path: "AGENT.md".into(),
            node_program: "node".into(),
            bridge_script: "connectors/qqbot/bridge.mjs".into(),
            bridge_args: Vec::new(),
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
    let bootstrap = core_bootstrap(&options.agent_path)?;
    let plugin = QqBotPlugin::new(QqBotConfig {
        node_program: options.node_program,
        bridge_script: options.bridge_script,
        bridge_args: options.bridge_args,
        app_id: std::env::var("QQBOT_APP_ID").unwrap_or_else(|_| DEFAULT_QQBOT_APP_ID.into()),
        app_secret,
        sandbox,
    })?;
    let backends = KernelServices {
        state: Arc::new(FileStateStore::open(&options.state_directory)?),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(backends);
    let result = async {
        install_core(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
        )
        .await?;
        kernel.register(Box::new(plugin))?;
        kernel.start(&PluginId::new(QQBOT_PLUGIN_ID)?).await?;
        let handle = registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID)?)?
            .ok_or("QQBot 状态服务缺失")?
            .value
            .downcast::<QqBotStatusHandle>()
            .map_err(|_| "QQBot 状态服务类型错误")?;
        let mut status = handle.status.clone();
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
