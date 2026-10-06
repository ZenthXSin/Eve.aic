use crate::{
    QqBotConfig, QqBotStatus, QqCommandHandler, QqCommandInput, Segmentation, commands,
    observer::{CompletedInteraction, Observation},
    state::{Ledger, Message, Part, PartState, ReceiptState, Segments},
};
use eve_control_api::{
    CommitState, ControlEvent, ControlEventSink, ControlFuture, ControlInput, ControlPhase,
    ControlReport, ControlService, GenerationKey,
};
use eve_llm_api::{LlmError, LlmFuture, TurnEvent, TurnEventKind};
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
    sync::watch,
};

fn failure(code: &str) -> PluginError {
    PluginError::Task(code.into())
}
fn warn(ctx: &PluginContext, code: &str) {
    if let Ok(entry) = LogEntry::new(LogLevel::Warn, "eve.qqbot", code) {
        let _ = ctx.log(entry);
    }
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
}
struct ChannelEvents {
    signal: Arc<dyn TaskSignal>,
    stop: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
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
    fn emit(&self, _: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            if self.is_closed() {
                Err(LlmError::Cancelled)
            } else {
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
}
/// 一次只投递一条回复。分段时逐段写出并等待回执；段间停顿结束、
/// 路由与命令都收尾后，重新核对代际再写下一段。
struct Delivery {
    reply: Reply,
    pauses: Vec<u64>,
    index: usize,
    pause_until: Option<tokio::time::Instant>,
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
    } = services;
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
    let sink = Arc::new(ChannelEvents {
        signal: signal.clone(),
        stop: stop.clone(),
        closed: receiver,
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
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let result: PluginResult<()> = async {
        loop {
            if sink.is_closed() { break Ok(()); }
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
                    replies.push_back(Reply { message, text, guard: ReplyGuard::NoTask, interaction: None });
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
                        message_id: &message.id, session: &session, text: &message.text,
                    }) {
                        Ok(Some(text)) => {
                            replies.push_back(Reply { message, text, guard: ReplyGuard::Command, interaction: None });
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
                        text: message.text.clone(),
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
                            replies.push_back(Reply { message, text: "消息控制暂不可用或已达容量上限；当前任务保持不变。".into(), guard: ReplyGuard::Current(target), interaction: None });
                        }
                    }
                } else {
                    replies.push_back(Reply { message, text: NO_TASK.into(), guard: ReplyGuard::NoTask, interaction: None });
                }
                continue;
            }
            if delivering.is_none()
                && let Some(index) = replies.iter().position(|reply| !pending_control(&reply.message, &routing, &commands)) {
                let reply = replies.remove(index).expect("queued reply");
                if !reply.guard.accepts(control.as_ref()) {
                    mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                    warn(&ctx, "stale_reply_suppressed");
                    finish(&mut stdin, &reply.message.id).await?;
                    continue;
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
                        match eve_segment_api::plan_with_preference(
                            segmentation.planner.as_ref(), &reply.text, &segmentation.policy, &preference) {
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
                ledger.entries[index].segments = plan.map(|plan| Segments {
                    planner: plan.planner,
                    parts: plan.segments.iter().enumerate().map(|(i, s)| Part {
                        start: s.start, end: s.end,
                        state: if i == 0 { PartState::Sending } else { PartState::Pending },
                    }).collect(),
                });
                ledger.save(&ctx)?;
                // 保存也是同步操作；再次检查后至 write 完成不准入任何控制动作。
                if !reply.guard.accepts(control.as_ref()) {
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
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                delivering = Some(Delivery { reply, pauses, index: 0, pause_until: None });
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
                if !natural_message_judgement {
                    return routing.is_empty() && commands.is_empty() && active.is_empty()
                        && delivering.is_none() && replies.is_empty();
                }
                !pending_control(&queued.message, &routing, &commands)
                    && !active.iter().any(|a| same_session(&queued.message, &a.message))
                    && !replies.iter().any(|r| same_session(&queued.message, &r.message))
                    && !delivering.as_ref().is_some_and(|d| same_session(&queued.message, &d.reply.message))
            });
            if let Some(index) = next_ordinary {
                let queued = queue.remove(index).expect("queued ordinary message");
                let message = queued.message;
                if !queued.saved && !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    warn(&ctx, "receipt_limit");
                    finish(&mut stdin, &message.id).await?;
                    continue;
                }
                let submitted = control.submit(ControlInput {
                    session: SessionInput { key: message.session_key(&config.app_id)?, text: message.text.clone() },
                    task_id: format!("qq:{}", message.id),
                }, sink.clone());
                match submitted {
                    Ok(key) => {
                        controlled_sessions.insert(key.session.session_id.clone(), key.session.clone());
                        let wait = control.wait(&key); active.push(Active { message, key, ordinary: queued.ordinary, wait });
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
            let eligible: Vec<bool> = active.iter().map(|a| !pending_control(&a.message, &routing, &commands)).collect();
            tokio::select! {
                biased;
                _ = signal.cancelled() => break Ok(()),
                _ = stop.changed() => break Ok(()),
                _ = tokio::time::sleep_until(deadline), if !status.borrow().ready || delivering.as_ref().is_some_and(Delivery::awaiting) => {
                    break Err(failure("QQBot ready 或 delivery 等待超时"));
                }
                // 停顿结束时唤醒：每次循环先处理一条待路由命令，命令清空后才写下一段。
                _ = tokio::time::sleep_until(delivering.as_ref().and_then(|d| d.pause_until).unwrap_or(deadline)),
                    if delivering.as_ref().is_some_and(|d| !d.awaiting() && !pending_control(&d.reply.message, &routing, &commands)) => {}
                (index, report) = routed(&mut routing), if !routing.is_empty() => {
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
                    replies.push_back(Reply { message: route.message, text, guard: ReplyGuard::Current(route.target), interaction: None });
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
                        replies.push_back(Reply { message: current.message, text: text.clone(), guard, interaction });
                    } else {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &current.message.id)?;
                        if !report.cancel_requested {
                            status.send_modify(|s| s.failed += 1);
                            warn(&ctx, "turn_failed");
                        }
                        finish(&mut stdin, &current.message.id).await?;
                    }
                }
                frame = frames.next() => {
                    let Some(frame) = frame? else {
                        if !active.is_empty() || !routing.is_empty() || delivering.is_some() || !queue.is_empty()
                            || !commands.is_empty() || !replies.is_empty() || !status.borrow().ready {
                            break Err(failure("QQBot 在未完成交互时断开；保留状态"));
                        }
                        break Ok(());
                    };
                    if frame.get("version").and_then(Value::as_u64) != Some(1) { warn(&ctx, "protocol_version"); }
                    match frame.get("type").and_then(Value::as_str) {
                        Some("ready") => { status.send_modify(|s| s.ready = true); }
                        Some("warning") => {
                            let code = match frame.get("code").and_then(Value::as_str) {
                                Some("invalid_route_or_text") => "bridge_invalid_route_or_text",
                                Some("unsupported_message") => "bridge_unsupported_message",
                                Some("pending_limit") => "bridge_pending_limit",
                                Some("sdk_error") => "bridge_sdk_error",
                                _ => "bridge_warning",
                            };
                            warn(&ctx, code);
                        }
                        Some("fatal") => break Err(failure("QQBot SDK 启动或连接失败")),
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
                            if ok && let Some(interaction) = &reply.interaction
                                && let Some(observation) = &observation
                                && let Err(error) = interaction.observe(observation.observer.as_ref(), &config.app_id, message, &reply.text) {
                                warn(&ctx, "interaction_observer_failed_no_retry");
                                return Err(error);
                            }
                            delivering = None;
                        }
                        Some("message") => {
                            let mut payload = frame.clone();
                            if let Some(object) = payload.as_object_mut() {
                                object.remove("type"); object.remove("version");
                                if object.keys().any(|key| !["id", "scope", "target_id", "user_id", "text"].contains(&key.as_str())) {
                                    warn(&ctx, "ignored_message_fields");
                                }
                                object.retain(|key, _| ["id", "scope", "target_id", "user_id", "text"].contains(&key.as_str()));
                            }
                            let Ok(message) = serde_json::from_value::<Message>(payload) else { warn(&ctx, "invalid_message"); continue; };
                            if !message.valid() { warn(&ctx, "invalid_route_or_text"); continue; }
                            let live = queue.iter().any(|m| m.message.id == message.id)
                                || commands.iter().any(|c| c.message.id == message.id)
                                || active.iter().any(|a| a.message.id == message.id)
                                || routing.iter().any(|r| r.message.id == message.id)
                                || replies.iter().any(|r| r.message.id == message.id)
                                || delivering.as_ref().is_some_and(|d| d.reply.message.id == message.id);
                            if live { warn(&ctx, "duplicate_pending"); continue; }
                            let duplicate = ledger.find(&config.app_id, &message.id).is_some();
                            let pending = queue.len() + commands.len() + active.len() + replies.len()
                                + routing.len() + usize::from(delivering.is_some());
                            if duplicate || pending >= MAX_PENDING {
                                warn(&ctx, if duplicate { "duplicate_no_replay" } else { "queue_limit" });
                                finish(&mut stdin, &message.id).await?;
                            } else {
                                status.send_modify(|s| s.received += 1);
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
                                    commands.push_back(CommandMessage { message, target, natural: false, superseded: false, saved: false });
                                } else {
                                    // 按普通输入准入时的持久开关采集，不能延后到模型执行。
                                    // 启停/重置可先于排队任务执行；旧输入必须已有去重凭据。
                                    if let Some(training) = &training {
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
                                        commands.push_back(CommandMessage { message, target, natural: true, superseded: false, saved: true });
                                    } else {
                                        queue.push_back(Queued { message, saved: false, ordinary: true });
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
