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
            .get_mut(&self.event_id)
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
        input.reply_updates.insert(self.event_id);
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
    segmentation: Mutex<Option<LiveSegmentation>>,
}
#[derive(Clone)]
struct LiveSegmentation {
    preferences: Option<Arc<dyn eve_segment_api::SegmentPreferences>>,
    policy: eve_segment_api::SegmentPolicy,
}
impl LiveDialogue {
    /// 可信宿主在装配完成后配置；None 表示通道明确关闭分条。
    /// 未装配的独立工具夹具仍使用 QQ 默认策略与默认偏好。
    pub fn configure_segmentation(
        &self,
        preferences: Option<Arc<dyn eve_segment_api::SegmentPreferences>>,
        policy: eve_segment_api::SegmentPolicy,
    ) -> PluginResult<()> {
        policy
            .validate()
            .map_err(|_| PluginError::Task("实时分条策略无效".into()))?;
        if policy.max_segments > crate::QQ_MAX_SEGMENTS
            || policy.defaults.max_segment_bytes > 32768
            || policy.max_pause_ms > crate::QQ_SEGMENT_POLICY.max_pause_ms
        {
            return Err(PluginError::Task("实时分条策略超出 QQ 上限".into()));
        }
        *self
            .segmentation
            .lock()
            .map_err(|_| PluginError::Task("实时分条设置锁不可用".into()))? =
            Some(LiveSegmentation {
                preferences,
                policy,
            });
        Ok(())
    }

    fn effective_segmentation(
        &self,
        scope: &ContextScope,
    ) -> Result<eve_segment_api::EffectiveSegmentation, ToolExecutionError> {
        let unavailable = || ToolExecutionError::Rejected("当前分条设置不可用；未发送消息".into());
        let configured = self.segmentation.lock().map_err(|_| unavailable())?.clone();
        let (preference, policy) = match configured {
            Some(LiveSegmentation {
                preferences: Some(store),
                policy,
            }) => {
                let preference = store
                    .get(&eve_segment_api::SegmentScope {
                        channel: crate::QQ_SEGMENT_CHANNEL.into(),
                        session_id: scope.session_id.clone(),
                        user_id: scope.user_id.clone(),
                    })
                    .map_err(|_| unavailable())?;
                (preference, policy)
            }
            Some(LiveSegmentation {
                preferences: None,
                policy,
            }) => (
                eve_segment_api::SegmentPreference {
                    enabled: Some(false),
                    ..Default::default()
                },
                policy,
            ),
            None => (
                eve_segment_api::SegmentPreference::default(),
                crate::QQ_SEGMENT_POLICY,
            ),
        };
        preference.effective(&policy).map_err(|_| unavailable())
    }

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
            || input.replies.contains_key(event_id)
            || input.replies.values().any(|reply| {
                reply.kind == InputReplyKind::UserInput
                    && reply.input_message_id == input_message_id
            })
            || input.replies.len() >= 64
        {
            return Ok(None);
        }
        let handoff = InputReplyHandoff {
            kind: eve_llm_api::InputReplyKind::UserInput,
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
        input.replies.insert(event_id.into(), handoff);
        input.reply_updates.insert(event_id.into());
        Ok(Some(ClaimedInputReply {
            turn: Arc::downgrade(&turn),
            event_id: event_id.into(),
        }))
    }
    /// 活动进展没有新用户输入；同轮原子保存发言事实，正常工具返回时一起读到。
    pub(crate) fn claim_progress_reply(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        handoff: InputReplyHandoff,
        persist: impl FnOnce() -> PluginResult<bool>,
    ) -> PluginResult<Option<ClaimedInputReply>> {
        self.claim_progress(scope, turn_id, handoff, false, persist)
    }

    /// 同一最新待消费原话的新进度；锁内核对锚点和原话，不消费或重复登记用户输入。
    /// persist 仍须复核真实控制代、工具批次及取消，先保存 sending 再交接。
    pub(crate) fn claim_pending_progress_reply(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        handoff: InputReplyHandoff,
        persist: impl FnOnce() -> PluginResult<bool>,
    ) -> PluginResult<Option<ClaimedInputReply>> {
        self.claim_progress(scope, turn_id, handoff, true, persist)
    }

    fn claim_progress(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        handoff: InputReplyHandoff,
        pending: bool,
        persist: impl FnOnce() -> PluginResult<bool>,
    ) -> PluginResult<Option<ClaimedInputReply>> {
        if handoff.kind != InputReplyKind::Progress
            || handoff.delivery != InputReplyDelivery::Sending
        {
            return Err(PluginError::Task("进展交流交接类型无效".into()));
        }
        handoff
            .validate()
            .map_err(|_| PluginError::Task("进展交流事实无效".into()))?;
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
        let bytes = serde_json::to_vec(&handoff)
            .map_err(|_| PluginError::Task("交流交接事实无法序列化".into()))?
            .len();
        let input_matches = if pending {
            input.messages.last().is_some_and(|(text, anchor)| {
                anchor.as_deref() == Some(handoff.input_message_id.as_str())
                    && text == &handoff.input_text
            })
        } else {
            input.messages.is_empty()
        };
        if !input.accepting
            || !input_matches
            || input.replies.contains_key(&handoff.event_id)
            || input.replies.len() >= 64
            || input.reply_bytes.saturating_add(bytes) > 256 * 1024
        {
            return Ok(None);
        }
        if !persist()? {
            return Ok(None);
        }
        let event_id = handoff.event_id.clone();
        input.reply_bytes += bytes;
        input.replies.insert(event_id.clone(), handoff);
        input.reply_updates.insert(event_id.clone());
        Ok(Some(ClaimedInputReply {
            turn: Arc::downgrade(&turn),
            event_id,
        }))
    }

