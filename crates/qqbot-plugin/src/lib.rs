//! 官方 QQBot 通道实现；只通过公开 Control/Message/Session 与插件 Context 协作。
mod bridge;
mod commands;
mod observer;
mod state;

pub use commands::{QqCommandHandler, QqCommandInput};
pub use observer::{QqInteraction, QqInteractionObserver, QqInteractionSource};

use eve_control_api::{CONTROL_PLUGIN_ID, CONTROL_SERVICE_ID, ControlServiceHandle};
use eve_message_api::{MessageServiceHandle, ROUTER_PLUGIN_ID, ROUTER_SERVICE_ID};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginManifest,
    PluginResult, ServiceId, TaskMode, TaskSchedule, TaskSpec,
};
use eve_segment_api::{
    SegmentLimits, SegmentPlanner, SegmentPolicy, SegmentPreferences, SegmentScope,
};
use eve_session_api::{SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionKey, SessionServiceHandle};
use eve_training_api::{TRAINING_PLUGIN_ID, TRAINING_SERVICE_ID, TrainingServiceHandle};
use serde::Serialize;
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub const QQBOT_PLUGIN_ID: &str = "eve.channel.qqbot";
pub const QQBOT_STATUS_SERVICE_ID: &str = "eve.channel.qqbot.status";
pub const DEFAULT_QQBOT_APP_ID: &str = "1904159860";
/// 平台对同一条消息的被动回复次数有限；分段数不能超过该值。
pub const QQ_MAX_SEGMENTS: usize = 5;
/// QQ 默认分段预算：最多三段，单段与整条回复同上限，段前停顿最长 2.5 秒。
pub const QQ_SEGMENT_LIMITS: SegmentLimits = SegmentLimits {
    max_segments: 3,
    max_segment_bytes: 32768,
    max_pause_ms: 2500,
};
/// 会话设置可选范围：段数至多到平台上限，停顿最长 5 秒；未设置时仍用默认预算。
pub const QQ_SEGMENT_POLICY: SegmentPolicy = SegmentPolicy {
    defaults: QQ_SEGMENT_LIMITS,
    max_segments: QQ_MAX_SEGMENTS,
    max_pause_ms: 5000,
};
pub const QQ_SEGMENT_CHANNEL: &str = "qq";
/// 投递与命令处理共用同一作用域推导；会话已按 AppID、私聊/群、目标与发送者绑定。
pub fn segment_scope(session: &SessionKey) -> SegmentScope {
    SegmentScope {
        channel: QQ_SEGMENT_CHANNEL.into(),
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
    }
}

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
pub struct QqBotStatusHandle {
    pub status: watch::Receiver<QqBotStatus>,
    stop: watch::Sender<bool>,
}
impl QqBotStatusHandle {
    /// 先请求并等待 closed，再调用 Kernel stop，避免与在途模型的生命周期准入互相等待。
    pub fn request_stop(&self) {
        self.stop.send_replace(true);
    }
}

pub struct QqBotPlugin {
    manifest: PluginManifest,
    config: Arc<QqBotConfig>,
    command_handler: Option<Arc<dyn QqCommandHandler>>,
    training: bool,
    observer: Option<Arc<dyn QqInteractionObserver>>,
    segmentation: Option<Arc<Segmentation>>,
}
pub(crate) struct Segmentation {
    pub planner: Arc<dyn SegmentPlanner>,
    pub policy: SegmentPolicy,
    pub preferences: Option<Arc<dyn SegmentPreferences>>,
}
impl QqBotPlugin {
    pub fn new(config: QqBotConfig) -> PluginResult<Self> {
        if !state::valid_id(&config.app_id) || config.app_secret.trim().is_empty() {
            return Err(PluginError::State("QQBot 凭据缺失或 AppID 无效".into()));
        }
        let mut manifest = PluginManifest::new(QQBOT_PLUGIN_ID, "0.1.0")?;
        for id in [CONTROL_PLUGIN_ID, ROUTER_PLUGIN_ID] {
            manifest.dependencies.push(PluginDependency {
                id: eve_plugin_api::PluginId::new(id)?,
                requirement: Some("^0.1".into()),
            });
        }
        Ok(Self {
            manifest,
            config: Arc::new(config),
            command_handler: None,
            training: false,
            observer: None,
            segmentation: None,
        })
    }
    /// 把已完成的模型回复按计划分成少量消息投递；命令确认仍整条发送。
    /// 未接线时与原单条回复完全一致。
    pub fn with_segmenter(
        mut self,
        planner: Arc<dyn SegmentPlanner>,
        limits: SegmentLimits,
    ) -> PluginResult<Self> {
        if limits.validate().is_err()
            || limits.max_segments > QQ_MAX_SEGMENTS
            || limits.max_segment_bytes > 32768
        {
            return Err(PluginError::State("QQBot 分段预算无效".into()));
        }
        self.segmentation = Some(Arc::new(Segmentation {
            planner,
            policy: SegmentPolicy::fixed(limits),
            preferences: None,
        }));
        Ok(self)
    }
    /// 回复开始投递时按可信会话读取用户分段设置；必须先 `with_segmenter`，
    /// 且策略默认值与其预算一致、上限不超过平台限制。读取失败时整条发送。
    pub fn with_segment_preferences(
        mut self,
        preferences: Arc<dyn SegmentPreferences>,
        policy: SegmentPolicy,
    ) -> PluginResult<Self> {
        let Some(current) = self.segmentation.as_ref() else {
            return Err(PluginError::State("QQBot 分段设置需要先开启分段".into()));
        };
        if policy.validate().is_err()
            || policy.defaults != current.policy.defaults
            || policy.max_segments > QQ_MAX_SEGMENTS
        {
            return Err(PluginError::State("QQBot 分段策略无效".into()));
        }
        self.segmentation = Some(Arc::new(Segmentation {
            planner: current.planner.clone(),
            policy,
            preferences: Some(preferences),
        }));
        Ok(self)
    }

