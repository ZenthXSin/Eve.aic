//! 同一可信会话的轮内引导和主动交互；队列有界，结束后不接收、不重放。
use eve_llm_api::*;
use eve_plugin_api::{PluginError, PluginResult};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{Notify, oneshot};

type Key = (String, String);
fn key(scope: &ContextScope) -> Key {
    (scope.session_id.clone(), scope.user_id.clone())
}
fn unavailable() -> LlmError {
    LlmError::Context("实时交互队列不可用".into())
}

#[derive(Default)]
struct Input {
    accepting: bool,
    messages: Vec<(String, Option<String>)>,
    bytes: usize,
    accepted: usize,
    replies: BTreeMap<String, InputReplyHandoff>,
    reply_updates: BTreeSet<String>,
    reply_bytes: usize,
}
struct Turn {
    id: u64,
    input: Mutex<Input>,
    changed: Notify,
}

/// 发出时冻结的原轮句柄；工具结束后移除 ForegroundInput 也不能丢失终态归属。
pub(crate) struct ClaimedInputReply {
    turn: Weak<Turn>,
    input_message_id: String,
    event_id: String,
}
impl ClaimedInputReply {
    pub(crate) fn complete(self, delivery: InputReplyDelivery) -> PluginResult<()> {
        let Some(turn) = self.turn.upgrade() else {
            return Ok(());
        };
        let mut input = turn
            .input
            .lock()
            .map_err(|_| PluginError::Task("交流交接队列锁不可用".into()))?;
        if !input.accepting {
            return Ok(());
        }
        let handoff = input
            .replies
            .get_mut(&self.input_message_id)
            .ok_or_else(|| PluginError::Task("交流交接事实缺失".into()))?;
        if handoff.event_id != self.event_id || handoff.delivery != InputReplyDelivery::Sending {
            return Err(PluginError::Task("交流交接身份或终态不一致".into()));
        }
        if delivery == InputReplyDelivery::Sending {
            return Err(PluginError::Task("交流交接尚无终态".into()));
        }
        let updated = InputReplyHandoff {
            delivery,
            ..handoff.clone()
        };
        updated
            .validate()
            .map_err(|_| PluginError::Task("交流交接结果无效".into()))?;
        *handoff = updated;
        input.reply_updates.insert(self.input_message_id);
        // 不通知 wait_for_input：这只是同一输入的投递更新，不是用户又说了话。
        Ok(())
    }
}
pub(crate) struct LiveSend {
    pub scope: ContextScope,
    pub turn_id: u64,
    pub text: String,
    pub image: Option<String>,
    pub needs_answer: bool,
    pub call_id: String,
    context: ToolExecutionContext,
    turn: Weak<Turn>,
    result: oneshot::Sender<LiveDelivery>,
}
#[derive(Clone, Debug)]
pub(crate) enum LiveDelivery {
    Sent(String),
    Unknown,
    NotSent,
    NotSentReason(&'static str),
    Stale,
}
impl LiveDelivery {
    /// 只交给模型固定原因，不透传 SDK 原始错误或改变未知投递语义。
    pub(crate) fn not_sent(reason: Option<&str>) -> Self {
        let reason = match reason {
            Some("passive_window_elapsed") => "passive_window_elapsed",
            Some("passive_window_missing") => "passive_window_missing",
            Some("passive_quota_exhausted") => "passive_quota_exhausted",
            Some("reply_anchor_mismatch") => "reply_anchor_mismatch",
            Some("duplicate_text") => "duplicate_text",
            Some("progress_limit") => "progress_limit",
            Some("progress_rate_limit") => "progress_rate_limit",
            Some("reply_limit") => "reply_limit",
            Some("outbound_limit") => "outbound_limit",
            Some("http") => "http",
            Some("dns") => "dns",
            Some("tls") => "tls",
            _ => return Self::NotSent,
        };
        Self::NotSentReason(reason)
    }
}
impl LiveSend {
    pub fn cancelled(&self) -> bool {
        self.result.is_closed()
    }
    /// 从决定到真正写出之间，新输入和取消都可能让这句话失效。
    pub fn current(&self) -> bool {
        !self.cancelled()
            && !self.context.is_cancel_requested()
            && self.context.check_review_evidence().is_ok()
            && self.turn.upgrade().is_some_and(|turn| {
                turn.input
                    .lock()
                    .is_ok_and(|input| input.accepting && input.messages.is_empty())
            })
    }
    pub fn complete(self, outcome: LiveDelivery) {
        let _ = self.result.send(outcome);
    }
}

#[derive(Default)]
pub struct LiveDialogue {
    turns: Mutex<BTreeMap<Key, Weak<Turn>>>,
    outgoing: Mutex<VecDeque<LiveSend>>,
}
impl LiveDialogue {
    /// 与主轮消费争用同一个输入锁。主轮已取走输入时，前台不能再领取其即时交流。
    /// 先持久保存 sending，再让同消息批次看见交接；失败不吞输入、不宣称接手。
    /// persist 在锁内复核活动身份；返回 false 表示资格已撤销且没有写入发送状态。
    pub(crate) fn claim_input_reply(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        input_message_id: &str,
        event_id: &str,
        reply_text: &str,
        persist: impl FnOnce() -> PluginResult<bool>,
    ) -> PluginResult<Option<ClaimedInputReply>> {
        let turn = self
            .turns
            .lock()
            .map_err(|_| PluginError::Task("交互队列锁不可用".into()))?
            .get(&key(scope))
            .and_then(Weak::upgrade);
        let Some(turn) = turn.filter(|turn| turn.id == turn_id) else {
            return Ok(None);
        };
        let mut input = turn
            .input
            .lock()
            .map_err(|_| PluginError::Task("交流交接队列锁不可用".into()))?;
        let pending = input
            .messages
            .last()
            .filter(|(_, anchor)| anchor.as_deref() == Some(input_message_id));
        let Some((input_text, _)) = pending else {
            return Ok(None);
        };
        if !input.accepting
            || input.replies.contains_key(input_message_id)
            || input.replies.len() >= 64
        {
            return Ok(None);
        }
        let handoff = InputReplyHandoff {
            input_message_id: input_message_id.into(),
            input_text: input_text.clone(),
            event_id: event_id.into(),
            reply_text: reply_text.into(),
            delivery: InputReplyDelivery::Sending,
        };
        handoff
            .validate()
            .map_err(|_| PluginError::Task("交流交接事实无效".into()))?;
        let bytes = serde_json::to_vec(&handoff)
            .map_err(|_| PluginError::Task("交流交接事实无法序列化".into()))?
            .len();
        if input.reply_bytes.saturating_add(bytes) > 256 * 1024 {
            return Ok(None);
        }
        if !persist()? {
            return Ok(None);
        }
        input.reply_bytes += bytes;
        input.replies.insert(input_message_id.into(), handoff);
        input.reply_updates.insert(input_message_id.into());
        Ok(Some(ClaimedInputReply {
            turn: Arc::downgrade(&turn),
            input_message_id: input_message_id.into(),
            event_id: event_id.into(),
        }))
    }
    /// 持有入口锁时先保存平台回执，成功后才让运行时看见正文。
    #[cfg(test)]
    pub(crate) fn steer(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        text: String,
        persist: impl FnOnce() -> PluginResult<()>,
    ) -> PluginResult<bool> {
        self.steer_anchored(scope, turn_id, text, None, persist)
    }
    pub(crate) fn steer_anchored(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        text: String,
        anchor: Option<String>,
        persist: impl FnOnce() -> PluginResult<()>,
    ) -> PluginResult<bool> {
        let turn = self
            .turns
            .lock()
            .map_err(|_| PluginError::Task("交互队列锁不可用".into()))?
            .get(&key(scope))
            .and_then(Weak::upgrade);
        let Some(turn) = turn.filter(|turn| turn.id == turn_id) else {
            return Ok(false);
        };
        let mut input = turn
            .input
            .lock()
            .map_err(|_| PluginError::Task("引导队列锁不可用".into()))?;
        if !input.accepting || input.accepted >= 64 || input.bytes + text.len() > 256 * 1024 {
            return Ok(false);
        }
        persist()?;
        input.bytes += text.len();
        input.accepted += 1;
        input.messages.push((text, anchor));
        turn.changed.notify_waiters();
        Ok(true)
    }
    pub(crate) fn next(&self) -> Option<LiveSend> {
        self.outgoing.lock().ok()?.pop_front()
    }
    /// 宿主活动提示让出同会话已经排队的明确发言，不领取或完成那次发送。
    pub(crate) fn has_outgoing(&self, session: &eve_session_api::SessionKey) -> bool {
        self.outgoing.lock().map_or(true, |pending| {
            pending.iter().any(|send| {
                send.scope.session_id == session.session_id && send.scope.user_id == session.user_id
            })
        })
    }
    /// 只观察同一轮尚未被运行时消费的新输入，不取走正文或推进审核锚点。
    pub(crate) fn pending_anchor(&self, scope: &ContextScope, turn_id: u64) -> Option<String> {
        let turn = self.turns.lock().ok()?.get(&key(scope))?.upgrade()?;
        if turn.id != turn_id {
            return None;
        }
        let input = turn.input.lock().ok()?;
        if !input.accepting {
            return None;
        }
        input.messages.last()?.1.clone()
    }
    pub fn send_tool(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(SendMessage(self.clone()))
    }
    pub fn wait_tool(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(WaitForInput(self.clone()))
    }
    /// 图片也是模型的明确表达决定；不附加到内部 final，不等待自动收尾。
    pub fn send_image(
        &self,
        path: std::path::PathBuf,
        description: String,
        context: ToolExecutionContext,
    ) -> ToolFuture<'_> {
        Box::pin(async move {
            let rejected = || ToolExecutionError::Rejected("当前没有可投递的图片对话轮次".into());
            if description.trim().is_empty()
                || description.len() > 32768
                || description
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err(rejected());
            }
            let description = description.trim().to_owned();
            let path = path
                .to_str()
                .filter(|p| !p.is_empty() && p.len() <= 4096 && !p.chars().any(char::is_control))
                .ok_or_else(rejected)?
                .to_owned();
            let scope = context.scope().cloned().ok_or_else(rejected)?;
            let turn_id = context.turn_id().ok_or_else(rejected)?;
            let call_id = context.call_id().ok_or_else(rejected)?.to_owned();
            let turn = self
                .turns
                .lock()
                .map_err(|_| rejected())?
                .get(&key(&scope))
                .and_then(Weak::upgrade)
                .filter(|t| t.id == turn_id)
                .ok_or_else(rejected)?;
            let session_id = scope.session_id.clone();
            if context.is_cancel_requested()
                || context.check_review_evidence().is_err()
                || !turn
                    .input
                    .lock()
                    .is_ok_and(|i| i.accepting && i.messages.is_empty())
            {
                return Ok(
                    json!({"sent":false,"message_id":null,"session_id":session_id,"delivery_status":"stale"}),
                );
            }
            let (sender, receiver) = oneshot::channel();
            {
                let mut queue = self.outgoing.lock().map_err(|_| rejected())?;
                if queue.len() >= 64 {
                    return Err(rejected());
                }
                queue.push_back(LiveSend {
                    scope,
                    turn_id,
                    text: description,
                    image: Some(path),
                    needs_answer: false,
                    call_id: format!(
                        "speech-{}",
                        ring::digest::digest(&ring::digest::SHA256, call_id.as_bytes())
                            .as_ref()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    ),
                    context,
                    turn: Arc::downgrade(&turn),
                    result: sender,
                });
            }
            let outcome = receiver
                .await
                .map_err(|_| ToolExecutionError::Failed("图片投递结果未知，不要重发".into()))?;
            Ok(delivery_json(outcome, &session_id))
        })
    }
}
impl TurnSteeringSource for LiveDialogue {
    fn open(&self, scope: &ContextScope, turn_id: u64) -> Result<Arc<dyn TurnSteering>, LlmError> {
        let mut turns = self.turns.lock().map_err(|_| unavailable())?;
        turns.retain(|_, turn| turn.strong_count() > 0);
        if turns.get(&key(scope)).and_then(Weak::upgrade).is_some() {
            return Err(unavailable());
        }
        let turn = Arc::new(Turn {
            id: turn_id,
            input: Mutex::new(Input {
                accepting: true,
                ..Input::default()
            }),
            changed: Notify::new(),
        });
        turns.insert(key(scope), Arc::downgrade(&turn));
        Ok(turn)
    }
}
impl TurnSteering for Turn {
    fn take(&self, finish: bool) -> Result<Vec<String>, LlmError> {
        self.take_batch(finish).map(|batch| batch.messages)
    }
    fn take_batch(&self, finish: bool) -> Result<SteeringBatch, LlmError> {
        self.take_update(finish).map(|update| update.batch)
    }
    fn take_update(&self, finish: bool) -> Result<SteeringUpdate, LlmError> {
        let mut input = self.input.lock().map_err(|_| unavailable())?;
        if finish && input.messages.is_empty() {
            input.accepting = false;
        }
        let pending = std::mem::take(&mut input.messages);
        let review_anchor = pending.last().and_then(|(_, anchor)| anchor.clone());
        if review_anchor.is_some() && pending.iter().any(|(_, anchor)| anchor.is_none()) {
            return Err(unavailable());
        }
        let input_replies = std::mem::take(&mut input.reply_updates)
            .into_iter()
            .filter_map(|anchor| input.replies.get(&anchor).cloned())
            .collect();
        Ok(SteeringUpdate {
            batch: SteeringBatch {
                messages: pending.into_iter().map(|(text, _)| text).collect(),
                review_anchor,
            },
            input_replies,
        })
    }
}