    /// 只读判断原轮仍在接收且没有待消费的新输入，不改变用户消息或授权锚点。
    pub(crate) fn progress_is_current(&self, scope: &ContextScope, turn_id: u64) -> bool {
        self.turns
            .lock()
            .ok()
            .and_then(|turns| turns.get(&key(scope)).and_then(Weak::upgrade))
            .filter(|turn| turn.id == turn_id)
            .is_some_and(|turn| {
                turn.input
                    .lock()
                    .is_ok_and(|input| input.accepting && input.messages.is_empty())
            })
    }

    /// 待消费进度只绑定最后一条原话；消费或新消息到来立即撤销未认领资格。
    pub(crate) fn pending_progress_is_current(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        anchor: &str,
    ) -> bool {
        self.pending_anchor(scope, turn_id).as_deref() == Some(anchor)
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
const MAX_SEND_PARTS: usize = 5;
const MAX_SEND_BYTES: usize = 32768;

#[derive(Debug, Eq, PartialEq)]
struct SendPart {
    text: String,
    after_ms: u64,
}
struct PreparedParts {
    parts: Vec<SendPart>,
    requested_part_count: usize,
    applied_mode: &'static str,
}

fn prepare_parts(
    parts: &[Value],
    after_ms: u64,
    effective: eve_segment_api::EffectiveSegmentation,
) -> Result<PreparedParts, ToolExecutionError> {
    let mut chosen = parts
        .iter()
        .map(|part| SendPart {
            text: part["text"].as_str().unwrap().into(),
            after_ms: part.get("after_ms").and_then(Value::as_u64).unwrap_or(0),
        })
        .collect::<Vec<_>>();
    chosen[0].after_ms = chosen[0].after_ms.max(after_ms);
    let maximum = if effective.enabled {
        effective.limits.max_segments
    } else {
        1
    };
    let applied_mode = if !effective.enabled {
        "merged_disabled"
    } else if chosen.len() > maximum {
        "merged_limit"
    } else {
        "explicit_parts"
    };
    if chosen.len() > maximum {
        let tail = chosen.split_off(maximum - 1);
        chosen.push(SendPart {
            text: tail
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            after_ms: tail[0].after_ms,
        });
    }
    if chosen.iter().map(|part| part.text.len()).sum::<usize>() > MAX_SEND_BYTES
        || chosen
            .iter()
            .any(|part| part.text.len() > effective.limits.max_segment_bytes)
    {
        return Err(ToolExecutionError::Rejected(
            "应用当前分条设置后正文超过字节上限；未发送消息".into(),
        ));
    }
    for part in chosen.iter_mut().skip(1) {
        part.after_ms = (part.after_ms * u64::from(effective.pause_percent) / 100)
            .min(effective.limits.max_pause_ms);
    }
    Ok(PreparedParts {
        parts: chosen,
        requested_part_count: parts.len(),
        applied_mode,
    })
}

fn send_digest(id: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, id.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 只区分宿主同一次显式分条计划的不同部分，不豁免旧未知决定的重放。
pub(crate) fn distinct_parts_of_same_send(first: &str, second: &str) -> bool {
    fn identity(value: &str) -> Option<(&str, usize)> {
        let (digest, ordinal) = value.strip_prefix("speech-parts-")?.rsplit_once('-')?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || ordinal.len() != 1
        {
            return None;
        }
        let ordinal = ordinal.parse::<usize>().ok()?;
        (ordinal < MAX_SEND_PARTS).then_some((digest, ordinal))
    }
    matches!((identity(first), identity(second)),
        (Some((left, a)), Some((right, b))) if left == right && a != b)
}

fn valid_send_text(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        !text.trim().is_empty()
            && text.len() <= MAX_SEND_BYTES
            && !text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
    })
}

impl SendMessage {
    /// 每条复用原发送队列与真实回执；后一条不会提前入队。
    async fn send_one(
        &self,
        scope: &ContextScope,
        turn: &Arc<Turn>,
        context: &ToolExecutionContext,
        part: &SendPart,
        call_id: String,
        needs_answer: bool,
    ) -> Result<(LiveDelivery, bool), ToolExecutionError> {
        let rejected = || ToolExecutionError::Rejected("当前没有可投递的对话轮次".into());
        let changed = turn.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let current = || -> Result<bool, ToolExecutionError> {
            let input = turn.input.lock().map_err(|_| rejected())?;
            Ok(input.accepting
                && input.messages.is_empty()
                && !context.is_cancel_requested()
                && context.check_review_evidence().is_ok())
        };
        if !current()? {
            return Ok((LiveDelivery::Stale, false));
        }
        if part.after_ms > 0 {
            tokio::select! {
                _ = &mut changed => return Ok((LiveDelivery::Stale, false)),
                _ = tokio::time::sleep(Duration::from_millis(part.after_ms)) => {}
            }
        }
        if !current()? {
            return Ok((LiveDelivery::Stale, false));
        }
        let (sender, receiver) = oneshot::channel();
        {
            let mut queue = self.0.outgoing.lock().map_err(|_| rejected())?;
            if queue.len() >= 64 {
                return Err(rejected());
            }
            queue.push_back(LiveSend {
                scope: scope.clone(),
                turn_id: turn.id,
                text: part.text.clone(),
                image: None,
                needs_answer,
                call_id,
                context: context.clone(),
                turn: Arc::downgrade(turn),
                result: sender,
            });
        }
        // 队列已经交出；通道消失不能证明没有发，未获得回执记未知。
        let result = receiver.await.unwrap_or(LiveDelivery::Unknown);
        let result = match result {
            LiveDelivery::Sent(id) if !crate::state::valid_message_id(&id) => LiveDelivery::Unknown,
            other => other,
        };
        Ok((result, true))
    }
}

impl Tool for SendMessage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_message".into(),
            description: "自主向当前可信会话发送闲聊、进展、提问或回答；普通 final 只结束内部处理，不自动发送。text 是完整一条；parts 明确选择 1 至 5 个消息边界（每项 text、可选 after_ms），两者必须二选一，总计最多 32768 UTF-8 字节。实际条数遵循 /segment（默认最多 3 条，硬上限 5）；关闭时用两个换行合成全文，超上限合并尾部，保留正文；设置不可用则零发送。顶层与第一部分 after_ms 取较大值；后续在上一条真实回执后等待，按当前节奏缩放并截到停顿上限；0 至 10000 毫秒，默认立即，不按字数模拟打字。逐条等待真实回执；失败、未知或新输入停止后缀。attempted=false 表示未交给发送队列；单条 sent=true 且有 message_id 才确认送达，parts 的 sent=true 表示所有实际部分均确认，根 message_id 为 null，请读 parts 和 applied_mode。unknown 不重发；passive_window_* / passive_quota_exhausted 等待新输入，不反复改写尝试；duplicate_text 不重放。不结束任务、不接受地址，不把未完成说成完成。".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":32768},"parts":{"type":"array","minItems":1,"maxItems":5,"items":{"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":32768},"after_ms":{"type":"integer","minimum":0,"maximum":10000}},"required":["text"],"additionalProperties":false}},"after_ms":{"type":"integer","minimum":0,"maximum":10000},"purpose":{"type":"string","enum":["conversation","progress","question","authorization","answer"]}},"oneOf":[{"required":["text"]},{"required":["parts"]}],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.qq.live-send".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_none_or(|v| {
            v.keys()
                .any(|k| !matches!(k.as_str(), "text" | "parts" | "purpose" | "after_ms"))
        }) || value.get("purpose").is_some_and(|p| {
            !matches!(
                p.as_str(),
                Some("conversation" | "progress" | "question" | "authorization" | "answer")
            )
        }) || value
            .get("after_ms")
            .is_some_and(|p| p.as_u64().is_none_or(|ms| ms > 10_000))
            || (value.get("text").is_some() == value.get("parts").is_some())
            || value.get("text").is_some_and(|text| !valid_send_text(text))
            || value.get("parts").is_some_and(|parts| {
                parts.as_array().is_none_or(|parts| {
                    parts.is_empty()
                        || parts.len() > MAX_SEND_PARTS
                        || parts.iter().any(|part| {
                            part.as_object().is_none_or(|fields| {
                                fields
                                    .keys()
                                    .any(|k| !matches!(k.as_str(), "text" | "after_ms"))
                            }) || part.get("text").is_none_or(|text| !valid_send_text(text))
                                || part.get("after_ms").is_some_and(|delay| {
                                    delay.as_u64().is_none_or(|ms| ms > 10_000)
                                })
                        })
                        || parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .map(str::len)
                            .sum::<usize>()
                            > MAX_SEND_BYTES
                })
            })
        {
            return Err(ToolValidationError {
                message: "text 与 parts 必须二选一；parts 为 1 至 5 条，总计 1 至 32768 UTF-8 字节，after_ms 为 0 至 10000 的整数".into(),
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
            let digest = send_digest(&call.id);
            let after_ms = call
                .arguments
                .get("after_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let needs_answer = matches!(
                call.arguments.get("purpose").and_then(Value::as_str),
                Some("question" | "authorization")
            );
            let Some(parts) = call.arguments.get("parts").and_then(Value::as_array) else {
                let part = SendPart {
                    text: call.arguments["text"]
                        .as_str()
                        .ok_or_else(rejected)?
                        .trim()
                        .into(),
                    after_ms,
                };
                let (result, _) = self
                    .send_one(
                        &scope,
                        &turn,
                        &context,
                        &part,
                        format!("speech-{digest}"),
                        needs_answer,
                    )
                    .await?;
                return Ok(delivery_json(result, &scope.session_id));
            };
            let prepared = prepare_parts(parts, after_ms, self.0.effective_segmentation(&scope)?)?;
            let mut receipts = Vec::with_capacity(prepared.parts.len());
            let mut stopped = None;
            for (index, part) in prepared.parts.iter().enumerate() {
                if let Some(reason) = stopped {
                    receipts.push(json!({"index":index,"sent":false,"message_id":null,
                        "delivery_status":"not_sent","attempted":false,"failure_kind":reason}));
                    continue;
                }
                let (result, attempted) = match self
                    .send_one(
                        &scope,
                        &turn,
                        &context,
                        part,
                        format!("speech-parts-{digest}-{index}"),
                        needs_answer,
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    // 拒绝发生在入队前，已有发送前缀仍由聚合回执保留。
                    Err(_) => (LiveDelivery::NotSentReason("queue_unavailable"), false),
                };
                stopped = match &result {
                    LiveDelivery::Sent(_) => None,
                    LiveDelivery::Unknown => Some("previous_part_unknown"),
                    LiveDelivery::Stale => Some("previous_part_stale"),
                    LiveDelivery::NotSent | LiveDelivery::NotSentReason(_) => {
                        Some("previous_part_not_sent")
                    }
                };
                let mut receipt = delivery_json(result, &scope.session_id);
                receipt["index"] = json!(index);
                receipt["attempted"] = json!(attempted);
                receipts.push(receipt);
            }
            let mut result = parts_delivery_json(receipts, &scope.session_id);
            result["requested_part_count"] = json!(prepared.requested_part_count);
            result["actual_part_count"] = json!(prepared.parts.len());
            result["applied_mode"] = json!(prepared.applied_mode);
            Ok(result)
        })
    }
}