    /// 添加宿主命令扩展；默认不安装，原有消息控制行为不变。
    pub fn with_command_handler(mut self, handler: Arc<dyn QqCommandHandler>) -> Self {
        self.command_handler = Some(handler);
        self
    }
    /// 通过公开契约接线；未接线的通道继续使用既有消息规则。
    pub fn with_training(mut self) -> PluginResult<Self> {
        self.manifest.dependencies.push(PluginDependency {
            id: eve_plugin_api::PluginId::new(TRAINING_PLUGIN_ID)?,
            requirement: Some("^0.1".into()),
        });
        self.training = true;
        Ok(self)
    }
    /// 接收已提交且确认发送的普通交互；未接线时不读取 Session 或执行观察。
    pub fn with_interaction_observer(
        mut self,
        observer: Arc<dyn QqInteractionObserver>,
    ) -> PluginResult<Self> {
        if self.observer.is_none() {
            self.manifest.dependencies.push(PluginDependency {
                id: eve_plugin_api::PluginId::new(SESSION_PLUGIN_ID)?,
                requirement: Some("^0.1".into()),
            });
        }
        self.observer = Some(observer);
        Ok(self)
    }
}
impl Plugin for QqBotPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let ledger = state::Ledger::load(&ctx)?;
            let observation = if let Some(observer) = &self.observer {
                Some(Arc::new(observer::Observation {
                    observer: observer.clone(),
                    sessions: ctx
                        .service::<SessionServiceHandle>(&ServiceId::new(SESSION_SERVICE_ID)?)?
                        .ok_or_else(|| PluginError::State("QQBot 交互观察所需会话服务缺失".into()))?
                        .0
                        .clone(),
                }))
            } else {
                None
            };
            let training = if self.training {
                Some(
                    ctx.service::<TrainingServiceHandle>(&ServiceId::new(TRAINING_SERVICE_ID)?)?
                        .ok_or_else(|| PluginError::State("QQBot 训练服务缺失".into()))?
                        .0
                        .clone(),
                )
            } else {
                None
            };
            let control = ctx
                .service::<ControlServiceHandle>(&ServiceId::new(CONTROL_SERVICE_ID)?)?
                .ok_or_else(|| PluginError::State("QQBot 控制服务缺失".into()))?
                .0
                .clone();
            let messages = ctx
                .service::<MessageServiceHandle>(&ServiceId::new(ROUTER_SERVICE_ID)?)?
                .ok_or_else(|| PluginError::State("QQBot 消息服务缺失".into()))?
                .0
                .clone();
            let (status, receiver) = watch::channel(QqBotStatus::default());
            let (stop, stop_receiver) = watch::channel(false);
            ctx.provide_service(
                ServiceId::new(QQBOT_STATUS_SERVICE_ID)?,
                QqBotStatusHandle {
                    status: receiver,
                    stop,
                },
            )?;
            let ledger = Arc::new(tokio::sync::Mutex::new(Some(ledger)));
            let config = self.config.clone();
            let command_handler = self.command_handler.clone();
            let segmentation = self.segmentation.clone();
            let task_ctx = ctx.clone();
            ctx.spawn_task(TaskSpec::new(
                "QQBot JSONL 通道",
                TaskMode::Background,
                TaskSchedule::Immediate,
                Arc::new(move |signal| {
                    let config = config.clone();
                    let stop_receiver = stop_receiver.clone();
                    let ctx = task_ctx.clone();
                    let control = control.clone();
                    let messages = messages.clone();
                    let command_handler = command_handler.clone();
                    let training = training.clone();
                    let observation = observation.clone();
                    let segmentation = segmentation.clone();
                    let status = status.clone();
                    let ledger = ledger.clone();
                    Box::pin(async move {
                        let ledger =
                            ledger.lock().await.take().ok_or_else(|| {
                                PluginError::Task("QQBot 任务不得重复启动".into())
                            })?;
                        let result = bridge::run(
                            config,
                            ctx,
                            bridge::Services {
                                control,
                                messages,
                                command_handler,
                                training,
                                observation,
                                segmentation,
                            },
                            ledger,
                            signal,
                            status.clone(),
                            stop_receiver,
                        )
                        .await;
                        status.send_modify(|s| {
                            s.closed = true;
                            s.terminal_error = result.is_err();
                        });
                        result
                    })
                }),
            )?)?;
            Ok(None)
        })
    }
}