struct SendMessage(Arc<LiveDialogue>);
impl Tool for SendMessage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_message".into(),
            description: "由你决定在当前会话说什么；用于闲聊、提问、进展和最终回应。只在你想说且内容值得说时调用，没有固定汇报次数。普通 final 只结束内部处理，不自动发送。每次一条完整消息，最多 32768 UTF-8 字节；长内容自行决定分条。after_ms 可选择 0 至 10000 毫秒后再说，默认现在；期间有新消息则旧决定不发，返回 stale，交给你重新考虑。不会结束当前任务。只向可信的原会话发送，不接受地址。sent=true 且有 message_id 才确认送达；unknown 不重发；not_sent/stale 没有确认送达。not_sent 的 failure_kind 若为 passive_window_elapsed / passive_window_missing / passive_quota_exhausted，需等待同一用户的新输入带来有效回复窗口，不要改写正文反复尝试；duplicate_text 表示已有相同正文的投递记录，不重放。不要把未完成说成完成，不重复已说过的话。".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":32768},"after_ms":{"type":"integer","minimum":0,"maximum":10000},"purpose":{"type":"string","enum":["conversation","progress","question","authorization","answer"]}},"required":["text"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.qq.live-send".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_none_or(|v| {
            v.keys()
                .any(|k| !matches!(k.as_str(), "text" | "purpose" | "after_ms"))
        }) || value.get("purpose").is_some_and(|p| {
            !matches!(
                p.as_str(),
                Some("conversation" | "progress" | "question" | "authorization" | "answer")
            )
        }) || value
            .get("after_ms")
            .is_some_and(|p| p.as_u64().is_none_or(|ms| ms > 10_000))
            || value
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(|text| {
                    text.trim().is_empty()
                        || text.len() > 32768
                        || text
                            .chars()
                            .any(|c| c.is_control() && c != '\n' && c != '\t')
                })
        {
            return Err(ToolValidationError {
                message: "text 需要 1 至 32768 UTF-8 字节，after_ms 为 0 至 10000 的整数".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|error| ToolExecutionError::Rejected(error.message))?;
            let rejected = || ToolExecutionError::Rejected("当前没有可投递的对话轮次".into());
            let scope = context.scope().cloned().ok_or_else(rejected)?;
            let turn_id = context.turn_id().ok_or_else(rejected)?;
            let turn = self
                .0
                .turns
                .lock()
                .map_err(|_| rejected())?
                .get(&key(&scope))
                .and_then(Weak::upgrade)
                .filter(|t| t.id == turn_id)
                .ok_or_else(rejected)?;
            if !turn.input.lock().map_err(|_| rejected())?.accepting
                || context.is_cancel_requested()
            {
                return Err(rejected());
            }
            let session_id = scope.session_id.clone();
            let stale = || json!({"sent":false,"message_id":null,"session_id":session_id,"delivery_status":"stale"});
            let changed = turn.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if context.check_review_evidence().is_err()
                || !turn
                    .input
                    .lock()
                    .map_err(|_| rejected())?
                    .messages
                    .is_empty()
            {
                return Ok(stale());
            }
            if let Some(ms) = call
                .arguments
                .get("after_ms")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
            {
                tokio::select! {
                    _ = &mut changed => return Ok(stale()),
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                }
            }
            if context.is_cancel_requested()
                || context.check_review_evidence().is_err()
                || !turn
                    .input
                    .lock()
                    .map_err(|_| rejected())?
                    .messages
                    .is_empty()
            {
                return Ok(stale());
            }
            let (sender, receiver) = oneshot::channel();
            {
                let mut queue = self.0.outgoing.lock().map_err(|_| rejected())?;
                if queue.len() >= 64 {
                    return Err(rejected());
                }
                queue.push_back(LiveSend {
                    scope,
                    turn_id,
                    text: call.arguments["text"]
                        .as_str()
                        .ok_or_else(rejected)?
                        .trim()
                        .into(),
                    image: None,
                    needs_answer: matches!(
                        call.arguments.get("purpose").and_then(Value::as_str),
                        Some("question" | "authorization")
                    ),
                    call_id: format!(
                        "speech-{}",
                        ring::digest::digest(&ring::digest::SHA256, call.id.as_bytes())
                            .as_ref()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    ),
                    context,
                    turn: Arc::downgrade(&turn),
                    result: sender,
                });
            }
            let result = receiver
                .await
                .map_err(|_| ToolExecutionError::Failed("消息投递结果未知，不要重发".into()))?;
            Ok(delivery_json(result, &session_id))
        })
    }
}

