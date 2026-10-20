use crate::{
    QqBotConfig, QqBotStatus, QqCommandHandler, QqCommandInput, QqFileDelivery, QqFileResult,
    Segmentation, commands,
    files::valid_path,
    observer::{CompletedInteraction, Observation},
    outreach::{
        OUTREACH_PLANNER, QQ_OUTREACH_JUDGE_TIMEOUT, QQ_OUTREACH_MAX_REPLY_PARTS, QqOutreach,
        QqOutreachMoment, QqOutreachResult, valid_text as valid_outreach_text,
    },
    state::{Ledger, Message, Part, PartState, Progress, ReceiptState, Segments, valid_message_id},
};
use eve_control_api::{
    CommitState, ControlEvent, ControlEventSink, ControlFuture, ControlInput, ControlPhase,
    ControlReport, ControlService, GenerationKey, RunFailure,
};
use eve_llm_api::{
    LlmError, LlmFuture, ProviderFailure, ProviderFailureKind, TurnEvent, TurnEventKind,
};
use eve_message_api::{IncomingMessage, MessageFuture, MessageService, RouteOutcome, RouteReport};
use eve_plugin_api::{LogEntry, LogLevel, PluginContext, PluginError, PluginResult, TaskSignal};
use eve_session_api::{SessionInput, SessionKey};
use eve_training_api::{TrainingCommand, TrainingService};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
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

