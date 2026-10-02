use crate::{
    QqBotConfig, QqBotStatus,
    state::{Ledger, Message, ReceiptState},
};
use eve_control_api::{
    CommitState, ControlEvent, ControlEventSink, ControlFuture, ControlInput, ControlReport,
    ControlService, GenerationKey,
};
use eve_llm_api::{LlmError, LlmFuture, TurnEvent, TurnEventKind};
use eve_message_api::{IncomingMessage, MessageFuture, MessageService, RouteOutcome, RouteReport};
use eve_plugin_api::{LogEntry, LogLevel, PluginContext, PluginError, PluginResult, TaskSignal};
use eve_session_api::{SessionInput, SessionKey};
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

struct Active {
    message: Message,
    key: GenerationKey,
    // 必须在提交时捕获；路由器可能已经替换控制服务中的最新代。
    wait: ControlFuture<'static, ControlReport>,
}
struct CommandMessage {
    message: Message,
    target: Option<GenerationKey>,
}
struct Routing {
    message: Message,
    target: GenerationKey,
    wait: MessageFuture<'static, RouteReport>,
}
struct Reply {
    message: Message,
    text: String,
    guard: ReplyGuard,
}
enum ReplyGuard {
    // 控制消息的确认仍可用于已取消/阻塞代，但不能用于已被替换的代。
    Current(GenerationKey),
    Completed(ControlEvent),
    NoTask,
}
impl ReplyGuard {
    fn accepts(&self, control: &dyn ControlService) -> bool {
        match self {
            Self::Current(key) => control
                .snapshot(&key.session)
                .is_ok_and(|s| s.is_some_and(|s| s.key == *key)),
            Self::Completed(event) => control.accepts(event),
            Self::NoTask => true,
        }
    }
}
fn explicit(text: &str) -> bool {
    text.lines().any(|line| line.trim_start().starts_with('/'))
}
fn mark_failed(ledger: &mut Ledger, ctx: &PluginContext, app: &str, id: &str) -> PluginResult<()> {
    let index = ledger.find(app, id).expect("inserted receipt");
    ledger.entries[index].state = ReceiptState::Failed;
    ledger.save(ctx)
}
async fn finish(stdin: &mut ChildStdin, id: &str) -> PluginResult<()> {
    write(stdin, json!({"type":"finish","version":1,"id":id})).await
}
async fn completed(
    active: &mut [Active],
) -> (usize, eve_control_api::ControlResult<ControlReport>) {
    poll_fn(|cx| {
        for (index, active) in active.iter_mut().enumerate() {
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
    let Services { control, messages } = services;
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
    let mut queue: VecDeque<Message> = VecDeque::new();
    let mut commands: VecDeque<CommandMessage> = VecDeque::new();
    let mut active: Vec<Active> = Vec::new();
    let mut controlled_sessions: BTreeMap<String, SessionKey> = BTreeMap::new();
    let mut routing: Option<Routing> = None;
    let mut replies: VecDeque<Reply> = VecDeque::new();
    let mut delivering: Option<Message> = None;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let result: PluginResult<()> = async {
        loop {
            if sink.is_closed() { break Ok(()); }
            // 所有通道取消/切换与写入在这个循环串行执行。路由 Future 由服务持有，
            // 在 route 完成前不外发任何排队结果，也不开普通任务。
            if routing.is_none() && let Some(command) = commands.pop_front() {
                let message = command.message;
                if !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    warn(&ctx, "receipt_limit");
                    finish(&mut stdin, &message.id).await?;
                    continue;
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
                    match messages.submit(incoming, sink.clone()) {
                        Ok(ticket) => routing = Some(Routing { message, target, wait: messages.wait(&ticket) }),
                        Err(_) => {
                            warn(&ctx, "message_submit_failed");
                            replies.push_back(Reply { message, text: "消息控制暂不可用或已达容量上限；当前任务保持不变。".into(), guard: ReplyGuard::Current(target) });
                        }
                    }
                } else {
                    replies.push_back(Reply { message, text: NO_TASK.into(), guard: ReplyGuard::NoTask });
                }
            }
            if routing.is_none() && commands.is_empty() && delivering.is_none()
                && let Some(reply) = replies.pop_front() {
                if !reply.guard.accepts(control.as_ref()) {
                    mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                    warn(&ctx, "stale_reply_suppressed");
                    finish(&mut stdin, &reply.message.id).await?;
                    continue;
                }
                let index = ledger.find(&config.app_id, &reply.message.id).expect("inserted receipt");
                ledger.entries[index].state = ReceiptState::ReplyPending;
                ledger.entries[index].reply = Some(reply.text.clone());
                ledger.save(&ctx)?;
                // 保存也是同步操作；再次检查后至 write 完成不准入任何控制动作。
                if !reply.guard.accepts(control.as_ref()) {
                    mark_failed(&mut ledger, &ctx, &config.app_id, &reply.message.id)?;
                    warn(&ctx, "stale_reply_suppressed");
                    finish(&mut stdin, &reply.message.id).await?;
                    continue;
                }
                write(&mut stdin, json!({"type":"reply","version":1,"id":reply.message.id,"text":reply.text})).await?;
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                delivering = Some(reply.message);
            }
            if routing.is_none() && commands.is_empty() && active.is_empty()
                && delivering.is_none() && replies.is_empty() && let Some(message) = queue.pop_front() {
                if !ledger.insert(&ctx, &config.app_id, message.clone())? {
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
                        let wait = control.wait(&key); active.push(Active { message, key, wait });
                    }
                    Err(_) => {
                        mark_failed(&mut ledger, &ctx, &config.app_id, &message.id)?;
                        status.send_modify(|s| s.failed += 1);
                        warn(&ctx, "control_submit_failed");
                        finish(&mut stdin, &message.id).await?;
                    }
                }
            }
            tokio::select! {
                biased;
                _ = signal.cancelled() => break Ok(()),
                _ = stop.changed() => break Ok(()),
                _ = tokio::time::sleep_until(deadline), if !status.borrow().ready || delivering.is_some() => {
                    break Err(failure("QQBot ready 或 delivery 等待超时"));
                }
                report = async { routing.as_mut().expect("routing message").wait.as_mut().await }, if routing.is_some() => {
                    let route = routing.take().expect("routing message");
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
                    let text = match report.outcome {
                        RouteOutcome::Replaced { generation, .. } => {
                            let wait = control.wait(&generation);
                            active.push(Active { message: route.message, key: generation, wait });
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
                    replies.push_back(Reply { message: route.message, text, guard: ReplyGuard::Current(route.target) });
                }
                (index, report) = completed(&mut active), if !active.is_empty() && routing.is_none() => {
                    let current = active.remove(index);
                    let report = report.map_err(|_| failure("QQBot 控制任务收尾失败"))?;
                    let guard = ReplyGuard::Completed(ControlEvent { key: report.key.clone(), event: TurnEvent {
                        turn_id: report.run.turn_id, kind: TurnEventKind::SessionSaved,
                    }});
                    if report.run.commit == CommitState::Completed && report.run.failure.is_none()
                        && let Some(text) = report.run.text.filter(|t| !t.trim().is_empty() && t.len() <= 32768) {
                        status.send_modify(|s| s.completed += 1);
                        replies.push_back(Reply { message: current.message, text, guard });
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
                        if !active.is_empty() || routing.is_some() || delivering.is_some() || !queue.is_empty()
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
                            let Some(message) = delivering.as_ref() else { warn(&ctx, "unexpected_delivery"); continue; };
                            if frame.get("id").and_then(Value::as_str) != Some(message.id.as_str()) {
                                warn(&ctx, "delivery_id_mismatch"); continue;
                            }
                            let Some(ok) = frame.get("ok").and_then(Value::as_bool) else { warn(&ctx, "invalid_delivery"); continue; };
                            let index = ledger.find(&config.app_id, &message.id).expect("inserted receipt");
                            ledger.entries[index].state = if ok { ReceiptState::Sent } else { ReceiptState::Failed };
                            ledger.save(&ctx)?;
                            status.send_modify(|s| { if ok { s.sent += 1; } else { s.failed += 1; } });
                            if !ok { warn(&ctx, "delivery_failed_no_retry"); }
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
                            let live = queue.iter().any(|m| m.id == message.id)
                                || commands.iter().any(|c| c.message.id == message.id)
                                || active.iter().any(|a| a.message.id == message.id)
                                || routing.as_ref().is_some_and(|r| r.message.id == message.id)
                                || replies.iter().any(|r| r.message.id == message.id)
                                || delivering.as_ref().is_some_and(|m| m.id == message.id);
                            if live { warn(&ctx, "duplicate_pending"); continue; }
                            let duplicate = ledger.find(&config.app_id, &message.id).is_some();
                            let pending = queue.len() + commands.len() + active.len() + replies.len()
                                + usize::from(routing.is_some()) + usize::from(delivering.is_some());
                            if duplicate || pending >= MAX_PENDING {
                                warn(&ctx, if duplicate { "duplicate_no_replay" } else { "queue_limit" });
                                finish(&mut stdin, &message.id).await?;
                            } else {
                                status.send_modify(|s| s.received += 1);
                                if explicit(&message.text) {
                                    let session = message.session_key(&config.app_id)?;
                                    let target = control.snapshot(&session)
                                        .map_err(|_| failure("QQBot 任务快照不可用"))?.map(|s| s.key);
                                    commands.push_back(CommandMessage { message, target });
                                } else {
                                    queue.push_back(message);
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
    if let Some(route) = routing
        && route.wait.await.is_err()
    {
        cleanup_error = Some(failure("QQBot 消息控制收尾失败"));
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