fn delivery_json(result: LiveDelivery, session_id: &str) -> Value {
    let (status, message_id, reason) = match result {
        LiveDelivery::Sent(id) => ("sent", Some(id), None),
        LiveDelivery::Unknown => ("unknown", None, None),
        LiveDelivery::NotSent => ("not_sent", None, None),
        LiveDelivery::NotSentReason(reason) => ("not_sent", None, Some(reason)),
        LiveDelivery::Stale => ("stale", None, None),
    };
    let mut value = json!({"sent":message_id.is_some(),"message_id":message_id,"session_id":session_id,"delivery_status":status});
    if let Some(reason) = reason {
        value["failure_kind"] = json!(reason);
    }
    value
}

struct WaitForInput(Arc<LiveDialogue>);
impl Tool for WaitForInput {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wait_for_input".into(),
            description: "你决定暂时听对方继续说时使用。等待当前会话的新输入或最多 60 秒，不发送任何话；新内容随后进入同一轮。没有新输入也不要求你发言，可以安静结束。更长等待使用持久触发器。".into(),
            argument_schema: json!({"type":"object","properties":{"timeout_ms":{"type":"integer","minimum":1,"maximum":60000}},"required":["timeout_ms"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_none_or(|v| v.len() != 1)
            || value
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .is_none_or(|ms| !(1..=60000).contains(&ms))
        {
            return Err(ToolValidationError {
                message: "timeout_ms 需要 1 至 60000 的整数".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|error| ToolExecutionError::Rejected(error.message))?;
            let unavailable = || ToolExecutionError::Rejected("当前会话没有可等待的轮次".into());
            let scope = context.scope().ok_or_else(unavailable)?;
            let turn = self
                .0
                .turns
                .lock()
                .map_err(|_| unavailable())?
                .get(&key(scope))
                .and_then(Weak::upgrade)
                .filter(|t| Some(t.id) == context.turn_id())
                .ok_or_else(unavailable)?;
            let changed = turn.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let input = turn.input.lock().map_err(|_| unavailable())?;
                if !input.accepting || context.is_cancel_requested() {
                    return Err(unavailable());
                }
                if !input.messages.is_empty() {
                    return Ok(json!({"new_input":true}));
                }
            }
            let received = tokio::select! {
                _ = &mut changed => true,
                _ = tokio::time::sleep(Duration::from_millis(call.arguments["timeout_ms"].as_u64().unwrap())) => false,
            };
            Ok(json!({"new_input":received}))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn confirmed_delivery_rejection_exposes_reason_without_claiming_sent() {
        let live = Arc::new(LiveDialogue::default());
        let scope = ContextScope {
            session_id: "trusted-session".into(),
            user_id: "owner".into(),
        };
        let _turn = live.open(&scope, 7).unwrap();
        let tool = live.send_tool();
        let call = ToolCall {
            id: "expired-window".into(),
            name: "send_message".into(),
            arguments: json!({"text":"处理结果"}),
        };
        let context = ToolExecutionContext::new()
            .with_scope(Some(scope))
            .with_turn_id(Some(7));
        let sending = tokio::spawn(async move { tool.execute(call, context).await });
        let outgoing = loop {
            if let Some(outgoing) = live.next() {
                break outgoing;
            }
            tokio::task::yield_now().await;
        };
        outgoing.complete(LiveDelivery::not_sent(Some("passive_window_elapsed")));
        let result = sending.await.unwrap().unwrap();
        assert_eq!(result["sent"], false);
        assert!(result["message_id"].is_null());
        assert_eq!(result["session_id"], "trusted-session");
        assert_eq!(result["delivery_status"], "not_sent");
        assert_eq!(result["failure_kind"], "passive_window_elapsed");
        let unknown = delivery_json(LiveDelivery::Unknown, "trusted-session");
        assert_eq!(unknown["delivery_status"], "unknown");
        assert!(unknown.get("failure_kind").is_none());
        let unrecognized = delivery_json(
            LiveDelivery::not_sent(Some("untrusted raw SDK error")),
            "trusted-session",
        );
        assert_eq!(unrecognized["delivery_status"], "not_sent");
        assert!(unrecognized.get("failure_kind").is_none());
    }

    #[tokio::test]
    async fn question_send_stays_in_live_turn_and_success_returns_both_bound_ids() {
        let live = Arc::new(LiveDialogue::default());
        let scope = ContextScope {
            session_id: "trusted-session".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 7).unwrap();
        let tool = live.send_tool();
        let call = ToolCall {
            id: "send-1".into(),
            name: "send_message".into(),
            arguments: json!({"text":"这个目录需要保留吗？", "purpose":"question"}),
        };
        tool.validate_arguments(&call.arguments).unwrap();
        let context = ToolExecutionContext::new()
            .with_scope(Some(scope.clone()))
            .with_turn_id(Some(7));
        let sending = tokio::spawn(async move { tool.execute(call, context).await });
        let outgoing = loop {
            if let Some(outgoing) = live.next() {
                break outgoing;
            }
            tokio::task::yield_now().await;
        };
        assert!(outgoing.needs_answer);
        assert_eq!(outgoing.scope, scope);
        outgoing.complete(LiveDelivery::Sent("platform-message-7".into()));
        let result = sending.await.unwrap().unwrap();
        assert_eq!(result["sent"], true);
        assert_eq!(result["message_id"], "platform-message-7");
        assert_eq!(result["session_id"], "trusted-session");
        assert!(live.steer(&scope, 7, "保留".into(), || Ok(())).unwrap());
        assert_eq!(turn.take(false).unwrap(), ["保留"]);
        let foreign = ToolExecutionContext::new()
            .with_scope(Some(ContextScope {
                user_id: "stranger".into(),
                ..scope
            }))
            .with_turn_id(Some(7));
        let tool = live.send_tool();
        let call = ToolCall {
            id: "send-2".into(),
            name: "send_message".into(),
            arguments: json!({"text":"越权"}),
        };
        assert!(tool.execute(call, foreign).await.is_err());
    }

    #[test]
    fn steering_is_isolated_durable_before_visible_and_atomically_sealed() {
        let live = LiveDialogue::default();
        let owner = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let other = ContextScope {
            user_id: "other".into(),
            ..owner.clone()
        };
        let turn = live.open(&owner, 1).unwrap();
        assert!(
            !live
                .steer(&other, 1, "attack".into(), || panic!("foreign persist"))
                .unwrap()
        );
        assert!(
            !live
                .steer(&owner, 2, "stale".into(), || panic!("stale persist"))
                .unwrap()
        );
        assert!(
            live.steer(&owner, 1, "not saved".into(), || Err(PluginError::State(
                "disk failure".into()
            )))
            .is_err()
        );
        assert!(turn.take(false).unwrap().is_empty());
        assert!(live.steer(&owner, 1, "先备份".into(), || Ok(())).unwrap());
        assert_eq!(turn.take(true).unwrap(), ["先备份"]);
        assert!(live.steer(&owner, 1, "继续处理".into(), || Ok(())).unwrap());
        assert_eq!(turn.take(false).unwrap(), ["继续处理"]);
        assert!(turn.take(true).unwrap().is_empty());
        assert!(
            !live
                .steer(&owner, 1, "too late".into(), || panic!("closed persist"))
                .unwrap()
        );
        drop(turn);
        let next = live.open(&owner, 2).unwrap();
        assert!(next.take(false).unwrap().is_empty());
    }

    #[test]
    fn consumed_batch_keeps_its_exact_anchor_when_later_input_arrives() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        assert!(
            live.steer_anchored(
                &scope,
                1,
                "展示第一条".into(),
                Some("first".into()),
                || Ok(())
            )
            .unwrap()
        );
        let first = turn.take_batch(false).unwrap();
        assert!(
            live.steer_anchored(
                &scope,
                1,
                "展示后到第二条".into(),
                Some("later".into()),
                || Ok(())
            )
            .unwrap()
        );
        assert_eq!(first.messages, ["展示第一条"]);
        assert_eq!(first.review_anchor.as_deref(), Some("first"));
        let later = turn.take_batch(true).unwrap();
        assert_eq!(later.messages, ["展示后到第二条"]);
        assert_eq!(later.review_anchor.as_deref(), Some("later"));
        assert_eq!(turn.take_batch(true).unwrap(), SteeringBatch::default());
    }

    #[test]
    fn observing_pending_input_never_consumes_it_or_crosses_identity_and_turn() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 7).unwrap();
        assert_eq!(live.pending_anchor(&scope, 7), None);
        live.steer_anchored(
            &scope,
            7,
            "先看看我".into(),
            Some("new-input".into()),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(live.pending_anchor(&scope, 7).as_deref(), Some("new-input"));
        assert_eq!(live.pending_anchor(&scope, 8), None);
        assert_eq!(
            live.pending_anchor(
                &ContextScope {
                    user_id: "other".into(),
                    ..scope.clone()
                },
                7
            ),
            None
        );
        assert_eq!(live.pending_anchor(&scope, 7).as_deref(), Some("new-input"));
        let consumed = turn.take_batch(false).unwrap();
        assert_eq!(consumed.messages, ["先看看我"]);
        assert_eq!(consumed.review_anchor.as_deref(), Some("new-input"));
        assert_eq!(live.pending_anchor(&scope, 7), None);
    }

    #[test]
    fn consumed_input_cannot_be_claimed_and_failed_persistence_keeps_the_original() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        live.steer_anchored(
            &scope,
            1,
            "继续查源码".into(),
            Some("child".into()),
            || Ok(()),
        )
        .unwrap();
        assert!(
            live.claim_input_reply(&scope, 1, "child", "event", "我在看", || {
                Err(PluginError::State("保存失败".into()))
            })
            .is_err()
        );
        assert!(
            live.claim_input_reply(&scope, 1, "child", "event", "我在看", || Ok(false))
                .unwrap()
                .is_none()
        );
        let update = turn.take_update(false).unwrap();
        assert_eq!(update.batch.messages, ["继续查源码"]);
        assert_eq!(update.batch.review_anchor.as_deref(), Some("child"));
        assert!(update.input_replies.is_empty());
        assert!(
            live.claim_input_reply(&scope, 1, "child", "event", "我在看", || {
                panic!("主轮先消费后，前台不能持久化或发出")
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn claiming_and_consuming_the_same_input_share_one_persistence_boundary() {
        use std::{sync::mpsc, thread};
        let live = Arc::new(LiveDialogue::default());
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        live.steer_anchored(
            &scope,
            1,
            "你还在吗？继续查源码".into(),
            Some("child".into()),
            || Ok(()),
        )
        .unwrap();
        let (saving, saved) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let claimant = {
            let live = live.clone();
            let scope = scope.clone();
            thread::spawn(move || {
                live.claim_input_reply(
                    &scope,
                    1,
                    "child",
                    "event",
                    "我在，命令还在运行",
                    || {
                        saving.send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(3)).unwrap();
                        Ok(true)
                    },
                )
                .unwrap()
                .unwrap()
            })
        };
        saved.recv_timeout(Duration::from_secs(3)).unwrap();
        let (started, starting) = mpsc::channel();
        let (consumed, consumption) = mpsc::channel();
        let consumer = {
            let turn = turn.clone();
            thread::spawn(move || {
                started.send(()).unwrap();
                consumed.send(turn.take_update(false).unwrap()).unwrap();
            })
        };
        starting.recv_timeout(Duration::from_secs(3)).unwrap();
        let blocked = consumption.recv_timeout(Duration::from_millis(30));
        release.send(()).unwrap();
        let claim = claimant.join().unwrap();
        consumer.join().unwrap();
        assert!(
            matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)),
            "持久化未完成时不能消费半份交接"
        );
        let update = consumption.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(update.batch.messages, ["你还在吗？继续查源码"]);
        assert_eq!(update.batch.review_anchor.as_deref(), Some("child"));
        assert_eq!(
            update.input_replies,
            [InputReplyHandoff {
                input_message_id: "child".into(),
                input_text: "你还在吗？继续查源码".into(),
                event_id: "event".into(),
                reply_text: "我在，命令还在运行".into(),
                delivery: InputReplyDelivery::Sending,
            }]
        );
        claim
            .complete(InputReplyDelivery::Sent {
                platform_message_id: "platform-1".into(),
            })
            .unwrap();
        let terminal = turn.take_update(false).unwrap();
        assert_eq!(terminal.batch, SteeringBatch::default());
        assert_eq!(
            terminal.input_replies[0].delivery,
            InputReplyDelivery::Sent {
                platform_message_id: "platform-1".into()
            }
        );
        assert_eq!(turn.take_update(false).unwrap(), SteeringUpdate::default());
    }

    #[test]
    fn each_child_keeps_its_own_handoff_even_when_the_words_are_identical() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        let mut claims = Vec::new();
        for id in ["first", "second"] {
            live.steer_anchored(&scope, 1, "继续".into(), Some(id.into()), || Ok(()))
                .unwrap();
            claims.push(
                live.claim_input_reply(
                    &scope,
                    1,
                    id,
                    &format!("event-{id}"),
                    "我在继续",
                    || Ok(true),
                )
                .unwrap()
                .unwrap(),
            );
            assert!(
                live.claim_input_reply(&scope, 1, id, "second-event", "我在继续", || panic!(
                    "同输入二次领取"
                ))
                .unwrap()
                .is_none()
            );
        }
        let update = turn.take_update(false).unwrap();
        assert_eq!(update.batch.messages, ["继续", "继续"]);
        assert_eq!(update.batch.review_anchor.as_deref(), Some("second"));
        assert_eq!(
            update
                .input_replies
                .iter()
                .map(|r| (r.input_message_id.as_str(), r.event_id.as_str()))
                .collect::<Vec<_>>(),
            [("first", "event-first"), ("second", "event-second")]
        );
        for claim in claims {
            claim.complete(InputReplyDelivery::Unknown).unwrap();
        }
        live.steer_anchored(
            &scope,
            1,
            "新的问题".into(),
            Some("third".into()),
            || Ok(()),
        )
        .unwrap();
        let next = turn.take_update(false).unwrap();
        assert_eq!(next.batch.messages, ["新的问题"]);
        assert_eq!(next.batch.review_anchor.as_deref(), Some("third"));
        assert_eq!(next.input_replies.len(), 2);
        assert!(next.input_replies.iter().all(|r| r.input_message_id != "third" && r.delivery == InputReplyDelivery::Unknown));
    }

    #[test]
    fn input_reply_handoffs_do_not_cross_scope_turn_or_reopen_closed_inputs() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        live.steer_anchored(&scope, 1, "继续".into(), Some("child".into()), || Ok(()))
            .unwrap();
        for (candidate, id) in [
            (
                ContextScope {
                    user_id: "other".into(),
                    ..scope.clone()
                },
                1,
            ),
            (
                ContextScope {
                    session_id: "elsewhere".into(),
                    ..scope.clone()
                },
                1,
            ),
            (scope.clone(), 2),
        ] {
            assert!(
                live.claim_input_reply(&candidate, id, "child", "event", "我在", || panic!(
                    "跨身份或轮次领取"
                ))
                .unwrap()
                .is_none()
            );
        }
        let claim = live
            .claim_input_reply(&scope, 1, "child", "event", "我在", || Ok(true))
            .unwrap()
            .unwrap();
        turn.take_update(false).unwrap();
        assert_eq!(turn.take_update(true).unwrap(), SteeringUpdate::default());
        claim.complete(InputReplyDelivery::Unknown).unwrap();
        assert_eq!(turn.take_update(false).unwrap(), SteeringUpdate::default());
        assert!(
            !live
                .steer_anchored(
                    &scope,
                    1,
                    "晚到原话".into(),
                    Some("late".into()),
                    || panic!("轮次已关闭")
                )
                .unwrap()
        );
        drop(turn);
        let next = live.open(&scope, 2).unwrap();
        live.steer_anchored(&scope, 2, "继续".into(), Some("next".into()), || Ok(()))
            .unwrap();
        let abandoned = live
            .claim_input_reply(&scope, 2, "next", "next-event", "我在", || Ok(true))
            .unwrap()
            .unwrap();
        drop(next);
        let replacement = live.open(&scope, 3).unwrap();
        abandoned
            .complete(InputReplyDelivery::Sent {
                platform_message_id: "late-platform-id".into(),
            })
            .unwrap();
        assert_eq!(
            replacement.take_update(false).unwrap(),
            SteeringUpdate::default()
        );
    }

