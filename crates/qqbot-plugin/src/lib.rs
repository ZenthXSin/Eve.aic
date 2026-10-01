//! 官方 QQBot 通道实现；只通过公开 Control/Session 与插件 Context 协作。
mod bridge;
mod state;

use eve_control_api::{CONTROL_PLUGIN_ID, CONTROL_SERVICE_ID, ControlServiceHandle};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginManifest,
    PluginResult, ServiceId, TaskMode, TaskSchedule, TaskSpec,
};
use serde::Serialize;
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub const QQBOT_PLUGIN_ID: &str = "eve.channel.qqbot";
pub const QQBOT_STATUS_SERVICE_ID: &str = "eve.channel.qqbot.status";
pub const DEFAULT_QQBOT_APP_ID: &str = "1904159860";

/// 不实现 Debug，避免将密钥带入宿主诊断。
pub struct QqBotConfig {
    pub node_program: OsString,
    pub bridge_script: PathBuf,
    pub bridge_args: Vec<OsString>,
    pub app_id: String,
    pub app_secret: String,
    pub sandbox: bool,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct QqBotStatus {
    pub ready: bool,
    pub closed: bool,
    pub terminal_error: bool,
    pub received: u64,
    pub completed: u64,
    pub sent: u64,
    pub failed: u64,
}
#[derive(Clone)]
pub struct QqBotStatusHandle(pub watch::Receiver<QqBotStatus>);

pub struct QqBotPlugin {
    manifest: PluginManifest,
    config: Arc<QqBotConfig>,
}
impl QqBotPlugin {
    pub fn new(config: QqBotConfig) -> PluginResult<Self> {
        if !state::valid_id(&config.app_id) || config.app_secret.trim().is_empty() {
            return Err(PluginError::State("QQBot 凭据缺失或 AppID 无效".into()));
        }
        let mut manifest = PluginManifest::new(QQBOT_PLUGIN_ID, "0.1.0")?;
        manifest.dependencies.push(PluginDependency {
            id: eve_plugin_api::PluginId::new(CONTROL_PLUGIN_ID)?,
            requirement: Some("^0.1".into()),
        });
        Ok(Self { manifest, config: Arc::new(config) })
    }
}
impl Plugin for QqBotPlugin {
    fn manifest(&self) -> &PluginManifest { &self.manifest }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let ledger = state::Ledger::load(&ctx)?;
            let control = ctx.service::<ControlServiceHandle>(&ServiceId::new(CONTROL_SERVICE_ID)?)?
                .ok_or_else(|| PluginError::State("QQBot 控制服务缺失".into()))?.0.clone();
            let (status, receiver) = watch::channel(QqBotStatus::default());
            ctx.provide_service(ServiceId::new(QQBOT_STATUS_SERVICE_ID)?, QqBotStatusHandle(receiver))?;
            let ledger = Arc::new(tokio::sync::Mutex::new(Some(ledger)));
            let config = self.config.clone();
            let task_ctx = ctx.clone();
            ctx.spawn_task(TaskSpec::new(
                "QQBot JSONL 通道", TaskMode::Background, TaskSchedule::Immediate,
                Arc::new(move |signal| {
                    let config = config.clone();
                    let ctx = task_ctx.clone();
                    let control = control.clone();
                    let status = status.clone();
                    let ledger = ledger.clone();
                    Box::pin(async move {
                        let ledger = ledger.lock().await.take()
                            .ok_or_else(|| PluginError::Task("QQBot 任务不得重复启动".into()))?;
                        let result = bridge::run(config, ctx, control, ledger, signal, status.clone()).await;
                        status.send_modify(|s| { s.closed = true; s.terminal_error = result.is_err(); });
                        result
                    })
                }),
            )?)?;
            Ok(None)
        })
    }
}
