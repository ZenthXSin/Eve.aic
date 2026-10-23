//! 官方 QQBot 通道实现；只通过公开 Control/Message/Session 与插件 Context 协作。
mod archive;
mod bridge;
mod commands;
mod conversations;
mod files;
mod observer;
mod outreach;
mod participants;
mod state;

pub use archive::{QqArchivedReceipt, QqReceiptArchive, read_archived_receipt};
pub use commands::{QqCommandHandler, QqCommandInput};
pub use conversations::{QqConversationIdentity, QqConversations, QqWakeupLease};
pub use files::{
    QQ_FILE_CAPTION_MAX_BYTES, QQ_FILE_NAME_MAX_BYTES, QQ_FILE_PATH_MAX_BYTES, QqFileDelivery,
    QqFileMessage, QqFileResult, QqRawMessage, QqRawMessageObserver, valid_caption, valid_name,
    valid_path,
};
pub use observer::{QqInteraction, QqInteractionObserver, QqInteractionSource};
pub use outreach::{
    QQ_OUTREACH_JUDGE_TIMEOUT, QQ_OUTREACH_MAX_BYTES, QQ_OUTREACH_MAX_REPLY_PARTS,
    QQ_OUTREACH_PAUSE_MS, QqGroupChimeDelivery, QqOutreach, QqOutreachFuture, QqOutreachMessage,
    QqOutreachMoment, QqOutreachPush, QqOutreachResult,
};
pub use participants::QqParticipantObserver;
pub use state::{ImageAttachment, Message};

/// 慢步骤进度的时间阈值：本轮某一步（等待模型或一批工具）持续超过它仍未结束时，
/// 通道补发一条说明当前步骤与已进行时长的进度。返回 None 表示关闭；每次检查时读取，可热修改。
pub trait QqSlowProgress: Send + Sync {
    fn after(&self) -> Option<std::time::Duration>;
}

/// 连发消息的等待窗口：同一会话里同一人连发的纯文字，在窗口内到达的合成一轮回复。
/// 返回 None 表示关闭；每次准入时读取，可热修改。
pub trait QqDebounce: Send + Sync {
    fn window(&self) -> Option<std::time::Duration>;
}

/// 本条回复要附带的表情包：宿主在本轮对话里选好，通道在回复正文发完后作为图片发出。
pub trait QqStickers: Send + Sync {
    /// 取出并清除这个会话本轮选好的表情包本地路径；没有时为 None。
    fn take(&self, session: &SessionKey) -> Option<std::path::PathBuf>;
}

/// 收到带附件的消息时通知宿主；只传已校验的消息，不做下载。
/// 已校验的全量群消息观察；同步回调只做本地记录和有界排队，不执行模型或工具。
pub trait QqGroupObserver: Send + Sync {
    fn replied(&self, _message: &Message, _text: &str) -> PluginResult<()> {
        Ok(())
    }
    fn observe(
        &self,
        message: &Message,
        sender_name: Option<&str>,
        mentions_me: bool,
        at_ms: u64,
    ) -> PluginResult<()>;
}

pub trait QqAttachmentObserver: Send + Sync {
    fn observe(&self, session: &SessionKey, message: &Message);
}