fn delivery_json(result: LiveDelivery, session_id: &str) -> Value {
    let (status, message_id, reason) = match result {
        LiveDelivery::Sent(id) if crate::state::valid_message_id(&id) => ("sent", Some(id), None),
        LiveDelivery::Sent(_) => ("unknown", None, None),
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

fn parts_delivery_json(parts: Vec<Value>, session_id: &str) -> Value {
    let sent_count = parts.iter().filter(|part| part["sent"] == true).count();
    let all_sent = sent_count == parts.len();
    let stopped = parts.iter().find(|part| part["sent"] != true).cloned();
    let status = if all_sent {
        "sent"
    } else if sent_count > 0 {
        "partial"
    } else {
        stopped
            .as_ref()
            .and_then(|part| part["delivery_status"].as_str())
            .unwrap_or("not_sent")
    };
    let mut result = json!({"sent":all_sent,"message_id":null,"session_id":session_id,
        "delivery_status":status,"sent_count":sent_count,
        "attempted_count":parts.iter().filter(|part| part["attempted"] == true).count(),"parts":parts});
    if let Some(stopped) = stopped {
        result["stopped_by"] = stopped["delivery_status"].clone();
        if let Some(reason) = stopped.get("failure_kind") {
            result["failure_kind"] = reason.clone();
        }
    }
    result
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

    fn parts_scope() -> ContextScope {
        ContextScope {
            session_id: "trusted-session".into(),
            user_id: "owner".into(),
        }
    }

    async fn next_part(live: &LiveDialogue) -> LiveSend {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(outgoing) = live.next() {
                    return outgoing;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("本部分应进入发送队列")
    }

    #[test]
    fn explicit_parts_validate_exclusive_shape_and_total_utf8_budget() {
        let tool = Arc::new(LiveDialogue::default()).send_tool();
        for invalid in [
            json!({}),
            json!({"text":"嗯", "parts":[{"text":"嗯"}]}),
            json!({"parts":[]}),
            json!({"parts":[{"text":" "}]}),
            json!({"parts":[{"text":"嗯","address":"other"}]}),
            json!({"parts":[{"text":"嗯","after_ms":-1}]}),
            json!({"parts":[{"text":"嗯","after_ms":1.5}]}),
            json!({"parts":[{"text":"嗯","after_ms":10001}]}),
            json!({"parts":[{"text":"含\u{0000}控制符"}]}),
            json!({"parts":[{"text":"嗯"},{"text":"嗯"},{"text":"嗯"},{"text":"嗯"},{"text":"嗯"},{"text":"嗯"}]}),
            json!({"parts":[{"text":"中".repeat(6000)},{"text":"文".repeat(6000)}]}),
            json!({"parts":[{"text":"嗯"}],"extra":true}),
        ] {
            assert!(tool.validate_arguments(&invalid).is_err(), "{invalid}");
        }
        for valid in [
            json!({"text":"保持兼容"}),
            json!({"parts":[{"text":"嗯"},{"text":"嗯","after_ms":0}]}),
            json!({"parts":[{"text":"两行\n\t正文"}],"after_ms":10000,"purpose":"question"}),
        ] {
            tool.validate_arguments(&valid).unwrap();
        }
    }

    #[test]
    fn preference_preserves_text_combines_tail_and_scales_only_between_parts() {
        use eve_segment_api::SegmentPreference;
        let values = vec![
            json!({"text":" 第一部分 ","after_ms":3000}),
            json!({"text":"第二部分", "after_ms":4000}),
            json!({"text":"第三部分", "after_ms":10000}),
        ];
        let effective = SegmentPreference {
            max_segments: Some(2),
            pause_percent: Some(50),
            ..Default::default()
        }
        .effective(&crate::QQ_SEGMENT_POLICY)
        .unwrap();
        let prepared = prepare_parts(&values, 3500, effective).unwrap();
        assert_eq!(prepared.applied_mode, "merged_limit");
        assert_eq!(prepared.requested_part_count, 3);
        assert_eq!(
            prepared.parts,
            [
                SendPart {
                    text: " 第一部分 ".into(),
                    after_ms: 3500
                },
                SendPart {
                    text: "第二部分\n\n第三部分".into(),
                    after_ms: 1250
                },
            ]
        );
        let disabled = SegmentPreference {
            enabled: Some(false),
            pause_percent: Some(0),
            ..Default::default()
        }
        .effective(&crate::QQ_SEGMENT_POLICY)
        .unwrap();
        let whole = prepare_parts(&values, 3500, disabled).unwrap();
        assert_eq!(whole.applied_mode, "merged_disabled");
        assert_eq!(
            whole.parts,
            [SendPart {
                text: " 第一部分 \n\n第二部分\n\n第三部分".into(),
                after_ms: 3500,
            }]
        );
        let near_limit = vec![
            json!({"text":"a".repeat(MAX_SEND_BYTES - 1)}),
            json!({"text":"b"}),
        ];
        assert!(
            prepare_parts(&near_limit, 0, disabled).is_err(),
            "合并新增换行也必须计入总预算"
        );
    }

    #[derive(Default)]
    struct ChangingPreferences {
        preference: Mutex<eve_segment_api::SegmentPreference>,
        unavailable: Mutex<bool>,
        scopes: Mutex<Vec<eve_segment_api::SegmentScope>>,
    }
    impl eve_segment_api::SegmentPreferences for ChangingPreferences {
        fn get(
            &self,
            scope: &eve_segment_api::SegmentScope,
        ) -> eve_segment_api::SegmentPreferenceResult<eve_segment_api::SegmentPreference> {
            scope.validate()?;
            self.scopes.lock().unwrap().push(scope.clone());
            if *self.unavailable.lock().unwrap() {
                return Err(eve_segment_api::SegmentPreferenceError::Unavailable);
            }
            Ok(*self.preference.lock().unwrap())
        }
        fn update(
            &self,
            _: &eve_segment_api::SegmentScope,
            _: eve_segment_api::SegmentChange,
        ) -> eve_segment_api::SegmentPreferenceResult<eve_segment_api::SegmentPreference> {
            panic!("发送工具不能修改用户设置")
        }
    }

    #[tokio::test]
    async fn segmentation_configuration_reads_current_trusted_scope_and_fails_without_sending() {
        let live = Arc::new(LiveDialogue::default());
        let scope = parts_scope();
        let _turn = live.open(&scope, 7).unwrap();
        let defaults = live.effective_segmentation(&scope).unwrap();
        assert!(defaults.enabled);
        assert_eq!(defaults.limits.max_segments, 3);
        live.configure_segmentation(None, crate::QQ_SEGMENT_POLICY)
            .unwrap();
        assert!(!live.effective_segmentation(&scope).unwrap().enabled);
        let store = Arc::new(ChangingPreferences::default());
        live.configure_segmentation(Some(store.clone()), crate::QQ_SEGMENT_POLICY)
            .unwrap();
        assert_eq!(
            live.effective_segmentation(&scope)
                .unwrap()
                .limits
                .max_segments,
            3
        );
        store.preference.lock().unwrap().max_segments = Some(2);
        assert_eq!(
            live.effective_segmentation(&scope)
                .unwrap()
                .limits
                .max_segments,
            2
        );
        assert!(
            store
                .scopes
                .lock()
                .unwrap()
                .iter()
                .all(|seen| seen.channel == "qq"
                    && seen.session_id == scope.session_id
                    && seen.user_id == scope.user_id)
        );
        *store.unavailable.lock().unwrap() = true;
        let result = live
            .send_tool()
            .execute(
                ToolCall {
                    id: "bad-preferences".into(),
                    name: "send_message".into(),
                    arguments: json!({"parts":[{"text":"绝不静默回退"}]}),
                },
                ToolExecutionContext::new()
                    .with_scope(Some(scope))
                    .with_turn_id(Some(7)),
            )
            .await;
        assert!(matches!(result, Err(ToolExecutionError::Rejected(_))));
        assert!(live.next().is_none());
    }

    #[tokio::test]
    async fn explicit_parts_wait_for_each_real_receipt_and_use_stable_distinct_ids() {
        let live = Arc::new(LiveDialogue::default());
        let scope = parts_scope();
        let _turn = live.open(&scope, 7).unwrap();
        let tool = live.send_tool();
        let context = ToolExecutionContext::new()
            .with_scope(Some(scope))
            .with_turn_id(Some(7));
        let sending = tokio::spawn(async move {
            tool.execute(ToolCall {
            id: "three-parts".into(), name: "send_message".into(),
            arguments: json!({"parts":[{"text":"嗯"},{"text":"嗯"},{"text":"完成了"}],"purpose":"question"}),
        }, context).await
        });
        for index in 0..3 {
            let outgoing = next_part(&live).await;
            assert_eq!(
                outgoing.call_id,
                format!("speech-parts-{}-{index}", send_digest("three-parts"))
            );
            assert!(outgoing.needs_answer);
            assert!(outgoing.current());
            tokio::task::yield_now().await;
            assert!(live.next().is_none(), "尚无回执不能提前入队下一条");
            assert!(!sending.is_finished());
            outgoing.complete(LiveDelivery::Sent(format!("platform-part-{index}")));
        }
        let result = sending.await.unwrap().unwrap();
        assert_eq!(result["sent"], true);
        assert!(result["message_id"].is_null());
        assert_eq!(result["sent_count"], 3);
        assert_eq!(result["attempted_count"], 3);
        assert_eq!(result["actual_part_count"], 3);
        assert_eq!(result["applied_mode"], "explicit_parts");
        for index in 0..3 {
            assert_eq!(
                result["parts"][index]["message_id"],
                format!("platform-part-{index}")
            );
            assert_eq!(result["parts"][index]["index"], index);
        }
    }

    #[tokio::test]
    async fn explicit_parts_stop_after_unknown_invalid_id_not_sent_or_stale() {
        for (outcome, state, suffix_reason) in [
            (LiveDelivery::Unknown, "unknown", "previous_part_unknown"),
            (
                LiveDelivery::Sent(String::new()),
                "unknown",
                "previous_part_unknown",
            ),
            (LiveDelivery::NotSent, "not_sent", "previous_part_not_sent"),
            (LiveDelivery::Stale, "stale", "previous_part_stale"),
        ] {
            let live = Arc::new(LiveDialogue::default());
            let scope = parts_scope();
            let _turn = live.open(&scope, 7).unwrap();
            let tool = live.send_tool();
            let context = ToolExecutionContext::new()
                .with_scope(Some(scope))
                .with_turn_id(Some(7));
            let sending = tokio::spawn(async move {
                tool.execute(ToolCall {
                id: "partial-plan".into(), name: "send_message".into(),
                arguments: json!({"parts":[{"text":"已完成前缀"},{"text":"本条未确认"},{"text":"后缀绝不能发"}]}),
            }, context).await
            });
            next_part(&live)
                .await
                .complete(LiveDelivery::Sent("prefix-id".into()));
            next_part(&live).await.complete(outcome);
            let result = sending.await.unwrap().unwrap();
            assert!(!result["sent"].as_bool().unwrap());
            assert_eq!(result["delivery_status"], "partial");
            assert_eq!(result["stopped_by"], state);
            assert_eq!(result["sent_count"], 1);
            assert_eq!(result["attempted_count"], 2);
            assert_eq!(result["parts"][1]["delivery_status"], state);
            assert_eq!(result["parts"][2]["attempted"], false);
            assert_eq!(result["parts"][2]["failure_kind"], suffix_reason);
            assert!(live.next().is_none());
        }
    }

    #[tokio::test]
    async fn new_input_interrupts_part_pause_and_preserves_confirmed_prefix() {
        let live = Arc::new(LiveDialogue::default());
        let scope = parts_scope();
        let turn = live.open(&scope, 7).unwrap();
        let tool = live.send_tool();
        let context = ToolExecutionContext::new()
            .with_scope(Some(scope.clone()))
            .with_turn_id(Some(7));
        let sending = tokio::spawn(async move {
            tool.execute(ToolCall {
            id: "paused-plan".into(), name: "send_message".into(),
            arguments: json!({"parts":[{"text":"先说这一条"},{"text":"这条过时了","after_ms":3000},{"text":"后缀也过时了"}]}),
        }, context).await
        });
        next_part(&live)
            .await
            .complete(LiveDelivery::Sent("prefix-id".into()));
        tokio::task::yield_now().await;
        assert!(live.steer(&scope, 7, "先停一下".into(), || Ok(())).unwrap());
        let result = tokio::time::timeout(Duration::from_secs(1), sending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result["delivery_status"], "partial");
        assert_eq!(result["parts"][1]["delivery_status"], "stale");
        assert_eq!(result["parts"][1]["attempted"], false);
        assert_eq!(result["parts"][2]["attempted"], false);
        assert_eq!(result["sent_count"], 1);
        assert_eq!(turn.take(false).unwrap(), ["先停一下"]);
        assert!(live.next().is_none());
    }

    #[test]
    fn dedup_exception_only_identifies_distinct_ordinals_of_one_explicit_send() {
        let first = format!("speech-parts-{}-0", send_digest("one-plan"));
        let next = format!("speech-parts-{}-1", send_digest("one-plan"));
        assert!(distinct_parts_of_same_send(&first, &next));
        assert!(!distinct_parts_of_same_send(&first, &first));
        assert!(!distinct_parts_of_same_send(
            &first,
            &format!("speech-parts-{}-1", send_digest("other-plan"))
        ));
        for malformed in [
            "speech-parts-abc-1".to_owned(),
            format!("speech-parts-{}-5", send_digest("one-plan")),
            format!("speech-parts-{}-01", send_digest("one-plan")),
            format!("speech-{}", send_digest("one-plan")),
        ] {
            assert!(!distinct_parts_of_same_send(&first, &malformed));
        }
    }

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
    fn natural_progress_handoff_does_not_invent_input_or_cross_turns_and_unknown_is_terminal() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 7).unwrap();
        let fact = |id: &str| InputReplyHandoff {
            kind: InputReplyKind::Progress,
            input_message_id: "original".into(),
            input_text: "查一下编译问题".into(),
            event_id: id.into(),
            reply_text: "编译还在运行，我在等结果".into(),
            delivery: InputReplyDelivery::Sending,
        };
        assert!(live.progress_is_current(&scope, 7));
        assert!(!live.progress_is_current(&scope, 8));
        assert!(
            live.claim_progress_reply(&scope, 8, fact("bad-turn"), || panic!("跨轮持久化"))
                .unwrap()
                .is_none()
        );
        assert!(
            live.claim_progress_reply(&scope, 7, fact("failed-save"), || Err(PluginError::State(
                "磁盘失败".into()
            )))
            .is_err()
        );
        assert_eq!(turn.take_update(false).unwrap(), SteeringUpdate::default());
        let claim = live
            .claim_progress_reply(&scope, 7, fact("progress-1"), || Ok(true))
            .unwrap()
            .unwrap();
        let update = turn.take_update(false).unwrap();
        assert_eq!(
            update.batch,
            SteeringBatch::default(),
            "没有用户输入或新授权锚点"
        );
        assert_eq!(update.input_replies, [fact("progress-1")]);
        claim.complete(InputReplyDelivery::Unknown).unwrap();
        let update = turn.take_update(false).unwrap();
        assert_eq!(update.batch, SteeringBatch::default());
        assert_eq!(
            update.input_replies[0].delivery,
            InputReplyDelivery::Unknown
        );
        assert!(
            live.claim_progress_reply(&scope, 7, fact("progress-1"), || panic!(
                "未知投递不可再次领取"
            ))
            .unwrap()
            .is_none()
        );
        let second = live
            .claim_progress_reply(&scope, 7, fact("progress-2"), || Ok(true))
            .unwrap()
            .unwrap();
        second
            .complete(InputReplyDelivery::Sent {
                platform_message_id: "platform-2".into(),
            })
            .unwrap();
        assert_eq!(
            turn.take_update(false).unwrap().input_replies[0].event_id,
            "progress-2"
        );
        live.steer_anchored(&scope, 7, "新问题".into(), Some("new".into()), || Ok(()))
            .unwrap();
        assert!(!live.progress_is_current(&scope, 7));
        assert!(
            live.claim_progress_reply(&scope, 7, fact("stale"), || panic!(
                "新输入到来旧进展不能持久化"
            ))
            .unwrap()
            .is_none()
        );
        assert_eq!(turn.take_batch(false).unwrap().messages, ["新问题"]);
        turn.take_update(true).unwrap();
        assert!(!live.progress_is_current(&scope, 7));
        assert!(
            live.claim_progress_reply(&scope, 7, fact("closed"), || panic!("结束不能再发"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pending_progress_claim_requires_exact_unconsumed_input_and_preserves_unknown() {
        let live = LiveDialogue::default();
        let scope = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 1).unwrap();
        let fact = |event: &str| InputReplyHandoff {
            kind: InputReplyKind::Progress,
            input_message_id: "pending".into(),
            input_text: "继续核对".into(),
            event_id: event.into(),
            reply_text: "第一步返回了，下一步还在运行".into(),
            delivery: InputReplyDelivery::Sending,
        };
        assert!(!live.pending_progress_is_current(&scope, 1, "pending"));
        assert!(
            live.claim_pending_progress_reply(&scope, 1, fact("empty"), || panic!(
                "无输入不可领取"
            ))
            .unwrap()
            .is_none()
        );
        live.steer_anchored(
            &scope,
            1,
            "继续核对".into(),
            Some("pending".into()),
            || Ok(()),
        )
        .unwrap();
        assert!(live.pending_progress_is_current(&scope, 1, "pending"));
        assert!(
            !live.progress_is_current(&scope, 1),
            "普通进度的空输入合同保留"
        );
        assert!(
            live.claim_progress_reply(&scope, 1, fact("ordinary"), || panic!(
                "普通入口不得领取pending"
            ))
            .unwrap()
            .is_none()
        );
        let foreign = ContextScope {
            user_id: "other".into(),
            ..scope.clone()
        };
        for (candidate, id) in [(&foreign, 1), (&scope, 2)] {
            assert!(!live.pending_progress_is_current(candidate, id, "pending"));
            assert!(
                live.claim_pending_progress_reply(candidate, id, fact("foreign"), || panic!(
                    "跨身份或轮次"
                ))
                .unwrap()
                .is_none()
            );
        }
        let mut changed = fact("changed");
        changed.input_text = "伪造原话".into();
        assert!(
            live.claim_pending_progress_reply(&scope, 1, changed, || panic!("原话不匹配"))
                .unwrap()
                .is_none()
        );
        assert!(
            live.claim_pending_progress_reply(&scope, 1, fact("cancelled"), || Ok(false))
                .unwrap()
                .is_none()
        );
        assert!(
            live.claim_pending_progress_reply(&scope, 1, fact("save-error"), || Err(
                PluginError::State("保存失败".into())
            ))
            .is_err()
        );
        let claim = live
            .claim_pending_progress_reply(&scope, 1, fact("sent-once"), || Ok(true))
            .unwrap()
            .unwrap();
        assert_eq!(
            live.pending_anchor(&scope, 1).as_deref(),
            Some("pending"),
            "领取不消费业务输入"
        );
        claim.complete(InputReplyDelivery::Unknown).unwrap();
        assert!(
            live.claim_pending_progress_reply(&scope, 1, fact("sent-once"), || panic!(
                "unknown不得重放"
            ))
            .unwrap()
            .is_none()
        );
        live.steer_anchored(
            &scope,
            1,
            "补充新问题".into(),
            Some("newer".into()),
            || Ok(()),
        )
        .unwrap();
        assert!(!live.pending_progress_is_current(&scope, 1, "pending"));
        assert!(
            live.claim_pending_progress_reply(&scope, 1, fact("old-anchor"), || panic!(
                "新消息撤销旧锚点"
            ))
            .unwrap()
            .is_none()
        );
        let update = turn.take_update(false).unwrap();
        assert_eq!(update.batch.messages, ["继续核对", "补充新问题"]);
        assert_eq!(update.batch.review_anchor.as_deref(), Some("newer"));
        assert_eq!(update.input_replies.len(), 1);
        assert_eq!(update.input_replies[0].kind, InputReplyKind::Progress);
        assert_eq!(
            update.input_replies[0].delivery,
            InputReplyDelivery::Unknown
        );
        assert!(!live.pending_progress_is_current(&scope, 1, "newer"));
        let consumed = InputReplyHandoff {
            input_message_id: "newer".into(),
            input_text: "补充新问题".into(),
            ..fact("consumed")
        };
        assert!(
            live.claim_pending_progress_reply(&scope, 1, consumed, || panic!("已消费不能发送"))
                .unwrap()
                .is_none()
        );
        assert_eq!(turn.take_update(true).unwrap(), SteeringUpdate::default());
    }

    #[test]
    fn pending_progress_claim_and_input_consumption_share_the_persistence_boundary() {
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
            "继续核对".into(),
            Some("pending".into()),
            || Ok(()),
        )
        .unwrap();
        let (saving, saved) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let claimant = {
            let live = live.clone();
            let scope = scope.clone();
            thread::spawn(move || {
                live.claim_pending_progress_reply(
                    &scope,
                    1,
                    InputReplyHandoff {
                        kind: InputReplyKind::Progress,
                        input_message_id: "pending".into(),
                        input_text: "继续核对".into(),
                        event_id: "new-revision".into(),
                        reply_text: "第一步返回了".into(),
                        delivery: InputReplyDelivery::Sending,
                    },
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
            "不得消费半份持久化交接"
        );
        let update = consumption.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(update.batch.messages, ["继续核对"]);
        assert_eq!(update.input_replies.len(), 1);
        assert_eq!(update.input_replies[0].kind, InputReplyKind::Progress);
        assert_eq!(
            update.input_replies[0].delivery,
            InputReplyDelivery::Sending
        );
        claim
            .complete(InputReplyDelivery::Sent {
                platform_message_id: "platform-1".into(),
            })
            .unwrap();
        let final_update = turn.take_update(false).unwrap();
        assert_eq!(final_update.batch, SteeringBatch::default());
        assert_eq!(
            final_update.input_replies[0].delivery,
            InputReplyDelivery::Sent {
                platform_message_id: "platform-1".into()
            }
        );
        assert_eq!(turn.take_update(false).unwrap(), SteeringUpdate::default());
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
                kind: eve_llm_api::InputReplyKind::UserInput,
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
