use crate::{
    QqBotConfig, QqBotStatus,
    state::{Ledger, Message, ReceiptState},
};
use eve_control_api::{
    CommitState, ControlInput, ControlService, DiscardControlEvents, GenerationKey,
};
use eve_plugin_api::{LogEntry, LogLevel, PluginContext, PluginError, PluginResult, TaskSignal};
use eve_session_api::SessionInput;
use serde_json::{Value, json};
use std::{collections::VecDeque, process::Stdio, sync::Arc, time::Duration};
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
pub(crate) async fn run(
    config: Arc<QqBotConfig>,
    ctx: PluginContext,
    control: Arc<dyn ControlService>,
    mut ledger: Ledger,
    signal: Arc<dyn TaskSignal>,
    status: watch::Sender<QqBotStatus>,
    mut stop: watch::Receiver<bool>,
) -> PluginResult<()> {
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
    let mut active: Option<(Message, GenerationKey)> = None;
    let mut delivering: Option<Message> = None;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let result: PluginResult<()> = async {
        loop {
            if active.is_none() && delivering.is_none() && let Some(message) = queue.pop_front() {
                if !ledger.insert(&ctx, &config.app_id, message.clone())? {
                    warn(&ctx, "receipt_limit"); write(&mut stdin, json!({"type":"finish","version":1,"id":message.id})).await?;
                    continue;
                }
                let submitted = control.submit(ControlInput {
                    session: SessionInput { key: message.session_key(&config.app_id)?, text: message.text.clone() },
                    task_id: format!("qq:{}", message.id),
                }, Arc::new(DiscardControlEvents));
                match submitted {
                    Ok(key) => { active = Some((message, key)); }
                    Err(_) => {
                        let index = ledger.find(&config.app_id, &message.id).expect("inserted receipt");
                        ledger.entries[index].state = ReceiptState::Failed; ledger.save(&ctx)?;
                        status.send_modify(|s| s.failed += 1); warn(&ctx, "control_submit_failed");
                        write(&mut stdin, json!({"type":"finish","version":1,"id":message.id})).await?;
                    }
                }
            }
            let wait = active.as_ref().map(|(_, key)| control.wait(key));
            tokio::select! {
                biased;
                _ = signal.cancelled() => break Ok(()),
                _ = stop.changed() => break Ok(()),
                _ = tokio::time::sleep_until(deadline), if !status.borrow().ready || delivering.is_some() => {
                    break Err(failure("QQBot ready 或 delivery 等待超时"));
                }
                report = async { wait.expect("active generation").await }, if active.is_some() => {
                    let (message, _) = active.take().expect("active generation");
                    let index = ledger.find(&config.app_id, &message.id).expect("inserted receipt");
                    let report = report.map_err(|_| failure("QQBot 控制任务收尾失败"))?;
                    if report.run.commit == CommitState::Completed && report.run.failure.is_none()
                        && let Some(text) = report.run.text.filter(|t| !t.trim().is_empty() && t.len() <= 32768) {
                        ledger.entries[index].state = ReceiptState::ReplyPending;
                        ledger.entries[index].reply = Some(text.clone()); ledger.save(&ctx)?;
                        status.send_modify(|s| s.completed += 1);
                        write(&mut stdin, json!({"type":"reply","version":1,"id":message.id,"text":text})).await?;
                        deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                        delivering = Some(message);
                    } else {
                        ledger.entries[index].state = ReceiptState::Failed; ledger.save(&ctx)?;
                        status.send_modify(|s| s.failed += 1); warn(&ctx, "turn_failed");
                        write(&mut stdin, json!({"type":"finish","version":1,"id":message.id})).await?;
                    }
                }
                frame = frames.next() => {
                    let Some(frame) = frame? else {
                        if active.is_some() || delivering.is_some() || !queue.is_empty() || !status.borrow().ready {
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
                                || active.as_ref().is_some_and(|(m, _)| m.id == message.id)
                                || delivering.as_ref().is_some_and(|m| m.id == message.id);
                            if live { warn(&ctx, "duplicate_pending"); continue; }
                            let duplicate = ledger.find(&config.app_id, &message.id).is_some();
                            if duplicate || queue.len() >= 16 {
                                warn(&ctx, if duplicate { "duplicate_no_replay" } else { "queue_limit" });
                                write(&mut stdin, json!({"type":"finish","version":1,"id":message.id})).await?;
                            } else {
                                status.send_modify(|s| s.received += 1); queue.push_back(message);
                            }
                        }
                        _ => warn(&ctx, "unknown_or_malformed_frame"),
                    }
                }
            }
        }
    }.await;
    let cancelled = if let Some((_, key)) = active {
        let requested = control.cancel(&key);
        let settled = control.wait(&key).await;
        requested.map_err(|_| failure("QQBot 取消失败")).and(
            settled
                .map(|_| ())
                .map_err(|_| failure("QQBot 取消收尾失败")),
        )
    } else {
        Ok(())
    };
    let _ = write(&mut stdin, json!({"type":"stop","version":1})).await;
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result.and(cancelled)
}
