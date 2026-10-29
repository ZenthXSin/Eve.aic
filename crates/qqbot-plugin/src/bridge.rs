use crate::{
    QqAttachmentObserver, QqBotConfig, QqBotStatus, QqCommandHandler, QqCommandInput,
    QqFileDelivery, QqFileResult, Segmentation, commands,
    files::valid_path,
    observer::{CompletedInteraction, Observation},
    outreach::{
        OUTREACH_PLANNER, QQ_OUTREACH_JUDGE_TIMEOUT, QQ_OUTREACH_MAX_REPLY_PARTS, QqOutreach,
        QqOutreachMoment, QqOutreachResult, valid_text as valid_outreach_text,
    },
    state::{
        Ledger, Message, Part, PartState, Progress, ReceiptState, Segments, StickerPart,
        passive_progress, valid_message_id,
    },
};
use eve_control_api::{
    CommitState, ControlEvent, ControlEventSink, ControlFuture, ControlInput, ControlPhase,
    ControlReport, ControlService, GenerationKey, RunFailure,
};
use eve_llm_api::{
    LlmError, LlmFuture, ProviderFailure, ProviderFailureKind, TurnEvent, TurnEventKind,
};
use eve_message_api::{
    ClarifyReason, IncomingMessage, MessageFuture, MessageIntent, MessageService, RouteOutcome,
    RouteReport,
};
use eve_plugin_api::{LogEntry, LogLevel, PluginContext, PluginError, PluginResult, TaskSignal};
use eve_session_api::{SessionInput, SessionKey};
use eve_training_api::{TrainingCommand, TrainingService};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::poll_fn,
    process::Stdio,
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    sync::{mpsc, watch},
};

/// /train words 的回复：按学到次数列出编号、词语或场合、含义与原话摘录。
fn phrase_list(phrases: &[eve_training_api::LearnedPhrase]) -> String {
    if phrases.is_empty() {
        return "本会话还没有学到说法；训练开启后，每攒够一批你的原话会学习一次。".into();
    }
    let mut text = format!("本会话从你的原话里学到 {} 条说法：", phrases.len());
    for p in phrases.iter().take(20) {
        let kind = match p.kind {
            eve_training_api::PhraseKind::Term => "词语",
            eve_training_api::PhraseKind::Expression => "表达",
        };
        text.push_str(&format!(
            "\n{} {kind}「{}」：{}（原话：“{}”，学到 {} 次）",
            p.id, p.text, p.meaning, p.example, p.uses
        ));
    }
    if phrases.len() > 20 {
        text.push_str("\n……其余按次数较少未列出。");
    }
    text.push_str("\n不需要的用 /train forget 编号 删除。");
    text
}

fn failure(code: &str) -> PluginError {
    PluginError::Task(code.into())
}
fn warn(ctx: &PluginContext, code: &str) {
    if let Ok(entry) = LogEntry::new(LogLevel::Warn, "eve.qqbot", code) {
        let _ = ctx.log(entry);
    }
}
fn delivery_log(ctx: &PluginContext, code: &str, frame: &Value) {
    let level = if frame.get("ok").and_then(Value::as_bool) == Some(true) {
        LogLevel::Info
    } else {
        LogLevel::Warn
    };
    if let Ok(mut entry) = LogEntry::new(level, "eve.qqbot", code) {
        for name in [
            "http_status",
            "biz_code",
            "elapsed_ms",
            "notice_http_status",
            "notice_biz_code",
        ] {
            if let Some(value) = frame.get(name).and_then(Value::as_i64) {
                entry = entry.with_field(name, value.to_string());
            }
        }
        for name in [
            "failure_kind",
            "delivery_mode",
            "notice_status",
            "notice_failure_kind",
        ] {
            if let Some(value) = frame.get(name).and_then(Value::as_str)
                && [
                    "http",
                    "timeout",
                    "transport_unknown",
                    "dns",
                    "connection",
                    "tls",
                    "passive_window_elapsed",
                    "passive",
                    "proactive",
                    "sent",
                    "failed",
                    "unknown",
                    "not_attempted",
                ]
                .contains(&value)
            {
                entry = entry.with_field(name, value);
            }
        }
        let _ = ctx.log(entry);
    }
}
fn passive_limit(message: &Message) -> usize {
    if message.scope == "c2c" {
        4
    } else {
        crate::QQ_MAX_SEGMENTS
    }
}
fn timing(enabled: bool, phase: &str, turn: Option<u64>) {
    if enabled {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        eprintln!(
            "EVE_QQ_TIMING {}",
            json!({"phase":phase,"turn":turn,"at_ms":at})
        );
    }
}
/// 工具名归为固定的活动类别，不暴露参数。
fn activities<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut activities = std::collections::BTreeSet::new();
    for name in names {
        activities.insert(match name {
            "read_file" | "list_files" | "search_files" | "github_api" => "源码与文件核对",
            "search_reference" | "read_reference" | "search_knowledge" | "search_memory" => {
                "资料检索"
            }
            "write_file" | "apply_patch" => "文件修改",
            "run_command" => "命令执行",
            "prepare_self_upgrade" | "apply_self_upgrade" | "self_upgrade_status" => "升级检查",
            _ => "工具处理",
        });
    }
    activities.into_iter().collect::<Vec<_>>().join("、")
}
/// 本轮当前步骤：由运行时真实事件开始，超过阈值仍未结束时补发一条进度。
struct SlowStep {
    event: ControlEvent,
    label: String,
    since: tokio::time::Instant,
    reported: bool,
    next: tokio::time::Instant,
}
/// 按真实事件更新当前步骤；返回 None 表示该事件不影响步骤记录。
fn slow_label(kind: &TurnEventKind) -> Option<Option<String>> {
    match kind {
        TurnEventKind::ProviderStarted { .. } => Some(Some("生成回复".into())),
        TurnEventKind::ToolBatchStarted { calls } => Some(Some(activities(
            calls.iter().map(|call| call.name.as_str()),
        ))),
        TurnEventKind::ResponseCompleted { .. }
        | TurnEventKind::TurnCompleted { .. }
        | TurnEventKind::Failed { .. } => Some(None),
        _ => None,
    }
}
fn progress_text(kind: &TurnEventKind) -> Option<String> {
    if let TurnEventKind::AssistantMessage { text } = kind {
        Some(text.clone())
    } else {
        None
    }
}
// 无法保存新业务回执时只发送拒绝通知：不运行原请求，失败不重试，不宣称回复已保存。
async fn capacity_notice(
    ctx: &PluginContext,
    status: &watch::Sender<QqBotStatus>,
    stdin: &mut ChildStdin,
    id: &str,
) -> PluginResult<()> {
    warn(ctx, "receipt_limit");
    status.send_modify(|s| {
        s.failed += 1;
        s.capacity_rejected += 1;
    });
    write(stdin, json!({"type":"capacity","version":1,"id":id})).await
}

fn observe_raw(
    observer: &dyn crate::QqRawMessageObserver,
    session: &SessionKey,
    raw: &Value,
    private: bool,
) {
    // 绑定原始事件可能在多个嵌套字段重复密钥，整个事件都不得交给调试读取工具。
    if private {
        return;
    }
    let text = raw.to_string();
    let mut end = text.len().min(24_576);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let bounded = text[..end].to_string();
    observer.record(
        session,
        crate::QqRawMessage {
            bytes: text.len(),
            truncated: text.len() > 24_576,
            json: bounded,
        },
    );
}

async fn write(stdin: &mut ChildStdin, value: Value) -> PluginResult<()> {
    let mut bytes = serde_json::to_vec(&value).map_err(|_| failure("QQBot 命令编码失败"))?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(2), async {
        stdin.write_all(&bytes).await?;
        stdin.flush().await
    })
    .await
    .map_err(|_| failure("QQBot 命令写入超时"))?
    .map_err(|_| failure("QQBot 命令写入失败"))
}
struct Frames {
    reader: BufReader<ChildStdout>,
    partial: Vec<u8>,
    discard: bool,
}
impl Frames {
    async fn next(&mut self) -> PluginResult<Option<Value>> {
        loop {
            let available = self
                .reader
                .fill_buf()
                .await
                .map_err(|_| failure("QQBot 输入读取失败"))?;
            if available.is_empty() {
                return Ok(None);
            }
            let end = available.iter().position(|b| *b == b'\n');
            let count = end.map_or(available.len(), |p| p + 1);
            if !self.discard {
                if self.partial.len() + count > 65536 {
                    self.partial.clear();
                    self.discard = true;
                } else {
                    self.partial.extend_from_slice(&available[..count]);
                }
            }
            self.reader.consume(count);
            if end.is_some() {
                let frame = if self.discard {
                    None
                } else {
                    serde_json::from_slice(&self.partial).ok()
                };
                self.partial.clear();
                self.discard = false;
                return Ok(Some(frame.unwrap_or(Value::Null)));
            }
        }
    }
}
const MAX_PENDING: usize = 16;
const NO_TASK: &str = "当前会话没有可控制的任务，请先发送普通文字开始任务。";
const BLOCKED: &str = "任务提交状态未确认，当前会话已阻塞；请检查保存状态，不能自动重试。";

pub(crate) struct Services {
    pub live: Option<Arc<crate::LiveDialogue>>,
    pub control: Arc<dyn ControlService>,
    pub messages: Arc<dyn MessageService>,
    pub command_handler: Option<Arc<dyn QqCommandHandler>>,
    pub training: Option<Arc<dyn TrainingService>>,
    pub observation: Option<Arc<Observation>>,
    pub segmentation: Option<Arc<Segmentation>>,
    pub natural_message_judgement: bool,
    pub outreach: Option<Arc<OutreachHook>>,
    pub files: Option<Arc<dyn QqFileDelivery>>,
    pub file_poll: Option<Duration>,
    pub attachment_observer: Option<Arc<dyn QqAttachmentObserver>>,
    pub raw_observer: Option<Arc<dyn crate::QqRawMessageObserver>>,
    pub conversations: crate::QqConversations,
    pub group_observer: Option<Arc<dyn crate::QqGroupObserver>>,
    pub participant_observer: Option<Arc<dyn crate::QqParticipantObserver>>,
    pub timing_diagnostics: bool,
    pub slow_progress: Option<Arc<dyn crate::QqSlowProgress>>,
    pub progress_narrator: Option<Arc<dyn crate::QqProgressNarrator>>,
    pub debounce: Option<Arc<dyn crate::QqDebounce>>,
    pub stickers: Option<Arc<dyn crate::QqStickers>>,
    pub failure_recovery: Option<Arc<dyn crate::QqFailureRecovery>>,
    /// 群白名单与主人目录；两者都为 Some 时，未放行的群消息在解析阶段丢弃。
    pub group_whitelist: Option<Arc<dyn crate::QqGroupWhitelist>>,
    pub owner_directory: Option<Arc<dyn crate::QqOwnerDirectory>>,
    /// 宿主工具用的群白名单接口；与上面两项共用同一份持久名单。
    pub group_access: Option<Arc<dyn crate::QqGroupAccess>>,
    pub admission: Option<Arc<dyn crate::QqAdmission>>,
}
/// 宿主接线的主动交流；`push_interval` 为空时不主动私聊。
pub(crate) struct OutreachHook {
    pub inner: Arc<dyn QqOutreach>,
    pub push_interval: Option<Duration>,
}
/// 回执写入宿主失败只记录警告；宿主自行在重启时核对未结束的尝试。
fn report(hook: &OutreachHook, ctx: &PluginContext, id: &str, result: QqOutreachResult) {
    if hook.inner.delivered(id, result).is_err() {
        warn(ctx, "outreach_receipt_unrecorded");
    }
}
fn delivery_unknown(frame: &Value) -> bool {
    (frame.get("ok").and_then(Value::as_bool) == Some(true)
        && !frame
            .get("message_id")
            .and_then(Value::as_str)
            .is_some_and(valid_message_id))
        || frame
            .get("failure_kind")
            .and_then(Value::as_str)
            .is_some_and(|v| matches!(v, "transport_unknown" | "timeout" | "connection"))
}
fn outreach_result(frame: &Value, ok: bool) -> QqOutreachResult {
    if ok {
        QqOutreachResult::Sent {
            platform_message_id: frame
                .get("message_id")
                .and_then(Value::as_str)
                .filter(|id| valid_message_id(id))
                .map(str::to_string),
        }
    } else if delivery_unknown(frame) {
        QqOutreachResult::Unknown
    } else {
        QqOutreachResult::Failed {
            http_status: frame
                .get("http_status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            biz_code: frame.get("biz_code").and_then(Value::as_i64),
        }
    }
}
/// 主动私聊的路由只取自本通道已确认的私聊回执；宿主只给出会话的用户标识。
fn c2c_route(ledger: &Ledger, app: &str, user_id: &str) -> PluginResult<Option<String>> {
    for entry in ledger.entries.iter().rev() {
        if entry.app_id == app
            && entry.message.scope == "c2c"
            && entry.state == ReceiptState::Sent
            && entry.message.session_key(app)?.user_id == user_id
        {
            return Ok(Some(entry.message.target_id.clone()));
        }
    }
    Ok(None)
}
fn session_route(
    ledger: &Ledger,
    app: &str,
    session: &SessionKey,
) -> PluginResult<Option<Message>> {
    for entry in ledger.entries.iter().rev() {
        if entry.app_id == app
            && entry.state == ReceiptState::Sent
            && entry.message.session_key(app)? == *session
        {
            return Ok(Some(entry.message.clone()));
        }
    }
    Ok(None)
}
/// 文件可在用户首轮任务中交付，路由取自已校验并持久接收的消息，不能要求正文先送达。
/// 主动跟进继续使用 session_route 的 Sent 条件，不扩大到未确认的发送。
fn route_for(ledger: &Ledger, app: &str, session: &SessionKey) -> PluginResult<Option<Message>> {
    for entry in ledger.entries.iter().rev() {
        if entry.app_id == app
            && matches!(
                entry.state,
                ReceiptState::Processing | ReceiptState::ReplyPending | ReceiptState::Sent
            )
            && entry.message.session_key(app)? == *session
        {
            return Ok(Some(entry.message.clone()));
        }
    }
    Ok(None)
}
/// 回执写入宿主失败只记录警告；宿主自行在重启时核对未结束的尝试。
fn report_file(
    files: &Arc<dyn QqFileDelivery>,
    ctx: &PluginContext,
    id: &str,
    result: QqFileResult,
) {
    if files.delivered(id, result).is_err() {
        warn(ctx, "file_receipt_unrecorded");
    }
}
/// 平台回执转成宿主结果；不合规的消息编号按没有编号处理。
fn file_result(frame: &Value, ok: bool) -> QqFileResult {
    if ok {
        QqFileResult::Sent {
            platform_message_id: frame
                .get("message_id")
                .and_then(Value::as_str)
                .filter(|id| valid_message_id(id))
                .map(str::to_string),
        }
    } else if delivery_unknown(frame) {
        QqFileResult::Unknown
    } else {
        QqFileResult::Failed {
            http_status: frame
                .get("http_status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            biz_code: frame.get("biz_code").and_then(Value::as_i64),
        }
    }
}
struct ChannelEvents {
    signal: Arc<dyn TaskSignal>,
    stop: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
    timing_diagnostics: bool,
    progress: mpsc::Sender<ControlEvent>,
    /// 慢步骤记录只是提示依据，队列满时丢弃，不影响本轮。
    steps: mpsc::Sender<ControlEvent>,
}
impl ChannelEvents {
    fn is_closed(&self) -> bool {
        self.signal.is_cancelled() || *self.stop.borrow() || *self.closed.borrow()
    }
}
async fn closing(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}
impl ControlEventSink for ChannelEvents {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            let stage = match &event.event.kind {
                TurnEventKind::ProviderStarted { .. } => Some("provider_begin"),
                TurnEventKind::ContextCompactionStarted => Some("context_compaction_begin"),
                TurnEventKind::ResponseCompleted { .. } => Some("provider_completed"),
                TurnEventKind::ToolBatchStarted { .. } => Some("tools_begin"),
                TurnEventKind::ToolResult { .. } => Some("tool_result"),
                TurnEventKind::SessionSaved => Some("session_saved"),
                TurnEventKind::Failed { .. } => Some("turn_failed"),
                _ => None,
            };
            if let Some(stage) = stage {
                timing(self.timing_diagnostics, stage, event.event.turn_id);
            }
            if self.is_closed() {
                Err(LlmError::Cancelled)
            } else {
                if matches!(
                    event.event.kind,
                    TurnEventKind::ToolProgress { .. }
                        | TurnEventKind::ContextCompactionStarted
                        | TurnEventKind::ToolBatchStarted { .. }
                ) {
                    let _ = self.progress.try_send(event.clone());
                }
                if slow_label(&event.event.kind).is_some() {
                    let _ = self.steps.try_send(event);
                }
                Ok(())
            }
        })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                _ = self.signal.cancelled() => {},
                _ = closing(self.stop.clone()) => {},
                _ = closing(self.closed.clone()) => {},
            }
            Ok(())
        })
    }
}

