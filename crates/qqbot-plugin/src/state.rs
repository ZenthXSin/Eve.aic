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
const SEGMENT_RESERVE: usize = 20 * 1024;
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
/// 通道只保存用户本次发送的附件引用；不会下载，也不等同于视觉解析结果。
const FILE_TYPES: [&str; 5] = [
    "text/plain",
    "application/pdf",
    "application/zip",
    "application/json",
    "application/octet-stream",
];
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageAttachment {
    pub content_type: String,
    pub url: String,
    /// 展示名；旧账本没有该字段，缺失时按没有文件名处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}
impl ImageAttachment {
    fn type_allowed(&self) -> bool {
        self.content_type
            .strip_prefix("image/")
            .is_some_and(|subtype| {
                !subtype.is_empty()
                    && subtype.len() <= 64
                    && subtype
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
            })
            || FILE_TYPES.contains(&self.content_type.as_str())
    }
    fn valid(&self) -> bool {
        self.type_allowed()
            && self.url.len() <= 4096
            && (self.url.is_empty()
                || (self.url.starts_with("https://")
                    && self.url[8..]
                        .split_once('/')
                        .is_some_and(|(host, _)| !host.is_empty())))
            && !self
                .url
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            && self.filename.as_ref().is_none_or(|name| {
                !name.is_empty()
                    && name.len() <= 255
                    && name.trim() == name
                    && !name
                        .chars()
                        .any(|c| c == '/' || c == '\\' || c.is_control())
            })
    }
    fn is_image(&self) -> bool {
        self.content_type.starts_with("image/")
    }
}
/// 被引用消息的文字与附件，和本条新发的附件分开保存。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotedMessage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ImageAttachment>,
}
impl QuotedMessage {
    fn valid(&self) -> bool {
        (!self.text.trim().is_empty() || !self.attachments.is_empty())
            && self.text.len() <= 1024
            && !self.text.chars().any(char::is_control)
            && self.attachments.len() <= 4
            && self.attachments.iter().all(ImageAttachment::valid)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: String,
    pub scope: String,
    pub target_id: String,
    pub user_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ImageAttachment>,
    /// 用户引用的上一条消息；旧账本没有该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<QuotedMessage>,
}
impl Message {
    pub fn turn_text(&self) -> String {
        let mut text = self.text.clone();
        if let Some(quoted) = &self.quoted {
            let mut line = String::from("【引用的消息】");
            if !quoted.text.is_empty() {
                line.push_str(&quoted.text);
            }
            if !quoted.attachments.is_empty() {
                line.push_str(&format!("（含 {} 个附件引用）", quoted.attachments.len()));
            }
            text = format!("{line}\n\n{text}");
        }
        if self.attachments.is_empty() {
            return text;
        }
        let images = self.attachments.iter().filter(|a| a.is_image()).count();
        let files = self.attachments.len() - images;
        let mut note = String::from("【通道附件信息】");
        if images > 0 {
            note.push_str(&format!("收到 {images} 个图片附件，已保存引用，尚未获取像素或进行视觉解析。不能声称已经看见或推测图片内容。"));
        }
        if files > 0 {
            let names: Vec<&str> = self
                .attachments
                .iter()
                .filter(|item| !item.is_image())
                .filter_map(|item| item.filename.as_deref())
                .collect();
            note.push_str(&format!(
                "收到 {files} 个文件附件，只保存了类型{}，未下载内容。",
                if names.is_empty() {
                    String::new()
                } else {
                    format!("（{}）", names.join("、"))
                }
            ));
        }
        note.push_str(
            "需要看文件内容时调用 read_attachment，index 从 1 起，只下载本条已记录的附件；             图片没有视觉解析，下载也只能看到类型和大小。",
        );
        format!("{text}\n\n{note}")
    }
    /// 本条消息第 `index` 个附件（从 1 起）；越界返回 None。
    pub fn attachment(&self, index: usize) -> Option<&ImageAttachment> {
        index.checked_sub(1).and_then(|i| self.attachments.get(i))
    }
    pub fn valid(&self) -> bool {
        [&self.target_id, &self.user_id]
            .into_iter()
            .all(|v| valid_id(v))
            && valid_message_id(&self.id)
            && matches!(self.scope.as_str(), "c2c" | "group")
            && (self.scope != "c2c" || self.target_id == self.user_id)
            && (!self.text.trim().is_empty() || !self.attachments.is_empty())
            && self.text.len() <= 32768
            && self.attachments.len() <= 4
            && self.attachments.iter().all(ImageAttachment::valid)
            && self.quoted.as_ref().is_none_or(QuotedMessage::valid)
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
    pub fn conversation_key(&self, app_id: &str) -> PluginResult<String> {
        let identity =
            serde_json::to_vec(&(app_id, &self.scope, &self.target_id)).map_err(|_| corrupt())?;
        let mut key = String::from("qq:");
        for byte in digest(&SHA256, &identity).as_ref() {
            use std::fmt::Write;
            write!(&mut key, "{byte:02x}").expect("writing a String cannot fail");
        }
        Ok(key)
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress: Vec<Progress>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub text: String,
    pub state: PartState,
}
/// 格式 1 不含片段，2 包含分段，3 包含途中进展。旧版本拒绝读取而不是忽略投递记录。
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
                || entry.progress.len() > 3
                || entry.progress.iter().any(|p| {
                    p.text.trim().is_empty()
                        || p.text.len() > 1024
                        || !matches!(
                            p.state,
                            PartState::Sending | PartState::Sent | PartState::Failed
                        )
                })
                || entry.progress.len() + entry.segments.as_ref().map_or(1, |s| s.parts.len())
                    > crate::QQ_MAX_SEGMENTS
            {
                return Err(corrupt());
            }
        }
        Ok(ledger)
    }
    fn format(&self) -> u8 {
        if self.entries.iter().any(|e| !e.progress.is_empty()) {
            3
        } else if self.entries.iter().any(|e| e.segments.is_some()) {
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
            progress: vec![],
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
                attachments: vec![],
                quoted: None,
                id: "msg-1".into(),
                scope: "c2c".into(),
                target_id: "user-1".into(),
                user_id: "user-1".into(),
                text: "你好".into(),
            },
            state,
            progress: vec![],
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
    fn progress_round_trips_without_becoming_a_success_and_preserves_unknown_delivery() {
        for state in [Sending, Sent, Failed] {
            let mut entry = receipt(ReceiptState::Processing, None);
            entry.progress.push(Progress {
                text: "资料检索已返回结果。".into(),
                state,
            });
            let ledger = round_trip(entry).unwrap();
            assert_eq!(ledger.version, 3);
            assert_eq!(ledger.entries[0].state, ReceiptState::Processing);
            assert_eq!(ledger.entries[0].progress[0].state, state);
            assert!(ledger.entries[0].reply.is_none());
        }
        let mut entry = receipt(ReceiptState::Sent, Some(&[Sent, Sent, Sent]));
        for _ in 0..3 {
            entry.progress.push(Progress {
                text: "回执".into(),
                state: Sent,
            });
        }
        assert!(round_trip(entry).is_err(), "进度加分段不得突破 QQ 五次配额");
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