fn failure(code: &str) -> PluginError {
    PluginError::Task(code.into())
}
fn warn(ctx: &PluginContext, code: &str) {
    if let Ok(entry) = LogEntry::new(LogLevel::Warn, "eve.qqbot", code) {
        let _ = ctx.log(entry);
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
fn progress_text(kind: &TurnEventKind) -> Option<String> {
    let TurnEventKind::ToolProgress {
        completed_calls,
        failed_calls,
        tool_names,
    } = kind
    else {
        return None;
    };
    let mut activities = std::collections::BTreeSet::new();
    for name in tool_names {
        activities.insert(match name.as_str() {
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
    Some(format!(
        "{}已返回结果，累计收到 {completed_calls} 次工具回执{}。正在继续核对与整理。",
        activities.into_iter().collect::<Vec<_>>().join("、"),
        if *failed_calls == 0 {
            String::new()
        } else {
            format!("（其中 {failed_calls} 次未成功）")
        }
    ))
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
    pub conversations: crate::QqConversations,
    pub participant_observer: Option<Arc<dyn crate::QqParticipantObserver>>,
    pub timing_diagnostics: bool,
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
fn outreach_result(frame: &Value, ok: bool) -> QqOutreachResult {
    if ok {
        QqOutreachResult::Sent {
            platform_message_id: frame
                .get("message_id")
                .and_then(Value::as_str)
                .filter(|id| valid_message_id(id))
                .map(str::to_string),
        }
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
) -> PluginResult<Option<(String, String)>> {
    for entry in ledger.entries.iter().rev() {
        if entry.app_id == app
            && entry.state == ReceiptState::Sent
            && entry.message.session_key(app)? == *session
        {
            return Ok(Some((
                entry.message.scope.clone(),
                entry.message.target_id.clone(),
            )));
        }
    }
    Ok(None)
}
/// 文件投递的路由只取自本通道已确认的回执：宿主给会话，通道自己找平台目标。
fn route_for(
    ledger: &Ledger,
    app: &str,
    session: &SessionKey,
) -> PluginResult<Option<(String, String)>> {
    session_route(ledger, app, session)
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
                if matches!(event.event.kind, TurnEventKind::ToolProgress { .. }) {
                    self.progress
                        .try_send(event)
                        .map_err(|_| LlmError::Cancelled)?;
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
}
impl Delivery {
    fn segmented(&self) -> bool {
        !self.pauses.is_empty()
    }
    fn awaiting(&self) -> bool {
        self.pause_until.is_none()
    }
}
fn segment_frame(ledger: &Ledger, index: usize, part: usize) -> Value {
    let entry = &ledger.entries[index];
    let reply = entry.reply.as_deref().expect("segmented receipt has reply");
    let parts = &entry.segments.as_ref().expect("segmented receipt").parts;
    json!({"type":"segment","version":1,"id":entry.message.id,"index":part,
        "count":parts.len(),"text":&reply[parts[part].start..parts[part].end]})
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
fn failed_turn_notice(report: &ControlReport) -> String {
    let error = match report.run.failure.as_ref() {
        Some(RunFailure::Execution(error)) => Some(error),
        Some(RunFailure::FailureRecord { execution, .. }) => Some(execution),
        _ => None,
    };
    let reason = match error {
        Some(error) if error.provider_failure().is_some() => {
            failure_reason(error.provider_failure().expect("checked failure"))
        }
        Some(LlmError::ProviderTimeout) => "模型服务响应超时".into(),
        Some(LlmError::Provider(_)) => "模型服务未能完成回复".into(),
        Some(LlmError::RoundLimit) => "本轮达到工具调用次数上限".into(),
        Some(LlmError::HistoryWindowExceeded) => {
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
    format!("{reason}。{effects}本轮已停止，不再自动重试；继续前需要核对现有结果。")
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
        conversations,
        participant_observer,
        timing_diagnostics,
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
            training.observe_user_message(&scope, &entry.message.id, &entry.message.text)?;
        }
    }
    let (closed, receiver) = watch::channel(false);
    let (progress_tx, mut progress_rx) = mpsc::channel(64);
    let sink = Arc::new(ChannelEvents {
        signal: signal.clone(),
        stop: stop.clone(),
        closed: receiver,
        timing_diagnostics,
        progress: progress_tx,
    });
    let mut command = Command::new(&config.node_program);
    command
        .arg(&config.bridge_script)
        .args(&config.bridge_args)
        .env_clear();
    for name in ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP"] {
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
    let mut progress_queue = VecDeque::new();
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
            while progress_queue.len() < 64 {
                match progress_rx.try_recv() {
                    Ok(event) => progress_queue.push_back(event),
                    Err(_) => break,
                }
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
                        warn(&ctx, "receipt_limit");
                        finish(&mut stdin, &message.id).await?;
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
                        queue.push_front(Queued { message, saved: true, ordinary: false });
                        continue;
                    }
                    let text = match action {
                        TrainingCommand::Stop => "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。".into(),
                        TrainingCommand::Status => if training.enabled(&scope)? { "当前会话：主动提问训练已开启。" } else { "当前会话：主动提问训练已关闭。" }.into(),
                        TrainingCommand::Stats => match training.expression_snapshot(&scope)? {
                            Some(s) => format!("本会话已学习 {} 条有效用户表达：字数中位数 {}，短消息 {}%，单段消息 {}%。{}当前要求始终优先；这是本地表达统计。", s.samples, s.median_chars, s.short_percent, s.single_paragraph_percent, if s.samples < 8 { "不足 8 条，暂不采用本会话统计。" } else { "" }),
                            None => "本会话暂无有效表达样本；训练开启后会从普通交流中学习。".into(),
                        },
                        TrainingCommand::Reset => { training.reset_expression(&scope)?; "已重置本会话的表达统计，原始聊天记录保留；旧消息不会再次计入，可继续从新消息学习。".into() },
                        TrainingCommand::Help => "训练命令：/train start 开始；/train stop 停止采集和主动训练；/train status 查看开关；/train stats 查看表达统计；/train reset 重置本会话统计。".into(),
                        TrainingCommand::Start => unreachable!(),
                    };
                    replies.push_back(Reply { message, text, guard: ReplyGuard::NoTask, interaction: None, invite: None });
                    continue;
                }
                if !command.saved && !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    warn(&ctx, "receipt_limit");
                    finish(&mut stdin, &message.id).await?;
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
                if let Some(target) = command.target {
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
                    match messages.submit(incoming, route_sink) {
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
            if delivering.is_none() && pushing.is_none() && sending_file.is_none() && progress_delivery.is_none()
                && progress_queue.front().is_some_and(|event: &ControlEvent|
                    active.iter().any(|a| a.key == event.key)
                    || !routing.iter().any(|r| r.target.session == event.key.session))
                && let Some(event) = progress_queue.pop_front() {
                // 只使用当前代绑定的原始平台路由；取消、插入控制和旧代事件不补发。
                let message = active.iter().find(|a| a.key == event.key).map(|a| a.message.clone());
                if let Some(message) = message
                    && !pending_control(&message, &routing, &commands)
                    && control.accepts(&event)
                    && control.snapshot(&event.key.session).is_ok_and(|s| s.is_some_and(|s| !s.cancel_requested))
                    && let Some(text) = progress_text(&event.event.kind) {
                    let index = ledger.find(&config.app_id, &message.id).expect("active receipt");
                    let entry = &mut ledger.entries[index];
                    if entry.state == ReceiptState::Processing && entry.progress.len() < 3 {
                        let part = entry.progress.len();
                        entry.progress.push(Progress { text: text.clone(), state: PartState::Sending });
                        ledger.save(&ctx)?;
                        // 同步落盘后再次核验；写出后未知结果不会自动补发。
                        if control.accepts(&event) && control.snapshot(&event.key.session).is_ok_and(|s| s.is_some_and(|s| !s.cancel_requested)) {
                            write(&mut stdin, json!({"type":"progress","version":1,"id":message.id,"index":part,"text":text})).await?;
                            progress_delivery = Some((message.id.clone(), part));
                            deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                        } else {
                            ledger.entries[index].progress[part].state = PartState::Failed;
                            ledger.save(&ctx)?;
                        }
                    }
                }
                continue;
            }
            if delivering.is_none() && pushing.is_none() && sending_file.is_none() && judging.is_none() && progress_delivery.is_none()
                && progress_queue.is_empty() && progress_rx.is_empty()
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
                        let progress_count = ledger.entries[ledger.find(&config.app_id, &reply.message.id).expect("reply receipt")].progress.len();
                        let mut policy = segmentation.policy;
                        policy.max_segments = policy.max_segments.min(crate::QQ_MAX_SEGMENTS - progress_count);
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
                    && ledger.entries[index].progress.len() + plan.as_ref().map_or(1, |plan| plan.segments.len()) < crate::QQ_MAX_SEGMENTS
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
                    json!({"type":"reply","version":1,"id":reply.message.id,"text":text})
                } else {
                    segment_frame(&ledger, index, 0)
                };
                write(&mut stdin, frame).await?;
                timing(timing_diagnostics, "reply_written", None);
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                delivering = Some(Delivery { reply, pauses, index: 0, pause_until: None, appendix });
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
            let next_ordinary = queue.iter().position(|queued| {
                if conversations.wakeup_claimed() { return false; }
                let Ok(session) = queued.message.session_key(&config.app_id) else { return false; };
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
                let message = queued.message;
                if !queued.saved && !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    warn(&ctx, "receipt_limit");
                    finish(&mut stdin, &message.id).await?;
                    continue;
                }
                let key = message.session_key(&config.app_id)?;
                // 先登记会话类型，本轮工具执行时即可读取。
                conversations.record(&key, &message.scope);
                let submitted = control.submit(ControlInput {
                    session: SessionInput { key, text: message.turn_text() },
                    task_id: format!("qq:{}", message.id),
                }, sink.clone());
                match submitted {
                    Ok(key) => {
                        timing(timing_diagnostics, "admitted", None);
                        controlled_sessions.insert(key.session.session_id.clone(), key.session.clone());
                        let wait = control.wait(&key); active.push(Active { message, key, ordinary: queued.ordinary, wait });
                    }
                    Err(eve_control_api::ControlError::Busy) => {
                        queue.push_front(Queued { message, saved: true, ordinary: queued.ordinary });
                    }
                    Err(_) => {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &message.id)?;
                        status.send_modify(|s| s.failed += 1);
                        warn(&ctx, "control_submit_failed");
                        finish(&mut stdin, &message.id).await?;
                    }
                }
                continue;
            }
            // 主动私聊：通道空闲时定期询问宿主；路由只取自已确认的私聊回执。
            let push_ready = push_interval.is_some() && pushing.is_none() && sending_file.is_none() && delivering.is_none() && progress_delivery.is_none()
                && judging.is_none() && replies.is_empty() && status.borrow().ready;
            let file_ready = file_poll.is_some() && files.is_some() && sending_file.is_none() && pushing.is_none()
                && delivering.is_none() && progress_delivery.is_none() && judging.is_none() && replies.is_empty() && status.borrow().ready;
            if push_ready && let (Some(hook), Some(interval)) = (&outreach, push_interval)
                && tokio::time::Instant::now() >= next_push {
                next_push = tokio::time::Instant::now() + interval;
                match hook.inner.next_push() {
                    Ok(Some(push)) => {
                        let target = match &push.session {
                            Some(session) if session.user_id == push.user_id => session_route(&ledger, &config.app_id, session)?,
                            Some(_) => None,
                            None => c2c_route(&ledger, &config.app_id, &push.user_id)?.map(|target| ("c2c".to_string(), target)),
                        };
                        match target.filter(|_| valid_message_id(&push.message.id) && valid_outreach_text(&push.message.text)) {
                            Some((scope, target)) => {
                                write(&mut stdin, json!({"type":"push","version":1,"id":push.message.id,
                                    "scope":scope,"target_id":target,"text":push.message.text})).await?;
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
            // 文件投递：通道空闲时询问宿主；路由只取自该会话已确认的回执，路径已由宿主校验。
            if file_ready && let (Some(files), Some(interval)) = (&files, file_poll)
                && tokio::time::Instant::now() >= next_push {
                next_push = tokio::time::Instant::now() + interval;
                match files.next_file() {
                    Ok(Some(file)) => {
                        let route = route_for(&ledger, &config.app_id, &file.session)?;
                        let name_ok = file.name.as_deref().is_none_or(crate::files::valid_name);
                        let caption_ok = file.caption.as_deref().is_none_or(crate::files::valid_caption);
                        match route.filter(|_| valid_message_id(&file.id) && valid_path(&file.path) && name_ok && caption_ok) {
                            Some((scope, target)) => {
                                write(&mut stdin, json!({"type":"file","version":1,"id":file.id,
                                    "scope":scope,"target_id":target,"path":file.path,
                                    "name":file.name,"caption":file.caption})).await?;
                                deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                                sending_file = Some(file.id);
                            }
                            None => report_file(files, &ctx, &file.id, QqFileResult::NotSent),
                        }
                    }
                    Ok(None) => {}
                    Err(_) => warn(&ctx, "file_delivery_unavailable"),
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
                Some(event) = progress_rx.recv(), if progress_queue.len() < 64 => { progress_queue.push_back(event); }
                invite = judged(&mut judging), if judging.is_some() => {
                    let (mut reply, _) = judging.take().expect("judging reply");
                    reply.invite = Some(invite);
                    replies.push_front(reply);
                }
                // 停顿结束时唤醒：每次循环先处理一条待路由命令，命令清空后才写下一段。
                _ = tokio::time::sleep_until(delivering.as_ref().and_then(|d| d.pause_until).unwrap_or(deadline)),
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
                        mark_failed(&mut ledger, &ctx, &config.app_id, &old.message.id)?;
                        finish(&mut stdin, &old.message.id).await?;
                    }
                    if route.cancelled.as_ref().is_some_and(|cancelled| *cancelled.borrow())
                        && !matches!(report.outcome, RouteOutcome::Replaced { .. }) {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &route.message.id)?;
                        finish(&mut stdin, &route.message.id).await?;
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
                            active.push(Active { message: route.message, key: generation, ordinary: false, wait });
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
                        replies.push_back(Reply { message: current.message,
                            text: failed_turn_notice(&report), guard: ReplyGuard::Current(report.key),
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
                        Some("delivery") if frame.get("progress").and_then(Value::as_bool) == Some(true) => {
                            let Some((id, part)) = progress_delivery.as_ref() else { warn(&ctx, "unexpected_progress_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(id.as_str()) || frame.get("index").and_then(Value::as_u64) != Some(*part as u64) {
                                warn(&ctx, "progress_delivery_mismatch"); continue;
                            }
                            let Some(ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let index = ledger.find(&config.app_id, id).expect("progress receipt");
                            ledger.entries[index].progress[*part].state = if ok { PartState::Sent } else { PartState::Failed };
                            ledger.save(&ctx)?;
                            if !ok { warn(&ctx, "progress_failed_no_retry"); }
                            progress_delivery = None;
                        }
                        Some("delivery") if frame.get("file").and_then(Value::as_bool) == Some(true) => {
                            let (Some(files), Some(current)) = (&files, sending_file.as_deref()) else { warn(&ctx, "unexpected_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(current) { warn(&ctx, "delivery_id_mismatch"); continue; }
                            let Some(ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            if !ok { warn(&ctx, "file_delivery_failed_no_retry"); }
                            report_file(files, &ctx, current, file_result(&frame, ok));
                            sending_file = None;
                        }
                        Some("delivery") if frame.get("push").and_then(Value::as_bool) == Some(true) => {
                            let (Some(hook), Some(current)) = (&outreach, pushing.as_deref()) else { warn(&ctx, "unexpected_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(current) { warn(&ctx, "delivery_id_mismatch"); continue; }
                            let Some(ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            if !ok { warn(&ctx, "outreach_push_failed_no_retry"); }
                            report(hook, &ctx, current, outreach_result(&frame, ok));
                            pushing = None;
                        }
                        Some("delivery") => {
                            let Some(delivery) = delivering.as_mut().filter(|d| d.awaiting()) else { warn(&ctx, "unexpected_delivery"); continue; };
                            let message = &delivery.reply.message;
                            if frame.get("id").and_then(Value::as_str) != Some(message.id.as_str()) {
                                warn(&ctx, "delivery_id_mismatch"); continue;
                            }
                            let Some(ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let index = ledger.find(&config.app_id, &message.id).expect("inserted receipt");
                            if delivery.segmented() {
                                let part = delivery.index;
                                if frame.get("index").and_then(Value::as_u64) != u64::try_from(part).ok() {
                                    warn(&ctx, "delivery_index_mismatch"); continue;
                                }
                                let segments = ledger.entries[index].segments.as_mut().expect("segmented receipt");
                                let last = part + 1 == segments.parts.len();
                                segments.parts[part].state = if ok { PartState::Sent } else { PartState::Failed };
                                if !ok {
                                    segments.skip_rest();
                                    ledger.entries[index].state = ReceiptState::Failed;
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
                                ledger.entries[index].state = if ok { ReceiptState::Sent } else { ReceiptState::Failed };
                                ledger.save(&ctx)?;
                            }
                            let reply = &delivery.reply;
                            let message = &reply.message;
                            status.send_modify(|s| { if ok { s.sent += 1; } else { s.failed += 1; } });
                            if !ok { warn(&ctx, "delivery_failed_no_retry"); }
                            if ok { timing(timing_diagnostics, "delivery_confirmed", None); }
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
                            delivering = None;
                        }
                        Some("message") => {
                            let mut payload = frame.clone();
                            let sender_name = payload.get("sender_name").and_then(Value::as_str).map(str::to_owned);
                            if let Some(object) = payload.as_object_mut() {
                                object.remove("type"); object.remove("version");
                                object.remove("sender_name");
                                if object.keys().any(|key| !["id", "scope", "target_id", "user_id", "text", "attachments", "quoted"].contains(&key.as_str())) {
                                    warn(&ctx, "ignored_message_fields");
                                }
                                object.retain(|key, _| ["id", "scope", "target_id", "user_id", "text", "attachments", "quoted"].contains(&key.as_str()));
                            }
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
                            let duplicate = ledger.find(&config.app_id, &message.id).is_some();
                            let pending = queue.len() + commands.len() + active.len() + replies.len()
                                + routing.len() + usize::from(delivering.is_some()) + usize::from(judging.is_some());
                            if duplicate || pending >= MAX_PENDING {
                                warn(&ctx, if duplicate { "duplicate_no_replay" } else { "queue_limit" });
                                finish(&mut stdin, &message.id).await?;
                            } else {
                                status.send_modify(|s| s.received += 1);
                                timing(timing_diagnostics, "message_received", None);
                                conversations.record_message(&config.app_id, &message)?;
                                if private_text.is_none() && let Some(observer) = &participant_observer {
                                    let session = message.session_key(&config.app_id)?;
                                    if let Err(error) = observer.observe(&session, &message.conversation_key(&config.app_id)?, &message.text, sender_name.as_deref()) {
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
                                            warn(&ctx, "receipt_limit");
                                            finish(&mut stdin, &message.id).await?;
                                            continue;
                                        }
                                        commands.push_back(CommandMessage { message, private_text: None, target, natural: true, superseded: false, saved: true });
                                    } else {
                                        let ordinary = message.attachments.is_empty();
                                        queue.push_back(Queued { message, saved: false, ordinary });
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