/// 已结束的失败轮次交给宿主处理；只允许有界的本地持久化，不能运行模型或工具。
/// 返回的说明代替默认的“不会自动重试”；只有确认任务保存后才能宣称已派发。
pub trait QqFailureRecovery: Send + Sync {
    fn recover(&self, report: &eve_control_api::ControlReport) -> PluginResult<Option<String>>;
}

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
use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};
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
    /// 机器人是否拥有 Markdown 权限；无权限时平台拒绝所有发送，故默认关闭。
    pub markdown: bool,
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
    /// 已送达但被交互观察者确定拒绝采集的交流数（如交互记忆容量已满）。
    pub interactions_not_saved: u64,
    /// 回执容量不足时拒绝的新请求数；不进入模型、工具或管理命令。
    pub capacity_rejected: u64,
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
    natural_message_judgement: bool,
    outreach: Option<Arc<bridge::OutreachHook>>,
    files: Option<Arc<dyn QqFileDelivery>>,
    file_poll: Option<Duration>,
    attachment_observer: Option<Arc<dyn QqAttachmentObserver>>,
    raw_observer: Option<Arc<dyn QqRawMessageObserver>>,
    conversations: QqConversations,
    participant_observer: Option<Arc<dyn QqParticipantObserver>>,
    timing_diagnostics: bool,
    slow_progress: Option<Arc<dyn QqSlowProgress>>,
    debounce: Option<Arc<dyn QqDebounce>>,
    stickers: Option<Arc<dyn QqStickers>>,
    failure_recovery: Option<Arc<dyn QqFailureRecovery>>,
    receipt_archive: Option<Arc<dyn QqReceiptArchive>>,
    group_observer: Option<Arc<dyn QqGroupObserver>>,
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
            natural_message_judgement: false,
            outreach: None,
            files: None,
            file_poll: None,
            attachment_observer: None,
            raw_observer: None,
            conversations: QqConversations::default(),
            participant_observer: None,
            timing_diagnostics: false,
            slow_progress: None,
            debounce: None,
            stickers: None,
            failure_recovery: None,
            receipt_archive: None,
            group_observer: None,
        })
    }
    /// 通道已提交过轮次的会话是私聊还是群聊；只读，供只在私聊中成立的宿主能力判断。
    pub fn conversations(&self) -> QqConversations {
        self.conversations.clone()
    }
    pub fn with_failure_recovery(mut self, recovery: Arc<dyn QqFailureRecovery>) -> Self {
        self.failure_recovery = Some(recovery);
        self
    }
    pub fn with_participant_observer(mut self, observer: Arc<dyn QqParticipantObserver>) -> Self {
        self.participant_observer = Some(observer);
        self
    }
    /// 显式开启在途普通文字判断；判断实现由宿主安装的 MessageService 决定。
    /// 默认关闭，普通文字继续排队。只捕获尚未收尾且没有请求取消的当前代，
    /// 判定中的显式命令可抢先执行；其他会话不会等待该判断。
    pub fn with_natural_message_judgement(mut self, enabled: bool) -> Self {
        self.natural_message_judgement = enabled;
        self
    }
    /// 只记录准入与执行阶段，不输出路由标识、消息正文或工具参数。
    pub fn with_timing_diagnostics(mut self, enabled: bool) -> Self {
        self.timing_diagnostics = enabled;
        self
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

    /// 接入宿主的主动交流：普通私聊回复经宿主判断时机合适后，在被动窗口内附带邀请；`push_interval` 给出时，
    /// 通道空闲后按该间隔询问是否主动私聊。未接线时不发送任何邀请。
    pub fn with_outreach(
        mut self,
        outreach: Arc<dyn QqOutreach>,
        push_interval: Option<Duration>,
    ) -> PluginResult<Self> {
        if push_interval.is_some_and(|interval| interval.is_zero()) {
            return Err(PluginError::State("QQBot 主动私聊间隔无效".into()));
        }
        self.outreach = Some(Arc::new(bridge::OutreachHook {
            inner: outreach,
            push_interval,
        }));
        Ok(self)
    }

    /// 接入宿主的文件投递：通道空闲时按 `poll_interval` 询问宿主是否有待发文件，
    /// 用该会话已确认的平台路由上传并回报结果。未接线时不发送任何文件。
    pub fn with_files(
        mut self,
        files: Arc<dyn QqFileDelivery>,
        poll_interval: Duration,
    ) -> PluginResult<Self> {
        if poll_interval.is_zero() {
            return Err(PluginError::State("QQBot 文件投递间隔无效".into()));
        }
        self.files = Some(files);
        self.file_poll = Some(poll_interval);
        Ok(self)
    }

    /// 开启慢步骤进度；与工具里程碑进度共用每轮至多 3 条进度和被动回复配额。未接线时不发。
    pub fn with_slow_progress(mut self, policy: Arc<dyn QqSlowProgress>) -> Self {
        self.slow_progress = Some(policy);
        self
    }

    /// 开启连发合并；未接线时每条消息各自成一轮。
    pub fn with_stickers(mut self, stickers: Arc<dyn QqStickers>) -> Self {
        self.stickers = Some(stickers);
        self
    }
    pub fn with_debounce(mut self, policy: Arc<dyn QqDebounce>) -> Self {
        self.debounce = Some(policy);
        self
    }

    /// 收到带附件的消息时回调宿主；未接线时只把附件说明写进对话文本。
    pub fn with_attachment_observer(mut self, observer: Arc<dyn QqAttachmentObserver>) -> Self {
        self.attachment_observer = Some(observer);
        self
    }

    /// 收到消息时把原始平台事件交给宿主，仅供调试；未接线时丢弃。
    pub fn with_raw_observer(mut self, observer: Arc<dyn QqRawMessageObserver>) -> Self {
        self.raw_observer = Some(observer);
        self
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
impl QqBotPlugin {
    pub fn with_group_observer(mut self, observer: Arc<dyn QqGroupObserver>) -> Self {
        self.group_observer = Some(observer);
        self
    }
    pub fn with_receipt_archive(mut self, archive: Arc<dyn QqReceiptArchive>) -> Self {
        self.receipt_archive = Some(archive);
        self
    }
}
impl Plugin for QqBotPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let mut ledger = state::Ledger::load(&ctx)?;
            ledger.attach_archive(self.receipt_archive.clone())?;
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
            let natural_message_judgement = self.natural_message_judgement;
            let outreach = self.outreach.clone();
            let files = self.files.clone();
            let file_poll = self.file_poll;
            let attachment_observer = self.attachment_observer.clone();
            let raw_observer = self.raw_observer.clone();
            let conversations = self.conversations.clone();
            let participant_observer = self.participant_observer.clone();
            let group_observer = self.group_observer.clone();
            let timing_diagnostics = self.timing_diagnostics;
            let slow_progress = self.slow_progress.clone();
            let debounce = self.debounce.clone();
            let stickers = self.stickers.clone();
            let failure_recovery = self.failure_recovery.clone();
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
                    let outreach = outreach.clone();
                    let files = files.clone();
                    let attachment_observer = attachment_observer.clone();
                    let raw_observer = raw_observer.clone();
                    let conversations = conversations.clone();
                    let participant_observer = participant_observer.clone();
                    let group_observer = group_observer.clone();
                    let slow_progress = slow_progress.clone();
                    let debounce = debounce.clone();
                    let stickers = stickers.clone();
                    let failure_recovery = failure_recovery.clone();
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
                                natural_message_judgement,
                                outreach,
                                files,
                                file_poll,
                                attachment_observer,
                                raw_observer,
                                conversations,
                                participant_observer,
                                group_observer,
                                timing_diagnostics,
                                slow_progress,
                                debounce,
                                stickers,
                                failure_recovery,
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
