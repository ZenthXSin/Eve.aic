use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionKey;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

const STATE_KEY: &str = "receipts.v1";
pub(crate) const MAX_BYTES: usize = 1_048_576;
const MAX_RECORDS: usize = 4096;
// ControlInput.task_id adds "qq:" to the platform message ID (256-byte limit).
const MAX_MESSAGE_ID_BYTES: usize = 253;
// 至多 8 个片段的范围与状态及规划器标识序列化后的上限。
const SEGMENT_RESERVE: usize = 512;
pub(crate) fn valid_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub(crate) fn valid_message_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= MAX_MESSAGE_ID_BYTES
        && v.trim() == v
        && !v.chars().any(char::is_control)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Message {
    pub id: String,
    pub scope: String,
    pub target_id: String,
    pub user_id: String,
    pub text: String,
}
impl Message {
    pub fn valid(&self) -> bool {
        [&self.target_id, &self.user_id]
            .into_iter()
            .all(|v| valid_id(v))
            && valid_message_id(&self.id)
            && matches!(self.scope.as_str(), "c2c" | "group")
            && (self.scope != "c2c" || self.target_id == self.user_id)
            && !self.text.trim().is_empty()
            && self.text.len() <= 32768
    }
    /// 首版临时会话映射；未来统一认知时只替换此策略并迁移旧历史。
    pub fn session_key(&self, app_id: &str) -> PluginResult<SessionKey> {
        let identity = serde_json::to_vec(&(app_id, &self.scope, &self.target_id, &self.user_id))
            .map_err(|_| corrupt())?;
        let mut key = String::from("qq:");
        for byte in digest(&SHA256, &identity).as_ref() {
            use std::fmt::Write;
            write!(&mut key, "{byte:02x}").expect("writing a String cannot fail");
        }
        SessionKey::new(key.clone(), key).map_err(|_| corrupt())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ReceiptState {
    Processing,
    ReplyPending,
    Sent,
    Failed,
}
/// 单个片段的投递状态。Sending 在写出前保存；重启后表示是否送达不确定，不会补发。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PartState {
    Pending,
    Sending,
    Sent,
    Failed,
    Skipped,
}
/// 片段是完整回复中的字节范围，不复制正文；完整回复仍是唯一的回复事实。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Part {
    pub start: usize,
    pub end: usize,
    pub state: PartState,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Segments {
    pub planner: String,
    pub parts: Vec<Part>,
}
impl Segments {
    /// 未发出的片段全部跳过；只用于确认没有片段正在写出的位置。
    pub fn skip_rest(&mut self) {
        for part in &mut self.parts {
            if matches!(part.state, PartState::Pending | PartState::Sending) {
                part.state = PartState::Skipped;
            }
        }
    }
    /// 片段状态必须是“已发送前缀 + 至多一个当前片段 + 同类剩余”。
    fn consistent(&self, state: ReceiptState) -> bool {
        let sent = self
            .parts
            .iter()
            .take_while(|p| p.state == PartState::Sent)
            .count();
        let rest = &self.parts[sent..];
        let tail = |current: PartState, remaining: PartState| {
            let tail = match rest.first() {
                Some(p) if p.state == current => &rest[1..],
                _ => rest,
            };
            tail.iter().all(|p| p.state == remaining)
        };
        match state {
            ReceiptState::Processing => false,
            ReceiptState::Sent => rest.is_empty(),
            ReceiptState::ReplyPending => {
                !rest.is_empty() && tail(PartState::Sending, PartState::Pending)
            }
            ReceiptState::Failed => !rest.is_empty() && tail(PartState::Failed, PartState::Skipped),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub app_id: String,
    pub message: Message,
    pub state: ReceiptState,
    pub reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segments: Option<Segments>,
}
/// 格式 1 不含片段；出现分段回执时整体写为格式 2，旧版本拒绝读取而不是忽略片段。
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    pub version: u8,
    pub entries: Vec<Receipt>,
}
impl Ledger {
    pub fn load(ctx: &PluginContext) -> PluginResult<Self> {
        let Some(bytes) = ctx.state_get(STATE_KEY)? else {
            return Ok(Self {
                version: 1,
                entries: Vec::new(),
            });
        };
        if bytes.len() > MAX_BYTES {
            return Err(corrupt());
        }
        let ledger: Self = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
        if ledger.version != ledger.format() || ledger.entries.len() > MAX_RECORDS {
            return Err(corrupt());
        }
        let mut identities = std::collections::BTreeSet::new();
        for entry in &ledger.entries {
            if !valid_id(&entry.app_id)
                || !entry.message.valid()
                || !identities.insert((&entry.app_id, &entry.message.id))
                || entry
                    .reply
                    .as_ref()
                    .is_some_and(|r| r.trim().is_empty() || r.len() > 32768)
                || (matches!(entry.state, ReceiptState::ReplyPending | ReceiptState::Sent)
                    && entry.reply.is_none())
                || entry.segments.as_ref().is_some_and(|segments| {
                    !valid_segments(segments, entry.reply.as_deref(), entry.state)
                })
            {
                return Err(corrupt());
            }
        }
        Ok(ledger)
    }
    fn format(&self) -> u8 {
        if self.entries.iter().any(|e| e.segments.is_some()) {
            2
        } else {
            1
        }
    }
    fn encode(&mut self) -> PluginResult<Vec<u8>> {
        self.version = self.format();
        serde_json::to_vec(self).map_err(|_| corrupt())
    }
    pub fn find(&self, app: &str, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|r| r.app_id == app && r.message.id == id)
    }
    pub fn insert(
        &mut self,
        ctx: &PluginContext,
        app: &str,
        message: Message,
    ) -> PluginResult<bool> {
        if self.entries.len() >= MAX_RECORDS {
            return Ok(false);
        }
        self.entries.push(Receipt {
            app_id: app.into(),
            message,
            state: ReceiptState::Processing,
            reply: None,
            segments: None,
        });
        let bytes = self.encode()?;
        // 预留最大回复转义后的大小与片段记录。
        if bytes.len() + 6 * 32768 + 64 + SEGMENT_RESERVE > MAX_BYTES {
            self.entries.pop();
            return Ok(false);
        }
        ctx.state_set(STATE_KEY, bytes)?;
        Ok(true)
    }
    pub fn save(&mut self, ctx: &PluginContext) -> PluginResult<()> {
        let bytes = self.encode()?;
        if bytes.len() > MAX_BYTES {
            return Err(PluginError::State(
                "QQBot 回执容量不足，保留既有状态".into(),
            ));
        }
        ctx.state_set(STATE_KEY, bytes)
    }
}
fn valid_segments(segments: &Segments, reply: Option<&str>, state: ReceiptState) -> bool {
    let Some(reply) = reply else {
        return false;
    };
    (2..=eve_segment_api::MAX_SEGMENTS).contains(&segments.parts.len())
        && !segments.planner.is_empty()
        && segments.planner.len() <= 64
        && segments
            .planner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && eve_segment_api::validate_ranges(
            reply,
            segments.parts.iter().map(|p| p.start..p.end),
            32768,
        )
        .is_ok()
        && segments.consistent(state)
}
fn corrupt() -> PluginError {
    PluginError::State("QQBot 回执状态损坏或版本不兼容；未清空".into())
}