/// 独立路由可被后到的显式命令撤销；已创建的替代代也沿用这个关闭信号。
struct RoutingEvents {
    channel: Arc<ChannelEvents>,
    cancelled: watch::Sender<bool>,
}
impl ControlEventSink for RoutingEvents {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            if *self.cancelled.borrow() {
                Err(LlmError::Cancelled)
            } else {
                self.channel.emit(event).await
            }
        })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                result = self.channel.closed() => result,
                _ = closing(self.cancelled.subscribe()) => Ok(()),
            }
        })
    }
}

struct Active {
    started: tokio::time::Instant,
    message: Message,
    key: GenerationKey,
    ordinary: bool,
    // 必须在提交时捕获；路由器可能已经替换控制服务中的最新代。
    wait: ControlFuture<'static, ControlReport>,
}
struct CommandMessage {
    message: Message,
    private_text: Option<String>,
    target: Option<GenerationKey>,
    natural: bool,
    superseded: bool,
    saved: bool,
}
struct Queued {
    message: Message,
    saved: bool,
    ordinary: bool,
    received: tokio::time::Instant,
}
/// 连发合并的上限：最多几条、从第一条起最多等多久、合并后正文最多多少字节。
const MAX_MERGED: usize = 5;
const MAX_MERGE_HOLD: Duration = Duration::from_secs(6);
const MAX_MERGED_BYTES: usize = 8192;
/// 能合并的只有尚未落盘的纯文字普通消息；附件、引用和恢复的条目各自成一轮。
fn mergeable(queued: &Queued) -> bool {
    queued.ordinary && !queued.saved && queued.message.quoted.is_none()
}
struct Routing {
    message: Message,
    target: GenerationKey,
    wait: MessageFuture<'static, RouteReport>,
    cancelled: Option<watch::Sender<bool>>,
}
struct Reply {
    message: Message,
    text: String,
    guard: ReplyGuard,
    interaction: Option<CompletedInteraction>,
    /// 宿主对附带邀请的时机判断；None 表示尚未判断。
    invite: Option<bool>,
}
type Judging = (
    Reply,
    std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
);
async fn judged(judging: &mut Option<Judging>) -> bool {
    poll_fn(|cx| match judging.as_mut() {
        Some((_, wait)) => wait.as_mut().poll(cx),
        None => Poll::Pending,
    })
    .await
}
/// 可以附带邀请的回复：已完成的模型回复，私聊，且不是命令。
fn invitable(reply: &Reply) -> bool {
    matches!(reply.guard, ReplyGuard::Completed(_))
        && reply.message.scope == "c2c"
        && !explicit(&reply.message.text)
}
/// 一次只投递一条回复。分段时逐段写出并等待回执；段间停顿结束、
/// 路由与命令都收尾后，重新核对代际再写下一段。
struct Delivery {
    reply: Reply,
    pauses: Vec<u64>,
    index: usize,
    pause_until: Option<tokio::time::Instant>,
    /// 随这条被动回复附带的邀请及其片段序号。
    appendix: Option<(String, usize)>,
    /// 正文发完后等待发表情包的时刻；`sticker_sending` 表示已写出、等回执。
    sticker_at: Option<tokio::time::Instant>,
    sticker_sending: bool,
}
/// 正文发完到发表情包之间的停顿。
const STICKER_PAUSE: Duration = Duration::from_millis(800);
impl Delivery {
    fn segmented(&self) -> bool {
        !self.pauses.is_empty()
    }
    fn awaiting(&self) -> bool {
        self.pause_until.is_none() && self.sticker_at.is_none()
    }
}
fn segment_frame(ledger: &Ledger, index: usize, part: usize) -> Value {
    let entry = &ledger.entries[index];
    let reply = entry.reply.as_deref().expect("segmented receipt has reply");
    let parts = &entry.segments.as_ref().expect("segmented receipt").parts;
    let mut frame = json!({"type":"segment","version":1,"id":entry.message.id,"index":part,
        "count":parts.len(),"text":&reply[parts[part].start..parts[part].end]});
    if entry.sticker.is_some() && part + 1 == parts.len() {
        frame["sticker_follows"] = json!(true);
    }
    frame
}
enum ReplyGuard {
    // 控制消息的确认仍可用于已取消/阻塞代，但不能用于已被替换的代。
    Current(GenerationKey),
    Completed(ControlEvent),
    NoTask,
    // 宿主命令的已保存结果不属于聊天生成代，但仍绑定原消息和持久回执。
    Command,
}
impl ReplyGuard {
    fn accepts(&self, control: &dyn ControlService) -> bool {
        match self {
            Self::Current(key) => control
                .snapshot(&key.session)
                .is_ok_and(|s| s.is_some_and(|s| s.key == *key)),
            Self::Completed(event) => control.accepts(event),
            Self::NoTask | Self::Command => true,
        }
    }
}
/// 宿主故障通知不依赖再次调用模型，不进入成功交互观察或训练证据。
fn failed_turn_notice(report: &ControlReport, recovery: Option<&str>) -> String {
    let error = match report.run.failure.as_ref() {
        Some(RunFailure::Execution(error)) => Some(error),
        Some(RunFailure::FailureRecord { execution, .. }) => Some(execution),
        _ => None,
    };
    let persistence = match report.run.failure.as_ref() {
        Some(RunFailure::Commit(_)) => Some("本轮已执行，但结果未能提交保存"),
        Some(RunFailure::FailureRecord { .. }) => Some("本轮失败记录未能保存"),
        Some(RunFailure::Session(eve_session_api::SessionError::Storage)) => {
            Some("会话状态读取或保存失败")
        }
        _ => None,
    };
    let reason = match (persistence, error) {
        (Some(reason), _) => format!("{reason}，请检查存储空间和写入权限"),
        (_, Some(LlmError::Protocol(reason))) if reason == "不同工具批次不能复用调用 ID" => {
            "模型工具调用标识被重复使用，重复批次未执行".into()
        }
        (_, Some(LlmError::Protocol(reason))) if reason.starts_with("任务仍标记为未完成；") => {
            "任务仍未完成，自动续行三次后仍未完成或说明阻塞，已停止本轮".into()
        }
        (_, Some(LlmError::Protocol(_))) => "模型回复格式或工具调用未通过协议检查".into(),
        (_, Some(LlmError::ContextCompactionFailed(failure))) => {
            format!(
                "上下文压缩失败：{}；原始记录与摘要进度保留",
                failure_reason(*failure)
            )
        }
        (_, Some(LlmError::Context(reason))) if reason.starts_with("上下文压缩") => {
            "上下文压缩未能完成，已保存的摘要进度与原始记录仍保留；压缩后的下一次对话请求未发出"
                .into()
        }
        (_, Some(error)) if error.provider_failure().is_some() => {
            failure_reason(error.provider_failure().expect("checked failure"))
        }
        (_, Some(LlmError::ProviderTimeout)) => "模型服务响应超时".into(),
        (_, Some(LlmError::Provider(_))) => "模型服务未能完成回复".into(),
        (_, Some(LlmError::RoundLimit)) => "本轮达到工具调用次数上限".into(),
        (_, Some(LlmError::HistoryWindowExceeded)) => {
            "最近一轮历史超过上下文窗口，原记录仍保留；请在面板 runtime.history 调大 max_bytes"
                .into()
        }
        _ => "本轮执行或状态保存未能完成".into(),
    };
    let effects = match report.run.started_tools {
        Some(0) => "尚未启动工具调用。".into(),
        Some(n) => format!(
            "已启动 {n} 次工具调用，收到 {} 条工具回执；已发生的操作不会自动撤销。",
            report.run.tool_results.len()
        ),
        None => "工具执行情况尚未确认，不能保证没有副作用。".into(),
    };
    let no_tools = if report.run.started_tools == Some(0) && persistence.is_none() {
        match error.and_then(LlmError::provider_failure).map(|f| f.kind) {
            Some(eve_llm_api::ProviderFailureKind::Unauthorized) => {
                Some("请检查模型服务的密钥或分组权限，修复后可继续。")
            }
            _ => None,
        }
    } else {
        None
    };
    let next = recovery
        .or(no_tools)
        .unwrap_or("本轮已停止，不再自动重试；继续前需要核对现有结果。");
    format!("{reason}。{effects}{next}")
}
/// 只用固定类别与状态码说明，不转述供应商错误正文。
fn failure_reason(failure: ProviderFailure) -> String {
    let status = failure
        .http_status
        .map_or(String::new(), |status| format!("（HTTP {status}）"));
    match failure.kind {
        ProviderFailureKind::PaymentRequired => {
            format!("模型服务拒绝请求{status}，需要检查额度或单次输出预算")
        }
        ProviderFailureKind::RateLimited => format!("模型服务限流{status}"),
        ProviderFailureKind::Unauthorized => format!("模型服务拒绝了凭据或权限{status}"),
        ProviderFailureKind::Rejected => format!("模型服务拒绝了这次请求{status}"),
        ProviderFailureKind::ServerError => format!("模型服务内部出错{status}"),
        ProviderFailureKind::Truncated => "模型回复达到单次输出上限，未完整生成".into(),
        ProviderFailureKind::ContentFiltered => "模型服务按内容策略中止了回复".into(),
        ProviderFailureKind::Network => "无法连接模型服务".into(),
        ProviderFailureKind::ResponseTooLarge => "模型回复超过本地接收上限".into(),
        ProviderFailureKind::Incomplete
        | ProviderFailureKind::UnexpectedStatus
        | ProviderFailureKind::Other => format!("模型服务未能完成回复{status}"),
    }
}
fn explicit(text: &str) -> bool {
    text.lines().any(|line| line.trim_start().starts_with('/'))
}
/// 只抢占无需模型判断且参数完整的任务改动；未知、冲突、缺少可信引用的
/// /answer 等交给公开路由器处理，不让一段看似命令的文字取消在途判断。
fn preempts_natural(text: &str, training: bool) -> bool {
    if training
        && matches!(
            TrainingCommand::parse(text),
            Some(TrainingCommand::Start | TrainingCommand::Stop)
        )
    {
        return true;
    }
    let mut cancel = 0;
    let mut new = 0;
    let mut revise = 0;
    let mut count = 0;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        count += 1;
        if count > 16 {
            return false;
        }
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        let payload = line[end..].trim();
        match &line[..end] {
            "/cancel" if payload.is_empty() => cancel += 1,
            "/new" if !payload.is_empty() => new += 1,
            "/add" | "/correct" if !payload.is_empty() => revise += 1,
            _ => return false,
        }
    }
    (cancel == 1 && count == 1) || (new == 1 && revise == 0) || (revise > 0 && new == 0)
}
fn same_session(a: &Message, b: &Message) -> bool {
    a.scope == b.scope && a.target_id == b.target_id && a.user_id == b.user_id
}
fn pending_control(
    message: &Message,
    routing: &[Routing],
    commands: &VecDeque<CommandMessage>,
) -> bool {
    routing.iter().any(|r| same_session(message, &r.message))
        || commands.iter().any(|c| same_session(message, &c.message))
}
async fn routed(routing: &mut [Routing]) -> (usize, eve_message_api::MessageResult<RouteReport>) {
    poll_fn(|cx| {
        for (index, route) in routing.iter_mut().enumerate() {
            if let Poll::Ready(report) = route.wait.as_mut().poll(cx) {
                return Poll::Ready((index, report));
            }
        }
        Poll::Pending
    })
    .await
}
fn mark_failed(ledger: &mut Ledger, ctx: &PluginContext, app: &str, id: &str) -> PluginResult<()> {
    let index = ledger.find(app, id).expect("inserted receipt");
    ledger.entries[index].state = ReceiptState::Failed;
    // 只在没有片段写出时调用：尚未写出的片段全部跳过，已发送片段保持原状。
    if let Some(segments) = ledger.entries[index].segments.as_mut() {
        segments.skip_rest();
    }
    if let Some(sticker) = ledger.entries[index].sticker.as_mut() {
        sticker.state = PartState::Skipped;
    }
    ledger.save(ctx)
}
async fn finish(stdin: &mut ChildStdin, id: &str) -> PluginResult<()> {
    write(stdin, json!({"type":"finish","version":1,"id":id})).await
}
async fn completed(
    active: &mut [Active],
    eligible: &[bool],
) -> (usize, eve_control_api::ControlResult<ControlReport>) {
    poll_fn(|cx| {
        for (index, active) in active.iter_mut().enumerate() {
            if !eligible[index] {
                continue;
            }
            if let Poll::Ready(report) = active.wait.as_mut().poll(cx) {
                return Poll::Ready((index, report));
            }
        }
        Poll::Pending
    })
    .await
}

/// 认领主人群时回填给白名单接口的更新顺序号；取本进程唯一值即可，只用于新记录。
fn claim_tick() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// 主人发起的白名单命令；解析与权限核对都在通道内完成。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccessCommand {
    Open,
    /// 放行主人最近说过话的群；群号由宿主会话目录给出。
    OpenRecent,
    Close,
    List,
}