    #[tokio::test]
    async fn delivery_updates_are_not_new_input_and_do_not_wake_wait_for_input() {
        let live = Arc::new(LiveDialogue::default());
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        let original = live
            .turns
            .lock()
            .unwrap()
            .get(&key(&scope))
            .unwrap()
            .upgrade()
            .unwrap();
        for (index, delivery) in [
            InputReplyDelivery::Sent {
                platform_message_id: "platform-1".into(),
            },
            InputReplyDelivery::Unknown,
            InputReplyDelivery::Failed,
            InputReplyDelivery::NotSent,
        ]
        .into_iter()
        .enumerate()
        {
            let anchor = format!("child-{index}");
            live.steer_anchored(&scope, 1, "继续".into(), Some(anchor.clone()), || Ok(()))
                .unwrap();
            let claim = live
                .claim_input_reply(
                    &scope,
                    1,
                    &anchor,
                    &format!("event-{index}"),
                    "我在",
                    || Ok(true),
                )
                .unwrap()
                .unwrap();
            let first = turn.take_update(false).unwrap();
            assert_eq!(first.input_replies[0].delivery, InputReplyDelivery::Sending);
            let tool = live.wait_tool();
            let waiting = tool.execute(
                ToolCall {
                    id: format!("wait-{index}"),
                    name: "wait_for_input".into(),
                    arguments: json!({"timeout_ms":100}),
                },
                ToolExecutionContext::new()
                    .with_scope(Some(scope.clone()))
                    .with_turn_id(Some(1)),
            );
            tokio::pin!(waiting);
            tokio::select! {
                result = &mut waiting => panic!("交接后没有新的用户输入：{result:?}"),
                _ = tokio::task::yield_now() => {},
            }
            let changed = original.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            claim.complete(delivery.clone()).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(5), &mut changed)
                    .await
                    .is_err()
            );
            assert_eq!(waiting.await.unwrap(), json!({"new_input":false}));
            let update = turn.take_update(false).unwrap();
            assert_eq!(update.batch, SteeringBatch::default());
            assert_eq!(update.input_replies.len(), 1);
            assert_eq!(update.input_replies[0].input_message_id, anchor);
            assert_eq!(update.input_replies[0].delivery, delivery);
            assert_eq!(turn.take_update(false).unwrap(), SteeringUpdate::default());
        }
    }
}
