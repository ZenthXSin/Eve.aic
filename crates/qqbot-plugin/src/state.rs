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
        match ctx.state_get(STATE_KEY)? {
            None => Ok(Self {
                version: 1,
                entries: Vec::new(),
            }),
            Some(bytes) => Self::decode(&bytes),
        }
    }
    /// 严格解析；任何不一致都视为损坏，调用方保留原字节。
    fn decode(bytes: &[u8]) -> PluginResult<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(corrupt());
        }
        let ledger: Self = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use PartState::*;

    const REPLY: &str = "第一段。\n\n第二段。\n\n第三段。";
    fn receipt(state: ReceiptState, parts: Option<&[PartState]>) -> Receipt {
        let ranges = [(0, 12), (14, 26), (28, 40)];
        Receipt {
            app_id: "1904159860".into(),
            message: Message {
                id: "msg-1".into(),
                scope: "c2c".into(),
                target_id: "user-1".into(),
                user_id: "user-1".into(),
                text: "你好".into(),
            },
            state,
            reply: (state != ReceiptState::Processing).then(|| REPLY.into()),
            segments: parts.map(|parts| Segments {
                planner: "paragraph-v1".into(),
                parts: parts
                    .iter()
                    .zip(ranges)
                    .map(|(state, (start, end))| Part {
                        start,
                        end,
                        state: *state,
                    })
                    .collect(),
            }),
        }
    }
    fn round_trip(entry: Receipt) -> PluginResult<Ledger> {
        let mut ledger = Ledger {
            version: 0,
            entries: vec![entry],
        };
        Ledger::decode(&ledger.encode()?)
    }

    #[test]
    fn every_state_the_channel_can_save_reloads() {
        assert_eq!(&REPLY[0..12], "第一段。");
        assert_eq!(&REPLY[28..40], "第三段。");
        let reachable: [(ReceiptState, &[PartState]); 9] = [
            // 首段写出前、段间停顿、末段在途。
            (ReceiptState::ReplyPending, &[Sending, Pending, Pending]),
            (ReceiptState::ReplyPending, &[Sent, Pending, Pending]),
            (ReceiptState::ReplyPending, &[Sent, Sent, Sending]),
            (ReceiptState::Sent, &[Sent, Sent, Sent]),
            // 片段失败、首段写出前旧代失效、段间旧代失效。
            (ReceiptState::Failed, &[Sent, Failed, Skipped]),
            (ReceiptState::Failed, &[Failed, Skipped, Skipped]),
            (ReceiptState::Failed, &[Skipped, Skipped, Skipped]),
            (ReceiptState::Failed, &[Sent, Skipped, Skipped]),
            (ReceiptState::Failed, &[Sent, Sent, Skipped]),
        ];
        for (state, parts) in reachable {
            let ledger = round_trip(receipt(state, Some(parts)))
                .unwrap_or_else(|_| panic!("{state:?} {parts:?} must reload"));
            assert_eq!(ledger.version, 2);
        }
        let plain = round_trip(receipt(ReceiptState::Sent, None)).unwrap();
        assert_eq!(plain.version, 1);
    }

    #[test]
    fn inconsistent_progress_or_format_is_corrupt() {
        let invalid: [(ReceiptState, &[PartState]); 8] = [
            (ReceiptState::Processing, &[Pending, Pending, Pending]),
            (ReceiptState::Sent, &[Sent, Sent, Pending]),
            (ReceiptState::ReplyPending, &[Sent, Sent, Sent]),
            (ReceiptState::ReplyPending, &[Sent, Skipped, Pending]),
            (ReceiptState::ReplyPending, &[Sending, Sending, Pending]),
            (ReceiptState::Failed, &[Sent, Pending, Skipped]),
            (ReceiptState::Failed, &[Sent, Failed, Failed]),
            (ReceiptState::Failed, &[Skipped, Sent, Skipped]),
        ];
        for (state, parts) in invalid {
            assert!(
                round_trip(receipt(state, Some(parts))).is_err(),
                "{state:?} {parts:?} must be rejected"
            );
        }
        let single: [PartState; 1] = [Sent];
        assert!(round_trip(receipt(ReceiptState::Sent, Some(&single))).is_err());
        let mut v1_with_parts = Ledger {
            version: 0,
            entries: vec![receipt(ReceiptState::Sent, Some(&[Sent, Sent, Sent]))],
        };
        let mut bytes: serde_json::Value =
            serde_json::from_slice(&v1_with_parts.encode().unwrap()).unwrap();
        bytes["version"] = 1.into();
        assert!(Ledger::decode(&serde_json::to_vec(&bytes).unwrap()).is_err());
        let mut v2_without = Ledger {
            version: 0,
            entries: vec![receipt(ReceiptState::Sent, None)],
        };
        let mut bytes: serde_json::Value =
            serde_json::from_slice(&v2_without.encode().unwrap()).unwrap();
        bytes["version"] = 2.into();
        assert!(Ledger::decode(&serde_json::to_vec(&bytes).unwrap()).is_err());
        // 范围切开字符、遗漏原文或不在完整回复内都拒绝。
        for (start, end) in [(1, 12), (0, 11), (0, 13)] {
            let mut entry = receipt(ReceiptState::Sent, Some(&[Sent, Sent, Sent]));
            let part = &mut entry.segments.as_mut().unwrap().parts[0];
            (part.start, part.end) = (start, end);
            assert!(round_trip(entry).is_err(), "{start}..{end}");
        }
    }
}