/// `/allow` 管理当前群，`/allow list` 列出已放行的群，`/allow all` 放行主人最近的群。
/// 不接受参数指定群号：群号只能来自会话。
fn access_command(text: &str) -> Option<(AccessCommand, Option<String>)> {
    let mut words = text.split_whitespace();
    if words.next()? != "/allow" {
        return None;
    }
    let argument = words.next();
    if words.next().is_some() {
        return None;
    }
    match argument {
        None => Some((AccessCommand::Open, None)),
        Some("-d") | Some("close") => Some((AccessCommand::Close, None)),
        Some("all") => Some((AccessCommand::OpenRecent, None)),
        Some("list") => Some((AccessCommand::List, None)),
        Some(_) => None,
    }
}

/// 放行主人最近说过话的群：群号取自通道校验过的群会话目录，跳过当前群与已放行的群；
/// 名单读不出来时返回 None，不猜。
fn admit_recent(
    conversations: &crate::QqConversations,
    whitelist: &dyn crate::QqGroupWhitelist,
    current: Option<&str>,
    at_ms: u64,
) -> Option<Vec<String>> {
    let identities = conversations.recent_identities().ok()?;
    let mut added = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for identity in identities {
        if added.len() >= 20 {
            break;
        }
        if identity.source != crate::QqInteractionSource::GroupMessage {
            continue;
        }
        let Some(group) = conversations.group_route(&identity.conversation) else {
            continue;
        };
        if current == Some(group.as_str()) || !seen.insert(group.clone()) {
            continue;
        }
        if whitelist.add(&group, at_ms).ok()? {
            added.push(group);
        }
    }
    Some(added)
}

/// 白名单读写失败会如实说明，不假装已经生效。
fn access_reply(
    whitelist: &dyn crate::QqGroupWhitelist,
    command: AccessCommand,
    requested: Option<String>,
    group: Option<&str>,
    recent: Option<Vec<String>>,
    at_ms: u64,
) -> String {
    let listed = |whitelist: &dyn crate::QqGroupWhitelist| match whitelist.list() {
        Ok(groups) if groups.is_empty() => {
            "群白名单现在为空：群里只有你本人能跟我说话。".to_string()
        }
        Ok(groups) => format!("已放行的群（{} 个）：\n{}", groups.len(), groups.join("\n")),
        Err(_) => "群白名单读取失败；现有名单未确认".into(),
    };
    match command {
        AccessCommand::List => listed(whitelist),
        AccessCommand::OpenRecent => match recent {
            Some(groups) if groups.is_empty() => "没有找到其他主人群会话；白名单没有变化".into(),
            Some(groups) => format!(
                "已放行这些群（{} 个）：\n{}",
                groups.len(),
                groups.join("\n")
            ),
            None => "群白名单读取失败；白名单没有改动".into(),
        },
        AccessCommand::Close => {
            let Some(group) = group else {
                return "只有群里才有群白名单；私聊不能指定群".into();
            };
            match whitelist.remove(group) {
                Ok(true) => format!("已取消本群放行；之后群里只有你本人能跟我说话。{group}"),
                Ok(false) => format!("本群本来就没放行，现在也没放行。{group}"),
                Err(_) => format!("取消放行失败；名单未改动。{group}"),
            }
        }
        AccessCommand::Open if requested.is_none() => {
            let Some(group) = group else {
                return "只有群里才有群白名单；要放行其他群，得先登记主人账号再用指定群号的方式放行".into();
            };
            match whitelist.add(group, at_ms) {
                Ok(true) => format!("已放行这个群：群友之后跟我说话都会被回应。{group}"),
                Ok(false) => format!("这个群之前就放行了。{group}"),
                Err(_) => format!("放行失败；名单未改动。{group}"),
            }
        }
        AccessCommand::Open => {
            let group = requested.as_deref().expect("checked above");
            match whitelist.add(group, at_ms) {
                Ok(true) => format!("已放行指定的群：群友之后跟我说话都会被回应。{group}"),
                Ok(false) => format!("指定的群之前就放行了。{group}"),
                Err(_) => format!("放行失败；名单未改动。{group}"),
            }
        }
    }
}

