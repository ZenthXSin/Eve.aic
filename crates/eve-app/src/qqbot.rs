use crate::{AppError, AppFailure, core_bootstrap, finish_core, install_core, qq_cognition};
use eve_cognition_loop_api::EndogenousPlannerFactory;
use eve_cognition_loop_plugin::ReflectionPlannerFactory;
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
use tokio::sync::watch;

pub const QQBOT_HELP: &str = "Eve 官方 QQBot 通道
用法：eve-qqbot [--training] [--cognition] [--cognition-max-executions 1至32] [--state-dir 目录] [--agent 文件] [--node 程序] [--bridge-script 文件] [--bridge-arg 参数]
AppID 默认 1904159860；可通过 QQBOT_APP_ID 覆盖。
必填环境：QQBOT_APP_SECRET、EVE_OPENAI_API_KEY；QQBOT_SANDBOX=true 使用测试环境。
QQ 普通文字排队开始新轮；逐行 /add 内容、/correct 内容、/cancel 控制当前任务。
--training 默认开启主动提问；/train start、/train stop、/train status 按会话启停/查询。
--cognition 开启本地内生反思；/goal 内容保存待办，/goals 查看待办，/mind [目标ID] 查询草稿。
反思默认关闭，每次启动最多执行 32 项；每项一次模型请求、零工具，草稿不代表父目标完成。
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
    pub cognition: bool,
    pub cognition_max_executions: u16,
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
            cognition: false,
            cognition_max_executions: 32,
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
            if arg == "--cognition" {
                options.cognition = true;
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
                Some("--cognition-max-executions") => {
                    options.cognition_max_executions = value
                        .to_str()
                        .and_then(|value| value.parse().ok())
                        .filter(|value| (1..=32).contains(value))
                        .ok_or("认知执行上限必须为 1 至 32 的整数")?;
                }
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
    run_qqbot_with_planner_factory(options, Arc::new(ReflectionPlannerFactory)).await
}

/// 受信宿主的可替换规划入口；仅在显式启用认知时创建规划器。
pub async fn run_qqbot_with_planner_factory(
    options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
) -> Result<QqBotStatus, AppError> {
    if !(1..=32).contains(&options.cognition_max_executions) {
        return Err("认知执行上限必须为 1 至 32 的整数".into());
    }
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
    let kernel = Kernel::with_services(KernelServices {
        events: backends.events.clone(),
        registry: backends.registry.clone(),
        state: backends.state.clone(),
        permissions: backends.permissions.clone(),
        tasks: backends.tasks.clone(),
        logger: backends.logger.clone(),
    });
    let mut background: Option<qq_cognition::Background> = None;
    let mut channel: Option<Arc<QqBotStatusHandle>> = None;
    let result: Result<(), AppError> = async {
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
        let commands = if options.cognition {
            let started = qq_cognition::start(
                &kernel,
                &backends,
                &options.agent_path,
                options.cognition_max_executions,
                factory,
            )
            .await?;
            let commands = started.commands.clone();
            background = Some(started);
            commands
        } else {
            qq_cognition::Commands::disabled()
        };
        kernel.register(Box::new(RelationPlugin::rules()?))?;
        kernel.register(Box::new(MessageRouterPlugin::builtin()?))?;
        kernel.register(Box::new(plugin.with_command_handler(commands)))?;
        kernel.start(&PluginId::new(QQBOT_PLUGIN_ID)?).await?;
        let handle = registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID)?)?
            .ok_or("QQBot 状态服务缺失")?
            .value
            .downcast::<QqBotStatusHandle>()
            .map_err(|_| "QQBot 状态服务类型错误")?;
        channel = Some(handle.clone());
        if let Some(background) = &background {
            background.activate();
        }
        wait_channel(
            handle.status.clone(),
            background.as_ref().map(qq_cognition::Background::finished),
        )
        .await
    }
    .await;
    // 先关闭通道准入并结束反思执行，再等待桥接收尾，最后进入 Kernel 生命周期写准入。
    // 启动失败、后台异常、EOF 和系统信号全部经过这里。
    let mut secondary = Vec::<AppError>::new();
    if channel.is_none() {
        match registry.get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID).expect("有效 QQ 状态 ID")) {
            Ok(Some(entry)) => match entry.value.downcast::<QqBotStatusHandle>() {
                Ok(handle) => channel = Some(handle),
                Err(_) => secondary.push("QQBot 收尾状态服务类型错误".into()),
            },
            Ok(None) => {}
            Err(error) => secondary.push(error.into()),
        }
    }
    if let Some(handle) = &channel {
        handle.request_stop();
    }
    if let Some(background) = background
        && let Err(error) = background.stop().await
    {
        secondary.push(error);
    }
    let summary = if let Some(handle) = channel {
        let mut status = handle.status.clone();
        while !status.borrow().closed {
            if status.changed().await.is_err() {
                secondary.push("QQBot 收尾通知丢失".into());
                break;
            }
        }
        let summary = *status.borrow();
        if summary.terminal_error {
            secondary.push("QQBot 通道异常结束；状态已保留".into());
        }
        summary
    } else {
        QqBotStatus::default()
    };
    let result = match (result, secondary.is_empty()) {
        (Ok(()), true) => Ok(summary),
        (Ok(()), false) => {
            let primary = secondary.remove(0);
            Err(Box::new(AppFailure { primary, secondary }) as AppError)
        }
        (Err(primary), _) => Err(Box::new(AppFailure { primary, secondary }) as AppError),
    };
    finish_core(&kernel, result).await
}

async fn background_finished(mut receiver: Option<watch::Receiver<bool>>) {
    let Some(receiver) = &mut receiver else {
        std::future::pending::<()>().await;
        return;
    };
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_channel(
    mut status: watch::Receiver<QqBotStatus>,
    background: Option<watch::Receiver<bool>>,
) -> Result<(), AppError> {
    let stop = interrupted();
    let stopped_background = background_finished(background);
    tokio::pin!(stop, stopped_background);
    let mut ready_announced = false;
    loop {
        if status.borrow().ready && !status.borrow().closed && !ready_announced {
            eprintln!("EVE_QQBOT_READY");
            ready_announced = true;
        }
        if status.borrow().closed {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = &mut stopped_background => return Err("认知后台已结束；QQ 通道停止准入并保留状态".into()),
            result = &mut stop => return result,
            result = status.changed() => result.map_err(|_| "QQBot 状态通知丢失")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<QqBotOptions, AppError> {
        Ok(QqBotOptions::parse(args.iter().map(OsString::from))?.unwrap())
    }

    #[test]
    fn cognition_is_explicit_and_has_bounded_independent_budget() {
        let options = parse(&[]).unwrap();
        assert!(!options.cognition);
        assert!(!options.training);
        assert_eq!(options.cognition_max_executions, 32);
        let options = parse(&[
            "--training",
            "--cognition",
            "--cognition-max-executions",
            "1",
        ])
        .unwrap();
        assert!(options.cognition && options.training);
        assert_eq!(options.cognition_max_executions, 1);
        let options = parse(&["--cognition-max-executions", "32"]).unwrap();
        assert!(!options.cognition);
        assert_eq!(options.cognition_max_executions, 32);
    }

    #[test]
    fn invalid_cognition_budget_is_rejected_before_startup() {
        for value in ["0", "33", "-1", "65536", "1.5", "one", ""] {
            assert!(parse(&["--cognition-max-executions", value]).is_err());
        }
        assert!(parse(&["--cognition-max-executions"]).is_err());
        assert!(
            QqBotOptions::parse([OsString::from("--help")])
                .unwrap()
                .is_none()
        );
    }
}