pub(crate) async fn run(
    config: Arc<QqBotConfig>,
    ctx: PluginContext,
    services: Services,
    mut ledger: Ledger,
    signal: Arc<dyn TaskSignal>,
    status: watch::Sender<QqBotStatus>,
    mut stop: watch::Receiver<bool>,
) -> PluginResult<()> {
    let Services {
        live: live_dialogue,
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
        progress_narrator,
        debounce,
        stickers,
        failure_recovery,
        group_whitelist,
        owner_directory,
        group_access,
        admission,
    } = services;
    // 只从已验证的持久回执重建路由身份索引，不重放消息、写记忆或触发工具。
    for entry in ledger
        .entries
        .iter()
        .filter(|entry| entry.app_id == config.app_id)
    {
        conversations.record_message(&config.app_id, &entry.message)?;
    }
    // 仅本地学习已验证回执中的用户表达；不提交旧任务、不调用模型或重发消息。
    if let Some(training) = &training {
        for entry in &ledger.entries {
            if entry.app_id != config.app_id {
                continue;
            }
            let session = entry.message.session_key(&config.app_id)?;
            let scope = eve_llm_api::ContextScope {
                session_id: session.session_id,
                user_id: session.user_id,
            };
            if training
                .observe_user_message(&scope, &entry.message.id, &entry.message.text)
                .is_err()
            {
                // 与在线观察一致：学习容量或单条来源冲突不能阻止通道启动。
                // 原表达状态和平台回执保留，不重放工具或发送；错误不能冒充已学习。
                warn(&ctx, "expression_history_observation_failed");
                break;
            }
        }
    }
    let (closed, receiver) = watch::channel(false);
    let (progress_tx, mut progress_rx) = mpsc::channel(64);
    let (steps_tx, mut steps_rx) = mpsc::channel::<ControlEvent>(256);
    let sink = Arc::new(ChannelEvents {
        signal: signal.clone(),
        stop: stop.clone(),
        closed: receiver,
        timing_diagnostics,
        progress: progress_tx,
        steps: steps_tx,
    });
    let mut command = Command::new(&config.node_program);
    command
        .arg(&config.bridge_script)
        .args(&config.bridge_args)
        .env_clear();
    // 先清理继承环境，再写入宿主确定的功能开关，避免开关被 env_clear 清除。
    command.env(
        "QQBOT_GROUP_OBSERVE",
        if group_observer.is_some() {
            "true"
        } else {
            "false"
        },
    );
    // 普通群回复同样指向触发者，不依赖全量群观察功能。
    command.env("QQBOT_GROUP_MENTION", "true");

    for name in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "QQBOT_GROUP_PREFIX_ONLY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("QQBOT_APP_ID", &config.app_id)
        .env("QQBOT_APP_SECRET", &config.app_secret)
        .env(
            "QQBOT_SANDBOX",
            if config.sandbox { "true" } else { "false" },
        )
        .env(
            "QQBOT_MARKDOWN",
            if config.markdown { "true" } else { "false" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| failure("QQBot Node 子进程启动失败"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| failure("QQBot stdin 缺失"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("QQBot stdout 缺失"))?;
    let mut frames = Frames {
        reader: BufReader::new(stdout),
        partial: Vec::new(),
        discard: false,
    };
    let mut queue: VecDeque<Queued> = VecDeque::new();
    let mut commands: VecDeque<CommandMessage> = VecDeque::new();
    let mut active: Vec<Active> = Vec::new();
    let mut controlled_sessions: BTreeMap<String, SessionKey> = BTreeMap::new();
    let mut routing: Vec<Routing> = Vec::new();
    let mut replies: VecDeque<Reply> = VecDeque::new();
    let mut delivering: Option<Delivery> = None;
    let mut progress_delivery: Option<(String, usize)> = None;
    let mut progress_queue: VecDeque<(ControlEvent, Option<String>, Option<u64>)> = VecDeque::new();
    let (narrated_tx, mut narrated_rx) = mpsc::channel(1);
    let mut narrating = false;
    let mut live_waiting: BTreeMap<String, crate::live::LiveSend> = BTreeMap::new();
    // 按原始消息 ID 记录的当前步骤；只用于慢步骤进度，不持久化。
    let mut slow_steps: BTreeMap<String, SlowStep> = BTreeMap::new();
    let mut slow_reported = BTreeSet::new();
    let mut last_progress: BTreeMap<String, tokio::time::Instant> = BTreeMap::new();
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    // 正在主动私聊的邀请；一次只发送一条，期间不开始新的回复投递。
    let mut pushing: Option<String> = None;
    // 正在上传的文件；一次只发一个，期间不开始新的回复投递。
    let mut sending_file: Option<String> = None;
    // 正在等宿主判断时机的回复；判断结束后回到队首照常投递。
    let mut judging: Option<Judging> = None;
    let push_interval = outreach.as_ref().and_then(|hook| hook.push_interval);
    let mut next_push = tokio::time::Instant::now();
    let result: PluginResult<()> = async {
        loop {
            if sink.is_closed() { break Ok(()); }
            ledger.archive_if_needed(&ctx)?;
            let abandoned: Vec<_> = live_waiting.iter().filter(|(id, send)| send.cancelled()
                || !active.iter().any(|a| &a.message.id == *id)).map(|(id, _)| id.clone()).collect();
            for id in abandoned { if let Some(send) = live_waiting.remove(&id) { send.complete(None); } }
            while progress_queue.len() < 64 {
                match progress_rx.try_recv() {
                    Ok(event) => progress_queue.push_back((event, None, None)),
                    Err(_) => break,
                }
            }
            while let Ok(event) = steps_rx.try_recv() {
                let Some(id) = active.iter().find(|a| a.key == event.key).map(|a| a.message.id.clone()) else { continue };
                match slow_label(&event.event.kind).flatten() {
                    Some(label) => { let reported = slow_reported.contains(&id); slow_steps.insert(id, SlowStep { event, label, since: tokio::time::Instant::now(), reported, next: tokio::time::Instant::now() }); }
                    None => { slow_steps.remove(&id); }
                }
            }
            slow_steps.retain(|id, step| active.iter().any(|a| &a.message.id == id && a.key == step.event.key));
            slow_reported.retain(|id| active.iter().any(|a| &a.message.id == id));
            last_progress.retain(|id, _| active.iter().any(|a| &a.message.id == id));
            let stages = slow_progress.as_ref().is_some_and(|p| p.stage_updates());
            let min_interval = slow_progress.as_ref().map_or(Duration::from_secs(15), |p| p.min_interval());
            let slow_after = slow_progress.as_ref().and_then(|policy| policy.after());
            if progress_delivery.is_none() && progress_queue.is_empty()
                && let Some(outgoing) = live_dialogue.as_ref().and_then(|live| live.next()) {
                let current = active.iter().find(|a| a.key.session.session_id == outgoing.scope.session_id
                    && a.key.session.user_id == outgoing.scope.user_id
                    && control.snapshot(&a.key.session).is_ok_and(|s| s.is_some_and(|s| s.turn_id == Some(outgoing.turn_id) && !s.cancel_requested)));
                if let Some(current) = current.filter(|_| !outgoing.cancelled()) {
                    let event = ControlEvent { key: current.key.clone(), event: TurnEvent {
                        turn_id: Some(outgoing.turn_id), kind: TurnEventKind::AssistantMessage { text: outgoing.text.clone() } } };
                    live_waiting.insert(current.message.id.clone(), outgoing);
                    progress_queue.push_front((event, None, None));
                } else { outgoing.complete(None); }
            }
            // 慢步骤：同一步只提示一次；与里程碑进度共用每轮至多 3 条和被动回复配额。
            if let Some(after) = slow_after && progress_queue.is_empty() && progress_delivery.is_none()
                && let Some((id, step)) = slow_steps.iter_mut().find(|(id, step)| (stages || !step.reported) && step.next.elapsed() >= after
                    && last_progress.get(*id).is_none_or(|last| last.elapsed() >= after)) {
                step.reported = true;
                step.next = tokio::time::Instant::now();
                slow_reported.insert(id.clone());
                progress_queue.push_back((step.event.clone(), None, Some(step.since.elapsed().as_secs())));
            }
            // 同会话输出与任务准入等候路由收尾；独立会话可继续处理。
            // 显式命令不等待已撤销的自然判断，路由任务仍由 MessageService 收尾。
            let command_index = commands.iter().position(|command| command.superseded ||
                !routing.iter().any(|route| same_session(&command.message, &route.message)
                    && (command.natural || route.cancelled.is_none())));
            if let Some(index) = command_index {
                let command = commands.remove(index).expect("queued command");
                let message = command.message;
                if command.superseded {
                    if command.saved || ledger.insert(&ctx, &config.app_id, message.clone())? {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &message.id)?;
                    }
                    finish(&mut stdin, &message.id).await?;
                    continue;
                }
                if let Some(training) = &training && let Some(action) = TrainingCommand::parse(&message.text) {
                    if !ledger.insert(&ctx, &config.app_id, message.clone())? {
                        capacity_notice(&ctx, &status, &mut stdin, &message.id).await?;
                        continue;
                    }
                    let session = message.session_key(&config.app_id)?;
                    let scope = eve_llm_api::ContextScope { session_id: session.session_id.clone(), user_id: session.user_id.clone() };
                    if matches!(action, TrainingCommand::Start | TrainingCommand::Stop) {
                        // 先保存开关；当前代取消并保存后才确认或发出新问题。
                        training.set_enabled(&scope, action == TrainingCommand::Start)?;
                        if let Some(snapshot) = control.snapshot(&session).map_err(|_| failure("训练任务快照不可用"))? {
                            control.cancel(&snapshot.key).map_err(|_| failure("训练任务取消失败"))?;
                            control.wait(&snapshot.key).await.map_err(|_| failure("训练任务收尾失败"))?;
                            let retired = active.iter().position(|a| a.key == snapshot.key);
                            if let Some(index) = retired {
                                let old = active.remove(index);
                                conversations.clear_turn_anchor(&old.key.session, &old.message.id)?;
                                mark_failed(&mut ledger, &ctx, &config.app_id, &old.message.id)?;
                                finish(&mut stdin, &old.message.id).await?;
                            }
                        }
                        let mut kept_replies = VecDeque::new();
                        while let Some(reply) = replies.pop_front() {
                            if reply.message.session_key(&config.app_id)? == session {
                                mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                                finish(&mut stdin, &reply.message.id).await?;
                            } else { kept_replies.push_back(reply); }
                        }
                        replies = kept_replies;
                        // 停止/重启训练不继续处理同一会话已经排队的回答。
                        let mut retained = VecDeque::new();
                        while let Some(queued) = queue.pop_front() {
                            if queued.message.session_key(&config.app_id)? == session {
                                if let Some(hook) = &admission { hook.abandon(&session,&format!("qq:{}",queued.message.id))?; }
                                if queued.saved || ledger.insert(&ctx, &config.app_id, queued.message.clone())? {
                                    mark_failed(&mut ledger, &ctx, &config.app_id, &queued.message.id)?;
                                }
                                finish(&mut stdin, &queued.message.id).await?;
                            } else { retained.push_back(queued); }
                        }
                        queue = retained;
                    }
                    if action == TrainingCommand::Start {
                        // 保留 /train start 原文进入 Session；上下文提供提问策略。
                        queue.push_front(Queued { message, saved: true, ordinary: false, received: tokio::time::Instant::now() });
                        continue;
                    }
                    let text = match &action {
                        TrainingCommand::Stop => "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。".into(),
                        TrainingCommand::Status => if training.enabled(&scope)? { "当前会话：主动提问训练已开启。" } else { "当前会话：主动提问训练已关闭。" }.into(),
                        TrainingCommand::Stats => match training.expression_snapshot(&scope)? {
                            Some(s) => format!("本会话已学习 {} 条有效用户表达：字数中位数 {}，短消息 {}%，单段消息 {}%。{}当前要求始终优先；这是本地表达统计。", s.samples, s.median_chars, s.short_percent, s.single_paragraph_percent, if s.samples < 8 { "不足 8 条，暂不采用本会话统计。" } else { "" }),
                            None => "本会话暂无有效表达样本；训练开启后会从普通交流中学习。".into(),
                        },
                        TrainingCommand::Reset => { training.reset_expression(&scope)?; "已重置本会话的表达统计和学到的说法，原始聊天记录保留；旧消息不会再次计入，可继续从新消息学习。".into() },
                        TrainingCommand::Words => phrase_list(&training.phrases(&scope)?),
                        TrainingCommand::Forget(id) => if training.forget_phrase(&scope, id)? { format!("已删除说法 {id}，之后不再参考。") } else { format!("本会话没有说法 {id}；用 /train words 查看编号。") },
                        TrainingCommand::Help => "训练命令：/train start 开始；/train stop 停止采集和主动训练；/train status 查看开关；/train stats 查看表达统计；/train words 查看学到的说法；/train forget 编号 删除一条说法；/train reset 重置本会话统计和说法。".into(),
                        TrainingCommand::Start => unreachable!(),
                    };
                    replies.push_back(Reply { message, text, guard: ReplyGuard::NoTask, interaction: None, invite: None });
                    continue;
                }
                if !command.saved && !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    capacity_notice(&ctx, &status, &mut stdin, &message.id).await?;
                    continue;
                }
                if !command.natural && let Some(id) = message.text.trim().strip_prefix("/receipt ") {
                    let session = message.session_key(&config.app_id)?;
                    let entry = if let Some(i) = ledger.find(&config.app_id,id) { Some(&ledger.entries[i]) } else { None };
                    let cold = if entry.is_none() { ledger.archived(&config.app_id,id)? } else { None };
                    let text = match entry.or(cold.as_ref()).filter(|e| e.message.session_key(&config.app_id).ok().as_ref() == Some(&session)) {
                        Some(e) => format!("{}", json!({"message_id":e.message.id,"input":e.message.text.chars().take(2048).collect::<String>(),"state":e.state,"reply":e.reply.as_ref().map(|r| r.chars().take(2048).collect::<String>()),"note":"正文各最多展示 2048 字；完整冷回执可用 read_raw_message(message_id,offset) 分页读取"})),
                        None => "本会话没有这条回执。".into(),
                    };
                    replies.push_back(Reply { message, text, guard: ReplyGuard::Command, interaction: None, invite: None });
                    continue;
                }
                if !command.natural && message.text.trim() == "/last-reply" {
                    let text = ledger.last_failed(&config.app_id, &message)?
                        .unwrap_or_else(|| "当前会话没有可取回的失败回复。".into());
                    replies.push_back(Reply { message, text, guard: ReplyGuard::Command, interaction: None, invite: None });
                    continue;
                }
                // 群白名单只认主人本人：身份来自配置里登记的账号，群号来自这条消息的真实目标。
                if !command.natural
                    && let (Some(whitelist), Some((access, requested))) =
                        (&group_whitelist, access_command(&message.text))
                {
                    let session = message.session_key(&config.app_id)?;
                    let text = match &owner_directory {
                        Some(owners) if owners.is_owner(&session.user_id).unwrap_or(false) => {
                            let group = (message.scope == "group").then_some(message.target_id.as_str());
                            let recent = (access == AccessCommand::OpenRecent)
                                .then(|| admit_recent(&conversations, whitelist.as_ref(), group, now_ms()))
                                .flatten();
                            access_reply(
                                whitelist.as_ref(),
                                access,
                                requested,
                                group,
                                recent,
                                now_ms(),
                            )
                        }
                        _ => "只有主人本人能改群白名单。".into(),
                    };
                    replies.push_back(Reply { message, text, guard: ReplyGuard::Command, interaction: None, invite: None });
                    continue;
                }
                if !command.natural && let Some(handler) = &command_handler {
                    let session = message.session_key(&config.app_id)?;
                    match commands::dispatch(handler.as_ref(), QqCommandInput {
                        message_id: &message.id, session: &session, text: command.private_text.as_deref().unwrap_or(&message.text),
                    }) {
                        Ok(Some(text)) => {
                            replies.push_back(Reply { message, text, guard: ReplyGuard::Command, interaction: None, invite: None });
                            continue;
                        }
                        Ok(None) if command.private_text.is_some() => {
                            replies.push_back(Reply { message, text: "管理员绑定未启用。".into(), guard: ReplyGuard::Command, interaction: None, invite: None });
                            continue;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            status.send_modify(|s| s.failed += 1);
                            mark_failed(&mut ledger, &ctx, &config.app_id, &message.id)?;
                            warn(&ctx, "host_command_failed_no_retry");
                            finish(&mut stdin, &message.id).await?;
                            if matches!(error, PluginError::State(_)) {
                                // 不传播宿主诊断正文；状态未知时停止后续命令和模型准入。
                                break Err(failure("QQBot 宿主命令状态不可确认，通道已停止"));
                            }
                            continue;
                        }
                    }
                }
                if let Some(target) = command.target.clone() {
                    controlled_sessions.insert(target.session.session_id.clone(), target.session.clone());
                    let incoming = IncomingMessage {
                        message_id: message.id.clone(),
                        target: target.clone(),
                        text: message.turn_text(),
                        // QQ v1 桥接尚未验证澄清引用；不得从文字推断 question_id。
                        reply_to: None,
                    };
                    let cancelled = command.natural.then(|| watch::channel(false).0);
                    let route_sink: Arc<dyn ControlEventSink> = match &cancelled {
                        Some(cancelled) => Arc::new(RoutingEvents { channel: sink.clone(), cancelled: cancelled.clone() }),
                        None => sink.clone(),
                    };
                    let admission_ticket = if let Some(hook) = &admission {
                        match hook.begin_control(&target.session, &format!("control:{}",message.id), message.text.trim()=="/cancel")? {
                            Some(ticket)=>Some(ticket),
                            None=>{
                                commands.push_front(CommandMessage{message,..command});
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                continue;
                            }
                        }
                    } else { None };
                    let submitted = messages.submit(incoming, route_sink);
                    if let (Some(hook),Some(ticket))=(&admission,admission_ticket) {
                        hook.finish(&ticket,if submitted.is_ok(){crate::QqAdmissionResult::Started}else{crate::QqAdmissionResult::Rejected})?;
                    }
                    match submitted {
                        Ok(ticket) => routing.push(Routing { message, target, wait: messages.wait(&ticket), cancelled }),
                        Err(_) => {
                            warn(&ctx, "message_submit_failed");
                            replies.push_back(Reply { message, text: "消息控制暂不可用或已达容量上限；当前任务保持不变。".into(), guard: ReplyGuard::Current(target), interaction: None, invite: None });
                        }
                    }
                } else {
                    replies.push_back(Reply { message, text: NO_TASK.into(), guard: ReplyGuard::NoTask, interaction: None, invite: None });
                }
                continue;
            }
            // 待发文件不等通道空闲：QQ 主动消息随时可发，和当前轮的回复互不占用配额。
            if sending_file.is_none() && file_poll.is_some() && let Some(files) = &files {
                match files.next_file() {
                    Ok(Some(file)) => {
                        let route = route_for(&ledger, &config.app_id, &file.session)?;
                        let name_ok = file.name.as_deref().is_none_or(crate::files::valid_name);
                        let caption_ok = file.caption.as_deref().is_none_or(crate::files::valid_caption);
                        match route.filter(|_| valid_message_id(&file.id) && valid_path(&file.path) && name_ok && caption_ok) {
                            Some(message) => {
                                write(&mut stdin, json!({"type":"file","version":1,"id":file.id,
                                    "scope":message.scope,"target_id":message.target_id,"path":file.path,
                                    "mention_user_id": if message.scope == "group" {Some(message.user_id)} else {None},
                                    "name":file.name,"caption":file.caption})).await?;
                                deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                                sending_file = Some(file.id);
                            }
                            None => report_file(files, &ctx, &file.id, QqFileResult::NotSent),
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(_) => warn(&ctx, "file_delivery_unavailable"),
                }
            }
            if delivering.is_none() && pushing.is_none() && sending_file.is_none() && progress_delivery.is_none()
                && !narrating
                && progress_queue.front().is_some_and(|(event, _, _): &(ControlEvent, Option<String>, Option<u64>)|
                    active.iter().any(|a| a.key == event.key)
                    || !routing.iter().any(|r| r.target.session == event.key.session))
                && let Some((event, text, waiting_seconds)) = progress_queue.pop_front() {
                if text.is_none() && !matches!(event.event.kind, TurnEventKind::AssistantMessage { .. }) {
                    if let Some(narrator) = progress_narrator.clone()
                        && let Some(current) = active.iter().find(|a| a.key == event.key) {
                        let index = ledger.find(&config.app_id, &current.message.id).expect("progress receipt");
                        let previous_messages: Vec<_> = ledger.entries[index].progress.iter().map(|p| p.text.clone()).collect();
                        let ordinary_count = ledger.entries[index].progress.iter().filter(|p| !p.necessary).count();
                        // 群进度只占开头与中间两个名额，保留结尾；必要问题由模型实时工具单独提出。
                        let interval_ok = !stages || previous_messages.is_empty() || last_progress.get(&current.message.id).is_none_or(|last| last.elapsed() >= min_interval);
                        let enabled = waiting_seconds.is_some() || match &event.event.kind {
                            TurnEventKind::ToolBatchStarted { calls } => stages && !calls.iter().any(|c| c.name == "send_message") && !calls.iter().all(|c| c.name == "task_progress"),
                            TurnEventKind::ToolProgress { .. } => stages || slow_progress.as_ref().is_some_and(|p| p.tool_milestones()),
                            _ => true,
                        };
                        if enabled && interval_ok && (current.message.scope == "c2c" || ordinary_count < 2) {
                            let (tool_names, failed_calls) = match &event.event.kind {
                                TurnEventKind::ToolBatchStarted { calls } => (calls.iter().map(|c| c.name.clone()).collect(), 0),
                                TurnEventKind::ToolProgress { tool_names, failed_calls, .. } => (tool_names.clone(), *failed_calls),
                                _ => (vec![], 0),
                            };
                            let facts = crate::QqProgressFacts {
                                user_request: current.message.text.clone(),
                                activity: slow_steps.get(&current.message.id).map(|s| s.label.clone()),
                                phase: if matches!(event.event.kind, TurnEventKind::ContextCompactionStarted) { "compaction" } else if waiting_seconds.is_some() { "waiting" } else if ordinary_count == 0 && matches!(event.event.kind, TurnEventKind::ToolBatchStarted { .. }) { "start" } else { "milestone" }.into(),
                                tool_names, failed_calls, waiting_seconds, previous_messages, group: current.message.scope != "c2c",
                            };
                            let tx = narrated_tx.clone();
                            narrating = true;
                            tokio::spawn(async move {
                                let (text, failed) = match tokio::time::timeout(Duration::from_secs(8), narrator.narrate(facts)).await {
                                    Ok(Ok(text)) => (text, false),
                                    _ => (None, true),
                                };
                                let _ = tx.send((event, text, waiting_seconds, failed)).await;
                            });
                        }
                    }
                    continue;
                }
                // 只使用当前代绑定的原始平台路由；取消、插入控制和旧代事件不补发。
                let elapsed = active.iter().find(|a| a.key == event.key).map(|a| a.started.elapsed()).unwrap_or_default();
                let is_start = waiting_seconds.is_none() && matches!(event.event.kind, TurnEventKind::ToolBatchStarted { .. });
                let message = active.iter().find(|a| a.key == event.key).map(|a| a.message.clone());
                if let Some(message) = message
                    && !pending_control(&message, &routing, &commands)
                    && control.accepts(&event)
                    && control.snapshot(&event.key.session).is_ok_and(|s| s.is_some_and(|s| !s.cancel_requested))
                    && (text.is_some() || !matches!(event.event.kind, TurnEventKind::ToolBatchStarted { .. }) || stages)
                    && (!matches!(event.event.kind, TurnEventKind::ToolProgress { .. })
                        || stages || slow_progress.as_ref().is_some_and(|policy| policy.tool_milestones()))
                    && let Some(text) = text.or_else(|| progress_text(&event.event.kind)) {
                    let index = ledger.find(&config.app_id, &message.id).expect("active receipt");
                    let entry = &mut ledger.entries[index];
                    let automatic_start = matches!(&event.event.kind, TurnEventKind::ToolBatchStarted { calls }
                        if !calls.iter().any(|c| c.name == "send_message")
                            && !calls.iter().all(|c| c.name == "task_progress"));
                    let ordinary_count = entry.progress.iter().filter(|p| !p.necessary).count();
                    let skip_start = is_start
                        && (!automatic_start || ordinary_count != 0);
                    let necessary_question = live_waiting.get(&message.id).is_some_and(|send| send.needs_answer);
                    let interval_ok = necessary_question || !stages || entry.progress.is_empty()
                        || last_progress.get(&message.id).is_none_or(|last| last.elapsed() >= min_interval)
                        || matches!(event.event.kind, TurnEventKind::ContextCompactionStarted);
                    let blocked = entry.progress.iter().any(|p| matches!(p.state, PartState::Failed | PartState::Unknown));
                    // 旧模式沿用 1/30/70 步节奏；新模式更密的事件不能抢占压缩通知名额。
                    let legacy_milestone = stages || match event.event.kind {
                        TurnEventKind::ToolProgress { completed_calls, .. } =>
                            completed_calls >= [1, 30, 70].get(entry.progress.len()).copied().unwrap_or(usize::MAX),
                        _ => true,
                    };
                    let scope_budget = message.scope == "c2c" || necessary_question || ordinary_count < 2;
                    if scope_budget && entry.state == ReceiptState::Processing && !skip_start && interval_ok && legacy_milestone
                        && (!stages || !blocked) && entry.progress.len() < if stages {64} else {3} {
                        let part = entry.progress.len();
                        let proactive = message.scope == "c2c" && stages && (passive_progress(entry) >= 2 || elapsed >= Duration::from_secs(230));
                        entry.progress.push(Progress { text: text.clone(), state: PartState::Sending, proactive, necessary: necessary_question });
                        last_progress.insert(message.id.clone(), tokio::time::Instant::now());
                        ledger.save(&ctx)?;
                        // 同步落盘后再次核验；写出后未知结果不会自动补发。
                        if control.accepts(&event) && control.snapshot(&event.key.session).is_ok_and(|s| s.is_some_and(|s| !s.cancel_requested)) {
                            write(&mut stdin, json!({"type":"progress","version":1,"id":message.id,"index":part,"text":text,"proactive":proactive})).await?;
                            progress_delivery = Some((message.id.clone(), part));
                            deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                        } else {
                            ledger.entries[index].progress[part].state = PartState::Failed;
                            ledger.save(&ctx)?;
                        }
                    }
                }
                if progress_delivery.is_none() && let Some(current) = active.iter().find(|a| a.key == event.key)
                    && let Some(outgoing) = live_waiting.remove(&current.message.id) { outgoing.complete(None); }
                continue;
            }
            if delivering.is_none() && pushing.is_none() && sending_file.is_none() && judging.is_none() && progress_delivery.is_none()
                && !narrating && progress_queue.is_empty() && progress_rx.is_empty()
                && let Some(index) = replies.iter().position(|reply| !pending_control(&reply.message, &routing, &commands)) {
                let reply = replies.remove(index).expect("queued reply");
                if !reply.guard.accepts(control.as_ref()) {
                    mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                    warn(&ctx, "stale_reply_suppressed");
                    finish(&mut stdin, &reply.message.id).await?;
                    continue;
                }
                // 有到期的邀请时，先等宿主看过用户这条消息与回复，判断此刻是否合适附带。
                if let Some(hook) = &outreach && reply.invite.is_none() && invitable(&reply) {
                    let session = reply.message.session_key(&config.app_id)?;
                    match hook.inner.due(&session) {
                        Ok(true) => {
                            let wait = hook.inner.judge(QqOutreachMoment {
                                session,
                                message_id: reply.message.id.clone(),
                                user_text: reply.message.text.clone(),
                                reply_text: reply.text.clone(),
                            });
                            let wait = Box::pin(async move {
                                matches!(tokio::time::timeout(QQ_OUTREACH_JUDGE_TIMEOUT, wait).await, Ok(Ok(true)))
                            });
                            judging = Some((reply, wait));
                            continue;
                        }
                        Ok(false) => {}
                        Err(_) => warn(&ctx, "outreach_unavailable"),
                    }
                }
                // 只分段已完成的模型回复；在开始投递时读取会话设置。
                // 设置读取失败时保持整条投递，但仍须满足宿主预算。
                let plan = match (&segmentation, &reply.guard) {
                    (Some(segmentation), ReplyGuard::Completed(_)) => {
                        let preference = match &segmentation.preferences {
                            None => eve_segment_api::SegmentPreference::default(),
                            Some(store) => match store.get(&crate::segment_scope(&reply.message.session_key(&config.app_id)?)) {
                                Ok(preference) => preference,
                                Err(_) => {
                                    warn(&ctx, "segment_preference_unavailable");
                                    eve_segment_api::SegmentPreference {
                                        enabled: Some(false),
                                        ..eve_segment_api::SegmentPreference::default()
                                    }
                                }
                            },
                        };
                        let progress_count = passive_progress(&ledger.entries[ledger.find(&config.app_id, &reply.message.id).expect("reply receipt")]);
                        let mut policy = segmentation.policy;
                        policy.max_segments = policy.max_segments.min(passive_limit(&reply.message) - progress_count);
                        policy.defaults.max_segments = policy.defaults.max_segments.min(policy.max_segments);
                        match eve_segment_api::plan_with_preference(
                            segmentation.planner.as_ref(), &reply.text, &policy, &preference) {
                            Ok((plan, warning)) => {
                                if warning.is_some() { warn(&ctx, "segment_plan_fallback"); }
                                Some(plan)
                            }
                            Err(_) => {
                                // Session 的完成事实不变，完整回复仍留在失败回执中；
                                // 不绕过宿主预算发送，也不形成已送达的交互证据。
                                let index = ledger.find(&config.app_id, &reply.message.id).expect("inserted receipt");
                                ledger.entries[index].reply = Some(reply.text.clone());
                                mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                                status.send_modify(|s| s.failed += 1);
                                warn(&ctx, "segment_plan_unavailable");
                                finish(&mut stdin, &reply.message.id).await?;
                                continue;
                            }
                        }
                    }
                    _ => None,
                };
                let index = ledger.find(&config.app_id, &reply.message.id).expect("inserted receipt");
                ledger.entries[index].state = ReceiptState::ReplyPending;
                ledger.entries[index].reply = Some(reply.text.clone());
                // 单段也必须使用校验过的切片；首尾空白不能让实际投递超出预算。
                let single = plan.as_ref().filter(|p| p.segments.len() == 1).map(|p| p.segments[0]);
                let plan = plan.filter(|p| p.segments.len() > 1);
                let pauses: Vec<u64> = plan.as_ref().map_or_else(Vec::new, |plan| plan.segments.iter().map(|s| s.pause_before_ms).collect());
                let parts = |ranges: &mut dyn Iterator<Item = (usize, usize)>| ranges.enumerate().map(|(i, (start, end))| Part {
                    start, end,
                    state: if i == 0 { PartState::Sending } else { PartState::Pending },
                }).collect();
                ledger.entries[index].segments = plan.as_ref().map(|plan| Segments {
                    planner: plan.planner.clone(),
                    parts: parts(&mut plan.segments.iter().map(|s| (s.start, s.end))),
                });
                let mut pauses = pauses;
                // 普通私聊回复可以在被动窗口内附带一条邀请，作为最后一段发出；回复本身不变。
                let mut appendix = None;
                if let Some(hook) = &outreach
                    && reply.invite == Some(true)
                    && invitable(&reply)
                    && passive_progress(&ledger.entries[index]) + plan.as_ref().map_or(1, |plan| plan.segments.len()) < passive_limit(&reply.message)
                    && plan.as_ref().map_or(1, |plan| plan.segments.len()) <= QQ_OUTREACH_MAX_REPLY_PARTS {
                    match hook.inner.attach(&reply.message.session_key(&config.app_id)?, &reply.message.id) {
                        Ok(Some(invitation)) => {
                            let base = match (&plan, single) {
                                (Some(plan), _) => Some((plan.planner.as_str(), plan.segments.iter().map(|s| (s.start..s.end, s.pause_before_ms)).collect())),
                                (None, Some(s)) => Some((OUTREACH_PLANNER, vec![(s.start..s.end, 0)])),
                                (None, None) => None,
                            };
                            match crate::outreach::append(&reply.text, base, &invitation.text) {
                                Some(appended) => {
                                    ledger.entries[index].segments = Some(Segments {
                                        planner: appended.planner,
                                        parts: parts(&mut appended.ranges.iter().map(|r| (r.start, r.end))),
                                    });
                                    ledger.entries[index].reply = Some(appended.text);
                                    pauses = appended.pauses;
                                    appendix = Some((invitation.id, appended.part));
                                }
                                None => {
                                    warn(&ctx, "outreach_not_appended");
                                    report(hook, &ctx, &invitation.id, QqOutreachResult::NotSent);
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(_) => warn(&ctx, "outreach_unavailable"),
                    }
                }
                // 本轮选好的表情包作为正文之后的图片发出；带邀请时不附带，也要清掉本轮的选择。
                if let Some(stickers) = &stickers
                    && matches!(reply.guard, ReplyGuard::Completed(_))
                    && let Some(path) = stickers.take(&reply.message.session_key(&config.app_id)?) {
                    let parts = ledger.entries[index].segments.as_ref().map_or(1, |s| s.parts.len());
                    let path = path.to_string_lossy().into_owned();
                    if appendix.is_none()
                        && passive_progress(&ledger.entries[index]) + parts < passive_limit(&reply.message)
                        && !path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control) {
                        ledger.entries[index].sticker = Some(StickerPart { path, state: PartState::Pending });
                    } else {
                        warn(&ctx, "sticker_not_attached");
                    }
                }
                if let Err(error) = ledger.save(&ctx) {
                    if let (Some(hook), Some((id, _))) = (&outreach, &appendix) {
                        report(hook, &ctx, id, QqOutreachResult::NotSent);
                    }
                    return Err(error);
                }
                // 保存也是同步操作；再次检查后至 write 完成不准入任何控制动作。
                if !reply.guard.accepts(control.as_ref()) {
                    if let (Some(hook), Some((id, _))) = (&outreach, &appendix) {
                        report(hook, &ctx, id, QqOutreachResult::NotSent);
                    }
                    mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                    warn(&ctx, "stale_reply_suppressed");
                    finish(&mut stdin, &reply.message.id).await?;
                    continue;
                }
                let frame = if pauses.is_empty() {
                    let text = single.map_or(reply.text.as_str(), |s| &reply.text[s.start..s.end]);
                    let mut frame = json!({"type":"reply","version":1,"id":reply.message.id,"text":text});
                    if ledger.entries[index].sticker.is_some() {
                        frame["sticker_follows"] = json!(true);
                    }
                    frame
                } else {
                    segment_frame(&ledger, index, 0)
                };
                write(&mut stdin, frame).await?;
                timing(timing_diagnostics, "reply_written", None);
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                delivering = Some(Delivery { reply, pauses, index: 0, pause_until: None, appendix, sticker_at: None, sticker_sending: false });
            }
            // 段间停顿结束后写下一段；路由或命令尚未收尾时先等待，取消或新代可在此关闭剩余片段。
            if let Some(delivery) = delivering.as_mut()
                && !pending_control(&delivery.reply.message, &routing, &commands)
                && delivery.pause_until.is_some_and(|until| until <= tokio::time::Instant::now()) {
                let id = delivery.reply.message.id.clone();
                let index = ledger.find(&config.app_id, &id).expect("inserted receipt");
                let part = delivery.index;
                let stale = if !delivery.reply.guard.accepts(control.as_ref()) {
                    true
                } else {
                    ledger.entries[index].segments.as_mut().expect("segmented receipt").parts[part].state = PartState::Sending;
                    ledger.save(&ctx)?;
                    !delivery.reply.guard.accepts(control.as_ref())
                };
                if stale {
                    // 后续片段（含附带的邀请）都没有写出。
                    if let (Some(hook), Some((id, _))) = (&outreach, delivery.appendix.take()) {
                        report(hook, &ctx, &id, QqOutreachResult::NotSent);
                    }
                    delivering = None;
                    mark_failed(&mut ledger, &ctx, &config.app_id, &id)?;
                    warn(&ctx, "stale_segments_suppressed");
                    finish(&mut stdin, &id).await?;
                    continue;
                }
                write(&mut stdin, segment_frame(&ledger, index, part)).await?;
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                delivery.pause_until = None;
            }
            // 正文发完、停顿结束后发表情包；写出前先记为发送中，中断后不重发。
            if let Some(delivery) = delivering.as_mut()
                && delivery.sticker_at.is_some_and(|at| at <= tokio::time::Instant::now()) {
                let id = delivery.reply.message.id.clone();
                let index = ledger.find(&config.app_id, &id).expect("inserted receipt");
                delivery.sticker_at = None;
                delivery.sticker_sending = true;
                let sticker = ledger.entries[index].sticker.as_mut().expect("receipt with sticker");
                sticker.state = PartState::Sending;
                let path = sticker.path.clone();
                ledger.save(&ctx)?;
                write(&mut stdin, json!({"type":"sticker","version":1,"id":id,"path":path})).await?;
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
            }
            let debounce_window = debounce.as_ref().and_then(|policy| policy.window());
            let next_ordinary = queue.iter().position(|queued| {
                if conversations.wakeup_claimed() { return false; }
                let Ok(session) = queued.message.session_key(&config.app_id) else { return false; };
                // 同会话按到达顺序准入：前面的消息还在等待时，后面的不能越过它。
                if queue.iter().take_while(|q| !std::ptr::eq(*q, queued))
                    .any(|q| q.message.session_key(&config.app_id).is_ok_and(|k| k == session)) {
                    return false;
                }
                // 等对方把连发的话说完：同会话最后一条仍在窗口内时暂不准入，最多等 MAX_MERGE_HOLD。
                if let Some(window) = debounce_window && mergeable(queued) {
                    let run: Vec<&Queued> = queue.iter()
                        .filter(|q| mergeable(q) && q.message.session_key(&config.app_id).is_ok_and(|k| k == session))
                        .collect();
                    let latest = run.iter().map(|q| q.received).max().unwrap_or(queued.received);
                    if latest.elapsed() < window && queued.received.elapsed() < MAX_MERGE_HOLD && run.len() < MAX_MERGED {
                        return false;
                    }
                }
                if control.snapshot(&session).ok().flatten().is_some_and(|s| s.phase != eve_control_api::ControlPhase::Finished) { return false; }

                if !natural_message_judgement {
                    return routing.is_empty() && commands.is_empty() && active.is_empty()
                        && delivering.is_none() && replies.is_empty() && judging.is_none();
                }
                if judging.as_ref().is_some_and(|(reply, _)| same_session(&queued.message, &reply.message)) {
                    return false;
                }
                !pending_control(&queued.message, &routing, &commands)
                    && !active.iter().any(|a| same_session(&queued.message, &a.message))
                    && !replies.iter().any(|r| same_session(&queued.message, &r.message))
                    && !delivering.as_ref().is_some_and(|d| same_session(&queued.message, &d.reply.message))
            });
            if let Some(index) = next_ordinary {
                timing(timing_diagnostics, "admission_begin", None);
                let queued = queue.remove(index).expect("queued ordinary message");
                let merge = debounce_window.is_some() && mergeable(&queued);
                let mut saved = queued.saved;
                let mut message = queued.message;
                let mut turn_messages = vec![message.clone()];
                // 回执保存每条消息的原话；合并后的文本只用于本轮会话输入。
                let mut merged_text = None;
                if merge {
                    // 同会话紧随其后的纯文字并入本轮：回复挂在最后一条上，前面的记为 Merged，不单独回复。
                    let session = message.session_key(&config.app_id)?;
                    let mut batch = vec![message];
                    let mut bytes = batch[0].text.len();
                    let mut cursor = index;
                    while cursor < queue.len() && batch.len() < MAX_MERGED {
                        let candidate = &queue[cursor];
                        if candidate.message.session_key(&config.app_id).is_ok_and(|k| k == session) {
                            if !mergeable(candidate) || bytes + 1 + candidate.message.text.len() > MAX_MERGED_BYTES {
                                break;
                            }
                            bytes += 1 + candidate.message.text.len();
                            batch.push(queue.remove(cursor).expect("merged message").message);
                        } else {
                            cursor += 1;
                        }
                    }
                    if batch.len() > 1 {
                        if !ledger.insert_merged(&ctx, &config.app_id, &batch)? {
                            for message in &batch {
                                capacity_notice(&ctx, &status, &mut stdin, &message.id).await?;
                            }
                            continue;
                        }
                        saved = true;
                    }
                    turn_messages = batch.clone();
                    let last = batch.pop().expect("batch keeps the latest message");
                    let mut texts: Vec<String> = Vec::new();
                    for earlier in batch {
                        texts.push(earlier.text.clone());
                        let id = earlier.id.clone();
                        finish(&mut stdin, &id).await?;
                    }
                    message = last;
                    if !texts.is_empty() {
                        texts.push(message.text.clone());
                        merged_text = Some(texts.join("\n"));
                    }
                }
                if !saved && !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    capacity_notice(&ctx, &status, &mut stdin, &message.id).await?;
                    continue;
                }
                if let Some(text) = merged_text {
                    message.text = text;
                }
                let key = message.session_key(&config.app_id)?;
                // 先登记会话类型，本轮工具执行时即可读取。
                conversations.record(&key, &message.scope);
                conversations.bind_turn_messages(&config.app_id, &turn_messages)?;
                let admission_ticket = if let Some(hook) = &admission {
                    match hook.begin(&key, &format!("qq:{}", message.id))? {
                        Some(ticket) => Some(ticket),
                        None => {
                            queue.push_back(Queued { message, saved: true, ordinary: queued.ordinary, received: queued.received });
                            // 短等待让全局队列更高优先级入口先取得租约，不能持锁等待LLM。
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            continue;
                        }
                    }
                } else { None };
                let submitted = control.submit(ControlInput {
                    session: SessionInput { key, text: message.turn_text() },
                    task_id: format!("qq:{}", message.id),
                }, sink.clone());
                if let (Some(hook), Some(ticket)) = (&admission, admission_ticket) {
                    hook.finish(&ticket, match &submitted {
                        Ok(_) => crate::QqAdmissionResult::Started,
                        Err(eve_control_api::ControlError::Busy) => crate::QqAdmissionResult::Retry,
                        Err(_) => crate::QqAdmissionResult::Rejected,
                    })?;
                }
                match submitted {
                    Ok(key) => {
                        timing(timing_diagnostics, "admitted", None);
                        controlled_sessions.insert(key.session.session_id.clone(), key.session.clone());
                        let wait = control.wait(&key); active.push(Active { message, key, ordinary: queued.ordinary, started: tokio::time::Instant::now(), wait });
                    }
                    Err(eve_control_api::ControlError::Busy) => {
                        queue.push_front(Queued { message, saved: true, ordinary: queued.ordinary, received: queued.received });
                    }
                    Err(_) => {
                        conversations.clear_turn_anchor(&message.session_key(&config.app_id)?, &message.id)?;
                        mark_failed(&mut ledger, &ctx, &config.app_id, &message.id)?;
                        status.send_modify(|s| s.failed += 1);
                        warn(&ctx, "control_submit_failed");
                        finish(&mut stdin, &message.id).await?;
                    }
                }
                continue;
            }
            // 主动私聊：通道空闲时定期询问宿主；路由只取自已确认的私聊回执。
            let channel_idle = pushing.is_none() && sending_file.is_none() && delivering.is_none() && progress_delivery.is_none()
                && judging.is_none() && replies.is_empty() && status.borrow().ready;
            let push_ready = push_interval.is_some() && channel_idle;
            if push_ready && let (Some(hook), Some(interval)) = (&outreach, push_interval)
                && tokio::time::Instant::now() >= next_push {
                next_push = tokio::time::Instant::now() + interval;
                match hook.inner.next_push() {
                    Ok(Some(push)) => {
                        let group = hook.inner.group_chime(&push.message.id)?;
                        let target = if let Some(meta) = &group {
                            if push.session.as_ref() == Some(&meta.session) && meta.session.user_id == "eve-chime" {
                                conversations.group_route(&meta.session.session_id).map(|route| ("group".to_string(),route,Some(meta.speaker_id.clone())))
                            } else { None }
                        } else { match &push.session {
                            Some(session) if session.user_id == push.user_id => session_route(&ledger, &config.app_id, session)?.map(|m| {
                                let mention = (m.scope == "group").then_some(m.user_id);
                                (m.scope, m.target_id, mention)
                            }),
                            Some(_) => None,
                            None => c2c_route(&ledger, &config.app_id, &push.user_id)?.map(|target| ("c2c".to_string(), target, None)),
                        }};
                        match target.filter(|_| valid_message_id(&push.message.id) && valid_outreach_text(&push.message.text)) {
                            Some((scope, target, mention)) => {
                                let mut frame=json!({"type":"push","version":1,"id":push.message.id,
                                    "scope":scope,"target_id":target,"text":push.message.text});
                                if let Some(mention)=mention { frame["mention_user_id"]=json!(mention); }
                                if let Some(meta)=group {
                                    frame["parts"]=json!(meta.parts);
                                }
                                write(&mut stdin, frame).await?;
                                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                                pushing = Some(push.message.id);
                            }
                            None => report(hook, &ctx, &push.message.id, QqOutreachResult::NotSent),
                        }
                    }
                    Ok(None) => {}
                    Err(_) => warn(&ctx, "outreach_unavailable"),
                }
                continue;
            }
            conversations.set_idle(active.is_empty() && queue.is_empty() && routing.is_empty() && commands.is_empty()
                && replies.is_empty() && delivering.is_none() && pushing.is_none() && sending_file.is_none() && judging.is_none() && status.borrow().ready);
            let eligible: Vec<bool> = active.iter().map(|a| !pending_control(&a.message, &routing, &commands)).collect();
            tokio::select! {
                biased;
                _ = signal.cancelled() => break Ok(()),
                _ = stop.changed() => break Ok(()),
                _ = tokio::time::sleep_until(deadline), if !status.borrow().ready || delivering.as_ref().is_some_and(Delivery::awaiting) || pushing.is_some() || sending_file.is_some() || progress_delivery.is_some() => {
                    break Err(failure("QQBot ready 或 delivery 等待超时"));
                }
                _ = tokio::time::sleep_until(next_push), if push_ready => {}
                _ = tokio::time::sleep(Duration::from_millis(100)), if !queue.is_empty() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)), if live_dialogue.is_some() => {}
                Some((event, text, waiting, failed)) = narrated_rx.recv(), if narrating => {
                    narrating = false;
                    if failed { warn(&ctx, "progress_narration_unavailable"); }
                    if let Some(text) = text { progress_queue.push_front((event, Some(text), waiting)); }
                }
                Some(event) = progress_rx.recv(), if progress_queue.len() < 64 => { progress_queue.push_back((event, None, None)); }
                Some(event) = steps_rx.recv() => {
                    if let Some(id) = active.iter().find(|a| a.key == event.key).map(|a| a.message.id.clone()) {
                        match slow_label(&event.event.kind).flatten() {
                            Some(label) => { let reported = slow_reported.contains(&id); slow_steps.insert(id, SlowStep { event, label, since: tokio::time::Instant::now(), reported, next: tokio::time::Instant::now() }); }
                            None => { slow_steps.remove(&id); }
                        }
                    }
                }
                _ = tokio::time::sleep_until(slow_steps.iter().filter(|(_, step)| stages || !step.reported).map(|(id, step)| step.next.max(last_progress.get(id).copied().unwrap_or(step.next)) + slow_after.unwrap_or_default()).min().unwrap_or(deadline)),
                    if slow_after.is_some() && progress_delivery.is_none() && slow_steps.values().any(|step| stages || !step.reported) => {}
                invite = judged(&mut judging), if judging.is_some() => {
                    let (mut reply, _) = judging.take().expect("judging reply");
                    reply.invite = Some(invite);
                    replies.push_front(reply);
                }
                // 停顿结束时唤醒：每次循环先处理一条待路由命令，命令清空后才写下一段。
                _ = tokio::time::sleep_until(delivering.as_ref().and_then(|d| d.pause_until.or(d.sticker_at)).unwrap_or(deadline)),
                    if delivering.as_ref().is_some_and(|d| !d.awaiting() && !pending_control(&d.reply.message, &routing, &commands)) => {}
                (index, report) = routed(&mut routing), if !routing.is_empty() => {
                    timing(timing_diagnostics, "routing_completed", None);
                    let route = routing.remove(index);
                    let report = report.map_err(|_| failure("QQBot 消息控制收尾失败"))?;
                    // 已被路由取消的旧轮不再显示。prior 仍由 MessageService/Control 保留，
                    // 不能把它当作控制消息或替代代的完成输出。
                    let prior = match &report.outcome {
                        RouteOutcome::Cancelled { prior } | RouteOutcome::Replaced { prior, .. }
                        | RouteOutcome::ControlFailed { prior, .. } => Some(prior.as_ref()),
                        RouteOutcome::Clarify { prior, .. } | RouteOutcome::Blocked { prior }
                        | RouteOutcome::Stale { prior } | RouteOutcome::Stopped { prior } => prior.as_deref(),
                        RouteOutcome::Unchanged => None,
                    };
                    let retired = prior.filter(|prior| prior.cancel_requested || matches!(prior.run.commit, CommitState::Pending | CommitState::Unknown))
                        .and_then(|prior| active.iter().position(|a| a.key == prior.key));
                    if let Some(index) = retired {
                        let old = active.remove(index);
                        conversations.clear_turn_anchor(&old.key.session, &old.message.id)?;
                        mark_failed(&mut ledger, &ctx, &config.app_id, &old.message.id)?;
                        finish(&mut stdin, &old.message.id).await?;
                    }
                    if route.cancelled.as_ref().is_some_and(|cancelled| *cancelled.borrow())
                        && !matches!(report.outcome, RouteOutcome::Replaced { .. }) {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &route.message.id)?;
                        finish(&mut stdin, &route.message.id).await?;
                        continue;
                    }
                    // 自然判断期间原轮已成功结束：含混文字与闲聊交回正常对话，
                    // 不再让针对旧任务的分类提示截断后续消息。显式命令、失败和未知结果仍走原路径。
                    let conversational = matches!(report.outcome,
                        RouteOutcome::Clarify { reason: ClarifyReason::LowConfidence | ClarifyReason::Conflict
                            | ClarifyReason::Ambiguous | ClarifyReason::MissingQuestion | ClarifyReason::Judge(_), .. })
                        || (matches!(report.outcome, RouteOutcome::Unchanged)
                            && report.decision.as_ref().is_some_and(|d| d.parts.iter().all(|p| p.intent == MessageIntent::Unrelated)));
                    if route.cancelled.is_some() && conversational
                        && control.snapshot(&route.target.session).is_ok_and(|snapshot| snapshot.is_some_and(|s|
                            s.key == route.target && s.phase == ControlPhase::Finished && !s.cancel_requested
                            && s.report.is_some_and(|r| r.run.commit == CommitState::Completed && r.run.failure.is_none()))) {
                        let ordinary = route.message.attachments.is_empty();
                        queue.push_back(Queued { message: route.message, saved: true, ordinary, received: tokio::time::Instant::now() });
                        continue;
                    }
                    let text = match report.outcome {
                        RouteOutcome::Replaced { generation, .. } => {
                            if !control.snapshot(&generation.session)
                                .map_err(|_| failure("QQBot 替代任务快照不可用"))?
                                .is_some_and(|snapshot| snapshot.key == generation) {
                                mark_failed(&mut ledger, &ctx, &config.app_id, &route.message.id)?;
                                warn(&ctx, "stale_replacement_suppressed");
                                finish(&mut stdin, &route.message.id).await?;
                                continue;
                            }
                            let wait = control.wait(&generation);
                            active.push(Active { message: route.message, key: generation, ordinary: false, started: tokio::time::Instant::now(), wait });
                            continue;
                        }
                        RouteOutcome::Unchanged => "当前任务保持不变。".into(),
                        RouteOutcome::Clarify { prompt, .. } => prompt,
                        RouteOutcome::Cancelled { .. } => "已取消当前任务；已完成的工具操作不会撤销。".into(),
                        RouteOutcome::Blocked { .. } => BLOCKED.into(),
                        RouteOutcome::Stale { .. } => {
                            mark_failed(&mut ledger, &ctx, &config.app_id, &route.message.id)?;
                            warn(&ctx, "stale_control_message");
                            finish(&mut stdin, &route.message.id).await?;
                            continue;
                        }
                        RouteOutcome::ControlFailed { .. } => "任务控制未完成；状态已保留，请检查后再发新要求。".into(),
                        RouteOutcome::Stopped { .. } => "消息控制已停止；没有自动重试。".into(),
                    };
                    replies.push_back(Reply { message: route.message, text, guard: ReplyGuard::Current(route.target), interaction: None, invite: None });
                }
                (index, report) = completed(&mut active, &eligible), if !active.is_empty() => {
                    let current = active.remove(index);
                    conversations.clear_turn_anchor(&current.key.session, &current.message.id)?;
                    if let Some(hook) = &admission {
                        hook.completed(&current.key.session, &format!("qq:{}", current.message.id), report.as_ref().is_ok_and(|r| r.run.commit != CommitState::Unknown))?;
                    }
                    let report = report.map_err(|_| failure("QQBot 控制任务收尾失败"))?;
                    let guard = ReplyGuard::Completed(ControlEvent { key: report.key.clone(), event: TurnEvent {
                        turn_id: report.run.turn_id, kind: TurnEventKind::SessionSaved,
                    }});
                    if report.run.commit == CommitState::Completed && report.run.failure.is_none()
                        && let Some(text) = report.run.text.as_ref().filter(|t| !t.trim().is_empty() && t.len() <= 32768) {
                        let interaction = if current.ordinary && let Some(observation) = &observation {
                            match CompletedInteraction::capture(&config.app_id, &current.message, &current.key, &report, observation.sessions.as_ref()) {
                                Ok(interaction) => interaction,
                                Err(error) => {
                                    warn(&ctx, "interaction_evidence_invalid");
                                    return Err(error);
                                }
                            }
                        } else { None };
                        status.send_modify(|s| s.completed += 1);
                        replies.push_back(Reply { message: current.message, text: text.clone(), guard, interaction, invite: None });
                    } else if !report.cancel_requested {
                        status.send_modify(|s| s.failed += 1);
                        warn(&ctx, "turn_failed");
                        let recovery = failure_recovery.as_ref().and_then(|handler| {
                            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.recover(&report))) {
                                Ok(Ok(notice)) => notice,
                                _ => {
                                    warn(&ctx, "failure_recovery_unconfirmed");
                                    Some("后台恢复任务未能确认保存；原记录保留，请核对现有结果。".into())
                                }
                            }
                        });
                        replies.push_back(Reply { message: current.message,
                            text: failed_turn_notice(&report, recovery.as_deref()), guard: ReplyGuard::Current(report.key),
                            interaction: None, invite: None });
                    } else {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &current.message.id)?;
                        finish(&mut stdin, &current.message.id).await?;
                    }
                }
                frame = frames.next() => {
                    conversations.set_idle(false);
                    let Some(frame) = frame? else {
                        if !active.is_empty() || !routing.is_empty() || delivering.is_some() || pushing.is_some() || progress_delivery.is_some() || judging.is_some() || !queue.is_empty()
                            || !commands.is_empty() || !replies.is_empty() || !status.borrow().ready {
                            break Err(failure("QQBot 在未完成交互时断开；保留状态"));
                        }
                        break Ok(());
                    };
                    if frame.get("version").and_then(Value::as_u64) != Some(1) {
                        warn(&ctx, "protocol_version");
                        continue;
                    }
                    match frame.get("type").and_then(Value::as_str) {
                        Some("ready") => { status.send_modify(|s| s.ready = true); }
                        Some("warning") => {
                            let code = match frame.get("code").and_then(Value::as_str) {
                                Some("invalid_route_or_text") => "bridge_invalid_route_or_text",
                                Some("unsupported_message") => "bridge_unsupported_message",
                                Some("pending_limit") => "bridge_pending_limit",
                                Some("outbound_limit") => "bridge_outbound_limit",
                                Some("frame_limit") => "bridge_frame_limit",
                                Some("protocol_version") => "bridge_protocol_version",
                                Some("sdk_error") => "bridge_sdk_error",
                                _ => "bridge_warning",
                            };
                            warn(&ctx, code);
                        }
                        Some("fatal") => break Err(failure("QQBot SDK 启动或连接失败")),
                        Some("delivery") if frame.get("capacity_notice").and_then(Value::as_bool) == Some(true) => {
                            // 拒绝通知没有执行模型、工具或管理命令，也不冒充已保存的业务回执。
                            if frame.get("ok").and_then(Value::as_bool) != Some(true) {
                                delivery_log(&ctx, "capacity_notice_failed_no_retry", &frame);
                            }
                        }
                        Some("delivery") if frame.get("progress").and_then(Value::as_bool) == Some(true) => {
                            let Some((id, part)) = progress_delivery.as_ref() else { warn(&ctx, "unexpected_progress_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(id.as_str()) || frame.get("index").and_then(Value::as_u64) != Some(*part as u64) {
                                warn(&ctx, "progress_delivery_mismatch"); continue;
                            }
                            let Some(reported_ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let ok = reported_ok && frame.get("message_id").and_then(Value::as_str).is_some_and(valid_message_id);
                            let index = ledger.find(&config.app_id, id).expect("progress receipt");
                            ledger.entries[index].progress[*part].state = if ok { PartState::Sent } else if delivery_unknown(&frame) { PartState::Unknown } else { PartState::Failed };
                            ledger.save(&ctx)?;
                            if !ok { delivery_log(&ctx, "progress_failed_no_retry", &frame); }
                            if let Some(outgoing) = live_waiting.remove(id) { outgoing.complete(if ok { frame.get("message_id").and_then(Value::as_str).map(str::to_owned) } else { None }); }
                            progress_delivery = None;
                        }
                        Some("delivery") if frame.get("file").and_then(Value::as_bool) == Some(true) => {
                            let (Some(files), Some(current)) = (&files, sending_file.as_deref()) else { warn(&ctx, "unexpected_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(current) { warn(&ctx, "delivery_id_mismatch"); continue; }
                            let Some(reported_ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let ok = reported_ok && frame.get("message_id").and_then(Value::as_str).is_some_and(valid_message_id);
                            if !ok { delivery_log(&ctx, "file_delivery_failed_no_retry", &frame); }
                            report_file(files, &ctx, current, file_result(&frame, ok));
                            sending_file = None;
                        }
                        Some("delivery") if frame.get("push").and_then(Value::as_bool) == Some(true) => {
                            let (Some(hook), Some(current)) = (&outreach, pushing.as_deref()) else { warn(&ctx, "unexpected_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(current) { warn(&ctx, "delivery_id_mismatch"); continue; }
                            let Some(reported_ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let ok = reported_ok && frame.get("message_id").and_then(Value::as_str).is_some_and(valid_message_id);
                            if !ok { delivery_log(&ctx, "outreach_push_failed_no_retry", &frame); }
                            report(hook, &ctx, current, outreach_result(&frame, ok));
                            pushing = None;
                        }
                        Some("delivery") if frame.get("sticker").and_then(Value::as_bool) == Some(true) => {
                            let Some(delivery) = delivering.as_ref().filter(|d| d.sticker_sending) else { warn(&ctx, "unexpected_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(delivery.reply.message.id.as_str()) {
                                warn(&ctx, "delivery_id_mismatch"); continue;
                            }
                            let Some(reported_ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let ok = reported_ok && frame.get("message_id").and_then(Value::as_str).is_some_and(valid_message_id);
                            let index = ledger.find(&config.app_id, &delivery.reply.message.id).expect("inserted receipt");
                            ledger.entries[index].sticker.as_mut().expect("receipt with sticker").state =
                                if ok { PartState::Sent } else if delivery_unknown(&frame) { PartState::Unknown } else { PartState::Failed };
                            ledger.save(&ctx)?;
                            if !ok { delivery_log(&ctx, "sticker_failed_no_retry", &frame); }
                            delivering = None;
                        }
                        Some("delivery") => {
                            let Some(delivery) = delivering.as_mut().filter(|d| d.awaiting() && !d.sticker_sending) else { warn(&ctx, "unexpected_delivery"); continue; };
                            let message = &delivery.reply.message;
                            if frame.get("id").and_then(Value::as_str) != Some(message.id.as_str()) {
                                warn(&ctx, "delivery_id_mismatch"); continue;
                            }
                            let Some(reported_ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let ok = reported_ok && frame.get("message_id").and_then(Value::as_str).is_some_and(valid_message_id);
                            let index = ledger.find(&config.app_id, &message.id).expect("inserted receipt");
                            if delivery.segmented() {
                                let part = delivery.index;
                                if frame.get("index").and_then(Value::as_u64) != u64::try_from(part).ok() {
                                    warn(&ctx, "delivery_index_mismatch"); continue;
                                }
                                let segments = ledger.entries[index].segments.as_mut().expect("segmented receipt");
                                let last = part + 1 == segments.parts.len();
                                segments.parts[part].state = if ok { PartState::Sent } else if delivery_unknown(&frame) { PartState::Unknown } else { PartState::Failed };
                                if !ok {
                                    segments.skip_rest();
                                    ledger.entries[index].state = if delivery_unknown(&frame) { ReceiptState::Unknown } else { ReceiptState::Failed };
                                } else if last {
                                    ledger.entries[index].state = ReceiptState::Sent;
                                }
                                ledger.save(&ctx)?;
                                // 邀请段的回执如实交回宿主；前面片段失败时邀请没有写出。
                                if let (Some(hook), Some((id, appended))) = (&outreach, &delivery.appendix) {
                                    if part == *appended {
                                        report(hook, &ctx, id, outreach_result(&frame, ok));
                                    } else if !ok {
                                        report(hook, &ctx, id, QqOutreachResult::NotSent);
                                    }
                                }
                                if ok && !last {
                                    // 已确认片段不再重发；下一段只在停顿后重新核对代际才写出。
                                    delivery.index += 1;
                                    delivery.pause_until = Some(tokio::time::Instant::now()
                                        + Duration::from_millis(delivery.pauses[delivery.index]));
                                    continue;
                                }
                            } else {
                                ledger.entries[index].state = if ok { ReceiptState::Sent } else if delivery_unknown(&frame) { ReceiptState::Unknown } else { ReceiptState::Failed };
                                ledger.save(&ctx)?;
                            }
                            let reply = &delivery.reply;
                            let message = &reply.message;
                            status.send_modify(|s| { if ok { s.sent += 1; } else { s.failed += 1; } });
                            if !ok { delivery_log(&ctx, "delivery_failed_no_retry", &frame); }
                            else if frame.get("delivery_mode").and_then(Value::as_str) == Some("proactive") {
                                delivery_log(&ctx, "late_reply_proactive_sent", &frame);
                            }
                            if ok { timing(timing_diagnostics, "delivery_confirmed", None); }
                            if ok && let Some(observer)=&group_observer && observer.replied(message,&reply.text).is_err() { warn(&ctx,"group_reply_observation_failed"); }
                            if ok && let Some(interaction) = &reply.interaction
                                && let Some(observation) = &observation
                                && let Err(error) = interaction.observe(observation.observer.as_ref(), &config.app_id, message, &reply.text) {
                                if !matches!(error, PluginError::Task(_)) {
                                    warn(&ctx, "interaction_observer_failed_no_retry");
                                    return Err(error);
                                }
                                // 确定拒绝（如记忆容量已满）不影响已送达的回复，计数后继续服务。
                                warn(&ctx, "interaction_not_saved");
                                status.send_modify(|s| s.interactions_not_saved += 1);
                            }
                            // 正文送达后停顿一下再发表情包；正文失败时表情包不再发。
                            if let Some(sticker) = ledger.entries[index].sticker.as_mut()
                                && sticker.state == PartState::Pending {
                                if ok {
                                    delivery.sticker_at = Some(tokio::time::Instant::now() + STICKER_PAUSE);
                                    continue;
                                }
                                sticker.state = PartState::Skipped;
                                ledger.save(&ctx)?;
                            }
                            delivering = None;
                        }
                        Some("group_observation") => {
                            let Some(observer) = &group_observer else { continue; };
                            let Ok(message) = serde_json::from_value::<Message>(frame["message"].clone()) else {
                                warn(&ctx, "invalid_group_observation"); continue;
                            };
                            if !message.valid() || message.scope != "group" || message.text.trim_start().starts_with('/') {
                                continue;
                            }
                            // 未放行的群不记录会话、不进群聊观察，与普通消息同一判定。
                            if let Some(whitelist) = &group_whitelist
                                && !whitelist.contains(&message.target_id).unwrap_or(false)
                            {
                                continue;
                            }
                            conversations.record_message(&config.app_id, &message)?;
                            let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |v| v.as_millis() as u64);
                            if observer.observe(&message, frame["sender_name"].as_str(), frame["mentions_me"].as_bool().unwrap_or(false), at_ms).is_err() {
                                warn(&ctx,"group_observation_failed");
                            }
                        }
                        Some("message") => {
                            let mut payload = frame.clone();
                            let sender_name = payload.get("sender_name").and_then(Value::as_str).map(str::to_owned);
                            if let Some(object) = payload.as_object_mut() {
                                object.remove("type"); object.remove("version");
                                object.remove("sender_name"); object.remove("raw");
                                if object.keys().any(|key| !["id", "scope", "target_id", "user_id", "text", "attachments", "quoted"].contains(&key.as_str())) {
                                    warn(&ctx, "ignored_message_fields");
                                }
                                object.retain(|key, _| ["id", "scope", "target_id", "user_id", "text", "attachments", "quoted"].contains(&key.as_str()));
                            }
                            let raw_debug = frame.get("raw").cloned().filter(|value| !value.is_null());
                            let Ok(mut message) = serde_json::from_value::<Message>(payload) else { warn(&ctx, "invalid_message"); continue; };
                            if !message.valid() { warn(&ctx, "invalid_route_or_text"); continue; }
                            // 绑定密钥只保留在命令处理器的临时内存；回执、观察器、训练与模型只能看到脱敏正文。
                            let private_text = (message.text.split_whitespace().next() == Some("/bind"))
                                .then(|| std::mem::replace(&mut message.text, "/bind [已隐藏]".into()));
                            let live = queue.iter().any(|m| m.message.id == message.id)
                                || commands.iter().any(|c| c.message.id == message.id)
                                || active.iter().any(|a| a.message.id == message.id)
                                || routing.iter().any(|r| r.message.id == message.id)
                                || replies.iter().any(|r| r.message.id == message.id)
                                || judging.as_ref().is_some_and(|(r, _)| r.message.id == message.id)
                                || delivering.as_ref().is_some_and(|d| d.reply.message.id == message.id);
                            if live { warn(&ctx, "duplicate_pending"); continue; }
                            let duplicate = ledger.contains(&config.app_id, &message.id)?;
                            let pending = queue.len() + commands.len() + active.len() + replies.len()
                                + routing.len() + usize::from(delivering.is_some()) + usize::from(judging.is_some());
                            if duplicate || pending >= MAX_PENDING {
                                warn(&ctx, if duplicate { "duplicate_no_replay" } else { "queue_limit" });
                                finish(&mut stdin, &message.id).await?;
                            } else {
                                status.send_modify(|s| s.received += 1);
                                timing(timing_diagnostics, "message_received", None);
                                conversations.record_message(&config.app_id, &message)?;
                                // 主人本人在群里说过话的会话，供群白名单工具定位群；只来自校验过的消息。
                                if message.scope == "group"
                                    && let Some(owners) = &owner_directory
                                    && owners.is_owner(&message.session_key(&config.app_id)?.user_id).unwrap_or(false)
                                    && let Some(access) = &group_access
                                {
                                    let conversation = message.conversation_key(&config.app_id)?;
                                    // 同时记进会话目录：白名单工具按会话前缀找群号，重启后也不用等新消息。
                                    conversations.record_group_identity(
                                        &config.app_id,
                                        message.session_key(&config.app_id)?,
                                        &message.target_id,
                                        &message.user_id,
                                        conversation.clone(),
                                    )?;
                                    let _ = access.claim_owner_group(
                                        message.session_key(&config.app_id)?,
                                        conversation,
                                        message.target_id.clone(),
                                        claim_tick(),
                                    );
                                }
                                // 群白名单：主人本人始终放行；其他人的消息只在已放行的群进入。
                                if message.scope == "group"
                                    && let (Some(whitelist), Some(owners)) =
                                        (&group_whitelist, &owner_directory)
                                {
                                    let owner = owners.is_owner(&message.session_key(&config.app_id)?.user_id).unwrap_or(false);
                                    let admitted = whitelist.contains(&message.target_id);
                                    match admitted {
                                        Ok(true) => {}
                                        Ok(false) if owner => {}
                                        Ok(false) => {
                                            warn(&ctx, "group_not_whitelisted");
                                            finish(&mut stdin, &message.id).await?;
                                            continue;
                                        }
                                        // 名单读不出来时不猜测放行，也不进模型；结束回执后继续服务主人。
                                        Err(_) => {
                                            warn(&ctx, "group_whitelist_unavailable");
                                            finish(&mut stdin, &message.id).await?;
                                            continue;
                                        }
                                    }
                                }
                                if let Some(observer) = &attachment_observer { observer.observe(&message.session_key(&config.app_id)?, &message); }
                                if let (Some(observer), Some(raw)) = (&raw_observer, raw_debug.as_ref()) {
                                    observe_raw(observer.as_ref(), &message.session_key(&config.app_id)?, raw, private_text.is_some());
                                }
                                if private_text.is_none() && let Some(observer) = &participant_observer {
                                    let session = message.session_key(&config.app_id)?;
                                    if let Err(error) = observer.observe(&session, &message.conversation_key(&config.app_id)?, message.scope == "c2c", &message.text, sender_name.as_deref()) {
                                        if matches!(error, PluginError::State(_)) { return Err(error); }
                                        warn(&ctx, "participant_metadata_not_saved");
                                    }
                                }
                                if explicit(&message.text) {
                                    let session = message.session_key(&config.app_id)?;
                                    let target = control.snapshot(&session)
                                        .map_err(|_| failure("QQBot 任务快照不可用"))?.map(|s| s.key);
                                    if preempts_natural(&message.text, training.is_some()) {
                                        for route in &routing {
                                            if same_session(&message, &route.message) && let Some(cancelled) = &route.cancelled {
                                                cancelled.send_replace(true);
                                            }
                                        }
                                        for command in &mut commands {
                                            if command.natural && same_session(&message, &command.message) {
                                                command.superseded = true;
                                            }
                                        }
                                    }
                                    commands.push_back(CommandMessage { message, private_text, target, natural: false, superseded: false, saved: false });
                                } else {
                                    // 按普通输入准入时的持久开关采集，不能延后到模型执行。
                                    // 启停/重置可先于排队任务执行；旧输入必须已有去重凭据。
                                    if message.attachments.is_empty() && let Some(training) = &training {
                                        let session = message.session_key(&config.app_id)?;
                                        let scope = eve_llm_api::ContextScope { session_id: session.session_id, user_id: session.user_id };
                                        if training.observe_user_message(&scope, &message.id, &message.text).is_err() {
                                            warn(&ctx, "expression_learning_failed");
                                        }
                                    }
                                    if let Some(live) = &live_dialogue {
                                        let session = message.session_key(&config.app_id)?;
                                        let mut saved = false;
                                        let current = active.iter().find(|a| a.key.session == session)
                                            .map(|a| (a.key.clone(), a.message.id.clone()));
                                        if let Some((current_key, parent_id)) = current
                                            && let Some(snapshot) = control.snapshot(&session).map_err(|_| failure("QQBot 任务快照不可用"))?
                                            && !snapshot.cancel_requested && snapshot.key == current_key
                                            && let Some(turn_id) = snapshot.turn_id {
                                            let scope = eve_llm_api::ContextScope { session_id: session.session_id, user_id: session.user_id };
                                            if !ledger.insert(&ctx, &config.app_id, message.clone())? {
                                                capacity_notice(&ctx, &status, &mut stdin, &message.id).await?;
                                                continue;
                                            }
                                            saved = true;
                                            if live.steer(&scope, turn_id, message.turn_text(), || {
                                                ledger.steer(&ctx, &config.app_id, &message.id, &parent_id)
                                            })? {
                                                finish(&mut stdin, &message.id).await?;
                                                continue;
                                            }
                                        }
                                        let ordinary = message.attachments.is_empty();
                                        queue.push_back(Queued { message, saved, ordinary, received: tokio::time::Instant::now() });
                                        continue;
                                    }
                                    let target = if natural_message_judgement {
                                        control.snapshot(&message.session_key(&config.app_id)?)
                                            .map_err(|_| failure("QQBot 任务快照不可用"))?
                                            .filter(|s| !s.cancel_requested && matches!(s.phase,
                                                ControlPhase::Starting | ControlPhase::Generating | ControlPhase::Tools | ControlPhase::Committing))
                                            .map(|s| s.key)
                                    } else { None };
                                    if target.is_some() {
                                        // 捕获代际时即持久化；即使仍在等同会话的判断，重启也不能把旧控制文字当新任务。
                                        if !ledger.insert(&ctx, &config.app_id, message.clone())? {
                                            capacity_notice(&ctx, &status, &mut stdin, &message.id).await?;
                                            continue;
                                        }
                                        commands.push_back(CommandMessage { message, private_text: None, target, natural: true, superseded: false, saved: true });
                                    } else {
                                        let ordinary = message.attachments.is_empty();
                                        queue.push_back(Queued { message, saved: false, ordinary, received: tokio::time::Instant::now() });
                                    }
                                }
                            }
                        }
                        _ => warn(&ctx, "unknown_or_malformed_frame"),
                    }
                }
            }
        }
    }.await;
    // 停止时尚未写出的邀请段确定没有发送；已写出但未收到回执的留给宿主重启时核对。
    if let (Some(hook), Some(delivery)) = (&outreach, &delivering)
        && let Some((id, part)) = &delivery.appendix
        && ledger
            .find(&config.app_id, &delivery.reply.message.id)
            .is_some_and(|index| {
                ledger.entries[index]
                    .segments
                    .as_ref()
                    .is_some_and(|segments| {
                        matches!(
                            segments.parts[*part].state,
                            PartState::Pending | PartState::Skipped
                        )
                    })
            })
    {
        report(hook, &ctx, id, QqOutreachResult::NotSent);
    }
    // MessageService 拥有准入动作；停止不能丢弃 wait 后遗留它启动的替代代。
    // 先等路由收尾，再查询目标会话最新代，随后取消并等待全部本地执行器。
    closed.send_replace(true);
    let mut cleanup_error = None;
    for route in routing {
        if route.wait.await.is_err() {
            cleanup_error = Some(failure("QQBot 消息控制收尾失败"));
        }
    }
    let mut settling = Vec::new();
    for session in controlled_sessions.into_values() {
        match control.snapshot(&session) {
            Ok(Some(snapshot)) => {
                let wait = control.wait(&snapshot.key);
                if control.cancel(&snapshot.key).is_err() {
                    cleanup_error = Some(failure("QQBot 取消失败"));
                }
                settling.push(wait);
            }
            Ok(None) => {}
            Err(_) => cleanup_error = Some(failure("QQBot 停止时任务快照不可用")),
        }
    }
    for wait in settling {
        if wait.await.is_err() {
            cleanup_error = Some(failure("QQBot 取消收尾失败"));
        }
    }
    let _ = write(&mut stdin, json!({"type":"stop","version":1})).await;
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result.and(cleanup_error.map_or(Ok(()), Err))
}

#[cfg(test)]
mod tests {
    use super::preempts_natural;

    #[derive(Default)]
    struct RawCapture(std::sync::Mutex<Vec<crate::QqRawMessage>>);
    impl crate::QqRawMessageObserver for RawCapture {
        fn record(&self, _: &eve_session_api::SessionKey, raw: crate::QqRawMessage) {
            self.0.lock().unwrap().push(raw);
        }
    }

    #[test]
    fn raw_observation_bounds_multibyte_input_without_panicking() {
        let capture = RawCapture::default();
        let session = eve_session_api::SessionKey::new("session", "user").unwrap();
        let raw = serde_json::json!({"content": format!("x{}", "界".repeat(10_000))});
        let source = raw.to_string();
        assert!(!source.is_char_boundary(24_576));
        super::observe_raw(&capture, &session, &raw, false);
        let rows = capture.0.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, source.len());
        assert!(rows[0].truncated);
        assert!(rows[0].json.len() <= 24_576);
        assert!(source.starts_with(&rows[0].json));
    }

    #[test]
    fn raw_observation_never_exposes_private_bind_payload() {
        let capture = RawCapture::default();
        let session = eve_session_api::SessionKey::new("session", "user").unwrap();
        let raw = serde_json::json!({"content": "/bind secret-token", "nested": {"echo": "secret-token"}});
        super::observe_raw(&capture, &session, &raw, true);
        assert!(capture.0.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_message_id_is_unknown_for_files_and_outreach() {
        let missing = serde_json::json!({"ok":true});
        assert!(super::delivery_unknown(&missing));
        assert_eq!(
            super::file_result(&missing, false),
            crate::QqFileResult::Unknown
        );
        assert_eq!(
            super::outreach_result(&missing, false),
            crate::QqOutreachResult::Unknown
        );
        let explicit = serde_json::json!({"ok":false,"failure_kind":"http","http_status":403});
        assert!(!super::delivery_unknown(&explicit));
        assert!(matches!(
            super::file_result(&explicit, false),
            crate::QqFileResult::Failed {
                http_status: Some(403),
                ..
            }
        ));
    }

    #[test]
    fn access_command_parses_only_exact_forms() {
        use super::{AccessCommand, access_command};
        assert_eq!(access_command("/allow"), Some((AccessCommand::Open, None)));
        assert_eq!(
            access_command("/allow all"),
            Some((AccessCommand::OpenRecent, None))
        );
        assert_eq!(
            access_command("/allow -d"),
            Some((AccessCommand::Close, None))
        );
        assert_eq!(
            access_command("/allow list"),
            Some((AccessCommand::List, None))
        );
        for bad in [
            "/allow 12 3",
            "/allow abc",
            "/allow １２３",
            "/allow 1 2",
            "/allow group:987",
            "/allow 123456",
            "/allowed",
            "allow",
            "/cancel",
        ] {
            assert_eq!(access_command(bad), None, "{bad}");
        }
    }

    struct MemoryWhitelist(std::sync::Mutex<std::collections::BTreeSet<String>>);
    impl crate::QqGroupWhitelist for MemoryWhitelist {
        fn contains(&self, group_id: &str) -> eve_plugin_api::PluginResult<bool> {
            Ok(self.0.lock().unwrap().contains(group_id))
        }
        fn add(&self, group_id: &str, _: u64) -> eve_plugin_api::PluginResult<bool> {
            Ok(self.0.lock().unwrap().insert(group_id.into()))
        }
        fn remove(&self, group_id: &str) -> eve_plugin_api::PluginResult<bool> {
            Ok(self.0.lock().unwrap().remove(group_id))
        }
        fn list(&self) -> eve_plugin_api::PluginResult<Vec<String>> {
            Ok(self.0.lock().unwrap().iter().cloned().collect())
        }
    }

    #[test]
    fn access_reply_reports_real_effect_and_refuses_private_group_management() {
        use super::{AccessCommand, access_reply};
        let whitelist = MemoryWhitelist(Default::default());
        assert_eq!(
            access_reply(&whitelist, AccessCommand::Open, None, Some("111"), None, 1),
            "已放行这个群：群友之后跟我说话都会被回应。111"
        );
        assert!(whitelist.0.lock().unwrap().contains("111"));
        assert!(
            access_reply(&whitelist, AccessCommand::Open, None, None, None, 1).contains("只有群里")
        );
        assert!(!whitelist.0.lock().unwrap().contains(""));
        assert_eq!(
            access_reply(&whitelist, AccessCommand::Close, None, Some("111"), None, 1),
            "已取消本群放行；之后群里只有你本人能跟我说话。111"
        );
        assert!(!whitelist.0.lock().unwrap().contains("111"));
        assert!(
            access_reply(&whitelist, AccessCommand::List, None, None, None, 1).contains("为空")
        );
        assert!(
            access_reply(
                &whitelist,
                AccessCommand::Open,
                Some("222".into()),
                None,
                None,
                1
            )
            .contains("222")
        );
        assert!(whitelist.0.lock().unwrap().contains("222"));
    }

    #[test]
    fn storage_and_protocol_failures_have_distinct_private_notices() {
        use super::{ControlReport, LlmError, RunFailure, failed_turn_notice};
        use eve_control_api::{CommitState, GenerationKey, RunReport};
        use eve_session_api::{SessionError, SessionKey};
        for (failure, expected) in [
            (
                RunFailure::Execution(LlmError::ContextCompactionFailed(
                    eve_llm_api::ProviderFailure::http(403).unwrap(),
                )),
                "上下文压缩失败：模型服务拒绝了凭据或权限（HTTP 403）",
            ),
            (
                RunFailure::Execution(LlmError::Protocol("private response body".into())),
                "模型回复格式或工具调用未通过协议检查",
            ),
            (
                RunFailure::Commit(SessionError::Storage),
                "本轮已执行，但结果未能提交保存",
            ),
            (
                RunFailure::FailureRecord {
                    storage: SessionError::Storage,
                    execution: LlmError::Provider("private provider detail".into()),
                },
                "本轮失败记录未能保存",
            ),
            (
                RunFailure::Session(SessionError::Storage),
                "会话状态读取或保存失败",
            ),
        ] {
            let report = ControlReport {
                key: GenerationKey {
                    session: SessionKey::new("s", "u").unwrap(),
                    task_id: "t".into(),
                    controller_epoch: [0; 16],
                    generation: 1,
                },
                cancel_requested: false,
                run: RunReport {
                    turn_id: Some(1),
                    commit: CommitState::Pending,
                    text: None,
                    transcript: None,
                    started_tools: Some(1),
                    tool_results: vec![],
                    failure: Some(failure),
                },
            };
            let notice = failed_turn_notice(&report, None);
            assert!(notice.starts_with(expected), "{notice}");
            assert!(!notice.contains("private"));
            assert!(!notice.contains("本轮执行或状态保存未能完成"));
            if matches!(
                report.run.failure,
                Some(RunFailure::Execution(LlmError::ContextCompactionFailed(_)))
            ) {
                assert!(notice.contains("核对现有结果"));
                let mut clean = report.clone();
                clean.run.started_tools = Some(0);
                let notice = failed_turn_notice(&clean, None);
                assert!(notice.contains("分组权限"));
                assert!(!notice.contains("核对现有结果"));
                clean.run.failure = Some(RunFailure::FailureRecord {
                    storage: SessionError::Storage,
                    execution: LlmError::ContextCompactionFailed(
                        eve_llm_api::ProviderFailure::http(403).unwrap(),
                    ),
                });
                assert!(failed_turn_notice(&clean, None).contains("核对现有结果"));
            }
            assert!(notice.contains("已启动 1 次工具调用"));
            assert!(notice.contains("不再自动重试"));
        }
    }

    #[test]
    fn failure_notices_name_the_fixed_kind_and_status_only() {
        use super::{ProviderFailure, ProviderFailureKind as Kind, failure_reason};
        assert_eq!(
            failure_reason(ProviderFailure::http(402).unwrap()),
            "模型服务拒绝请求（HTTP 402），需要检查额度或单次输出预算"
        );
        assert_eq!(
            failure_reason(ProviderFailure::http(429).unwrap()),
            "模型服务限流（HTTP 429）"
        );
        assert_eq!(
            failure_reason(ProviderFailure::http(503).unwrap()),
            "模型服务内部出错（HTTP 503）"
        );
        assert_eq!(
            failure_reason(ProviderFailure::new(Kind::Truncated)),
            "模型回复达到单次输出上限，未完整生成"
        );
        assert_eq!(
            failure_reason(ProviderFailure::new(Kind::Network)),
            "无法连接模型服务"
        );
        assert_eq!(
            failure_reason(ProviderFailure::http(307).unwrap()),
            "模型服务未能完成回复（HTTP 307）"
        );
    }

    #[test]
    fn only_complete_unambiguous_task_commands_preempt_natural_judgment() {
        for text in [
            "/cancel",
            " /correct 中文\n/add 保留细节 ",
            "/new 新任务",
            "/cancel\n/new 新任务",
            "/train start",
            "/train stop",
        ] {
            assert!(preempts_natural(text, true), "{text}");
        }
        for text in [
            "/cancelxxx",
            "/cancel extra",
            "/cancel\n/cancel",
            "/correct",
            "/new a\n/new b",
            "/new a\n/correct b",
            "/correct a\n/unrelated",
            "/cancel\n未知",
            "/answer a",
            "/memory status",
            "/train status",
            "/train start extra",
        ] {
            assert!(!preempts_natural(text, true), "{text}");
        }
        assert!(!preempts_natural("/train start", false));
        assert!(!preempts_natural(&"/add a\n".repeat(17), true));
    }
}
