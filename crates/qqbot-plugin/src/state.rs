use crate::{QqArchivedReceipt, QqReceiptArchive};
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionKey;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const STATE_KEY: &str = "receipts.v1";
// 保留全部去重与投递证据；容量不足不能淘汰旧回执后重新执行。
pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const ARCHIVE_TRIGGER: usize = 10 * 1024 * 1024;
const HOT_TARGET: usize = 1024 * 1024;
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
const FILE_TYPES: [&str; 6] = [
    "text/plain",
    "application/pdf",
    "application/zip",
    "application/json",
    "application/octet-stream",
    "file",
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
    /// 连发的消息并入同一会话稍后的一条，由那一条统一回复；本条不单独回复。
    Merged,
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
            ReceiptState::Merged => false,
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
    /// 状态为 Merged 时，统一回复的那条消息 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
    /// 正文发完后附带发出的表情包；失败或中断不重发。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticker: Option<StickerPart>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StickerPart {
    /// 宿主表情包库里的本地只读路径。
    pub path: String,
    pub state: PartState,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub text: String,
    pub state: PartState,
}
/// 格式 1 不含片段，2 包含分段，3 包含途中进展，4 包含合并的连发消息，5 包含附带的表情包。
/// 旧版本拒绝读取而不是忽略投递记录。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveBinding {
    identity: String,
    records: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    pub version: u8,
    pub entries: Vec<Receipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<ArchiveBinding>,
    #[serde(skip)]
    backend: Option<Arc<dyn QqReceiptArchive>>,
    #[serde(skip)]
    encoded_bytes: usize,
}
impl Ledger {
    pub fn load(ctx: &PluginContext) -> PluginResult<Self> {
        match ctx.state_get(STATE_KEY)? {
            None => Ok(Self {
                version: 1,
                entries: Vec::new(),
                ..Self::default()
            }),
            Some(bytes) => Self::decode(&bytes),
        }
    }
    /// 严格解析；任何不一致都视为损坏，调用方保留原字节。
    fn decode(bytes: &[u8]) -> PluginResult<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(corrupt());
        }
        let mut ledger: Self = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
        if ledger.version != ledger.format()
            || ledger.entries.len() > MAX_RECORDS
            || ledger.archive.as_ref().is_some_and(|a| {
                a.identity.len() != 32
                    || !a.identity.bytes().all(|b| b.is_ascii_hexdigit())
                    || a.records == 0
            })
        {
            return Err(corrupt());
        }
        let mut identities = std::collections::BTreeSet::new();
        for (index, entry) in ledger.entries.iter().enumerate() {
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
                || entry.progress.len()
                    + entry.segments.as_ref().map_or(1, |s| s.parts.len())
                    + usize::from(entry.sticker.is_some())
                    > crate::QQ_MAX_SEGMENTS
                || entry.sticker.as_ref().is_some_and(|sticker| {
                    sticker.path.is_empty()
                        || sticker.path.len() > 4096
                        || sticker.path.chars().any(char::is_control)
                        || !matches!(entry.state, ReceiptState::ReplyPending | ReceiptState::Sent | ReceiptState::Failed)
                        // 正文没有发完时表情包只能还在等待；正文失败时表情包从未写出。
                        || (entry.state == ReceiptState::ReplyPending && sticker.state != PartState::Pending)
                        || (entry.state == ReceiptState::Failed
                            && !matches!(sticker.state, PartState::Pending | PartState::Skipped))
                })
                || (entry.state == ReceiptState::Merged) != entry.merged_into.is_some()
                || (entry.state == ReceiptState::Merged
                    && (entry.reply.is_some()
                        || entry.segments.is_some()
                        || !entry.progress.is_empty()))
                || entry.merged_into.as_ref().is_some_and(|into| {
                    into == &entry.message.id
                        || !ledger.entries.iter().enumerate().any(|(target_index, e)| {
                            target_index > index
                                && e.app_id == entry.app_id
                                && &e.message.id == into
                                && e.state != ReceiptState::Merged
                                && e.message.scope == entry.message.scope
                                && e.message.target_id == entry.message.target_id
                                && e.message.user_id == entry.message.user_id
                        })
                })
            {
                return Err(corrupt());
            }
        }
        ledger.encoded_bytes = bytes.len();
        Ok(ledger)
    }
    fn format(&self) -> u8 {
        if self.archive.is_some() {
            6
        } else if self.entries.iter().any(|e| e.sticker.is_some()) {
            5
        } else if self.entries.iter().any(|e| e.merged_into.is_some()) {
            4
        } else if self.entries.iter().any(|e| !e.progress.is_empty()) {
            3
        } else if self.entries.iter().any(|e| e.segments.is_some()) {
            2
        } else {
            1
        }
    }
    fn encode(&mut self) -> PluginResult<Vec<u8>> {
        self.version = self.format();
        let bytes = serde_json::to_vec(self).map_err(|_| corrupt())?;
        self.encoded_bytes = bytes.len();
        Ok(bytes)
    }
    pub fn find(&self, app: &str, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|r| r.app_id == app && r.message.id == id)
    }

    pub fn attach_archive(
        &mut self,
        backend: Option<Arc<dyn QqReceiptArchive>>,
    ) -> PluginResult<()> {
        if let Some(binding) = &self.archive {
            let (id, count) = backend.as_ref().ok_or_else(corrupt)?.identity()?;
            if id != binding.identity || count < binding.records {
                return Err(corrupt());
            }
        }
        self.backend = backend;
        Ok(())
    }

    pub fn contains(&self, app: &str, id: &str) -> PluginResult<bool> {
        if self.find(app, id).is_some() {
            return Ok(true);
        }
        match &self.backend {
            Some(db) => Ok(db.get(app, id)?.is_some()),
            None => Ok(false),
        }
    }

    pub fn archived(&self, app: &str, id: &str) -> PluginResult<Option<Receipt>> {
        self.backend
            .as_ref()
            .map(|db| db.get(app, id))
            .transpose()?
            .flatten()
            .map(|bytes| decode_archived(&bytes))
            .transpose()
            .and_then(|entry| {
                if entry
                    .as_ref()
                    .is_some_and(|e| e.app_id != app || e.message.id != id)
                {
                    Err(corrupt())
                } else {
                    Ok(entry)
                }
            })
    }

    pub fn last_failed(&self, app: &str, message: &Message) -> PluginResult<Option<String>> {
        let session = message.session_key(app)?;
        if let Some(entry) = self.entries.iter().rev().find(|entry| {
            entry.app_id == app
                && entry.message.session_key(app).ok().as_ref() == Some(&session)
                && entry.message.text.trim() != "/last-reply"
                && entry.state == ReceiptState::Failed
                && entry.reply.is_some()
        }) {
            return Ok(entry.reply.clone());
        }
        let bytes = self
            .backend
            .as_ref()
            .map(|db| db.latest_failed(app, &session.session_id))
            .transpose()?
            .flatten();
        let entry = bytes.map(|b| decode_archived(&b)).transpose()?;
        if entry.as_ref().is_some_and(|e| {
            e.app_id != app
                || e.message.session_key(app).ok().as_ref() != Some(&session)
                || e.state != ReceiptState::Failed
                || e.reply.is_none()
                || e.message.text.trim() == "/last-reply"
        }) {
            return Err(corrupt());
        }
        Ok(entry.and_then(|e| e.reply))
    }

    /// 仅在循环边界归档；提交数据库后才缩小活跃账本，失败恢复内存且保留两边原数据。
    pub fn archive_if_needed(&mut self, ctx: &PluginContext) -> PluginResult<()> {
        if self.encoded_bytes <= ARCHIVE_TRIGGER {
            return Ok(());
        }
        self.archive_with(|bytes| ctx.state_set(STATE_KEY, bytes))
    }
    fn archive_with(
        &mut self,
        persist: impl FnOnce(Vec<u8>) -> PluginResult<()>,
    ) -> PluginResult<()> {
        if self.encode()?.len() <= ARCHIVE_TRIGGER {
            return Ok(());
        }
        let Some(db) = self.backend.clone() else {
            return Ok(());
        };
        let mut keep = vec![false; self.entries.len()];
        let mut latest = std::collections::BTreeSet::new();
        let mut confirmed = std::collections::BTreeSet::new();
        let mut bytes = 256usize;
        for (i, e) in self.entries.iter().enumerate().rev() {
            let identity = (
                &e.app_id,
                &e.message.scope,
                &e.message.target_id,
                &e.message.user_id,
            );
            let latest_session = latest.insert(identity);
            let latest_confirmed = e.state == ReceiptState::Sent && confirmed.insert(identity);
            let pinned = matches!(
                e.state,
                ReceiptState::Processing | ReceiptState::ReplyPending
            ) || e.progress.iter().any(|p| p.state == PartState::Sending)
                || e.sticker
                    .as_ref()
                    .is_some_and(|s| matches!(s.state, PartState::Pending | PartState::Sending))
                || latest_session
                || latest_confirmed;
            let size = serde_json::to_vec(e).map_err(|_| corrupt())?.len() + 1;
            let recent = bytes + size <= HOT_TARGET;
            bytes += size;
            if pinned || recent {
                keep[i] = true;
            }
        }
        // 合并组整体留热区或整体归档；不能留下指向冷区的 Merged 引用。
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(target) = &e.merged_into {
                let j = self.find(&e.app_id, target).ok_or_else(corrupt)?;
                if keep[i] || keep[j] {
                    keep[i] = true;
                    keep[j] = true;
                }
            }
        }
        // 同组另一条可能刚刚把目标钉住，再统一保留其所有成员。
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(target) = &e.merged_into
                && keep[self.find(&e.app_id, target).ok_or_else(corrupt)?]
            {
                keep[i] = true;
            }
        }
        let records: Vec<_> = self
            .entries
            .iter()
            .zip(&keep)
            .filter(|(_, keep)| !**keep)
            .map(|(e, _)| {
                Ok(QqArchivedReceipt {
                    app_id: e.app_id.clone(),
                    message_id: e.message.id.clone(),
                    session_id: e.message.session_key(&e.app_id)?.session_id,
                    failed_reply: e.state == ReceiptState::Failed
                        && e.reply.is_some()
                        && e.message.text.trim() != "/last-reply",
                    payload: serde_json::to_vec(e).map_err(|_| corrupt())?,
                })
            })
            .collect::<PluginResult<_>>()?;
        if records.is_empty() {
            return Ok(());
        }
        let count = db.store(&records)?;
        let (id, confirmed_count) = db.identity()?;
        if count != confirmed_count {
            return Err(corrupt());
        }
        let old_binding = self.archive.clone();
        let old_version = self.version;
        let old_bytes = self.encoded_bytes;
        // 通过字节还原避免更改原记录；失败可完整还原旧内存快照。
        let retained = self
            .entries
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(e, _)| {
                serde_json::to_vec(e)
                    .and_then(|b| serde_json::from_slice(&b))
                    .map_err(|_| corrupt())
            })
            .collect::<PluginResult<_>>()?;
        let previous = std::mem::replace(&mut self.entries, retained);
        self.archive = Some(ArchiveBinding {
            identity: id,
            records: count,
        });
        let result = self.encode().and_then(persist);
        if result.is_err() {
            self.entries = previous;
            self.archive = old_binding;
            self.version = old_version;
            self.encoded_bytes = old_bytes;
        }
        result
    }

    /// 整组一次提交：任意持久快照中，Merged 都必须已经有同组的目标回执。
    pub fn insert_merged(
        &mut self,
        ctx: &PluginContext,
        app: &str,
        messages: &[Message],
    ) -> PluginResult<bool> {
        self.insert_merged_with(app, messages, |bytes| ctx.state_set(STATE_KEY, bytes))
    }

    fn insert_merged_with(
        &mut self,
        app: &str,
        messages: &[Message],
        persist: impl FnOnce(Vec<u8>) -> PluginResult<()>,
    ) -> PluginResult<bool> {
        let Some(target) = messages.last() else {
            return Err(corrupt());
        };
        let mut ids = std::collections::BTreeSet::new();
        if messages.len() < 2
            || !valid_id(app)
            || messages.iter().any(|message| {
                !message.valid()
                    || !ids.insert(&message.id)
                    || self.find(app, &message.id).is_some()
                    || message.scope != target.scope
                    || message.target_id != target.target_id
                    || message.user_id != target.user_id
            })
        {
            return Err(corrupt());
        }
        if messages.len() > MAX_RECORDS.saturating_sub(self.entries.len()) {
            return Ok(false);
        }
        let old_len = self.entries.len();
        let old_version = self.version;
        for (index, message) in messages.iter().enumerate() {
            let merged = index + 1 < messages.len();
            self.entries.push(Receipt {
                app_id: app.into(),
                message: message.clone(),
                state: if merged {
                    ReceiptState::Merged
                } else {
                    ReceiptState::Processing
                },
                reply: None,
                segments: None,
                progress: vec![],
                merged_into: merged.then(|| target.id.clone()),
                sticker: None,
            });
        }
        let result = (|| {
            let bytes = self.encode()?;
            if bytes.len() + 6 * 32768 + 64 + SEGMENT_RESERVE > MAX_BYTES {
                return Ok(false);
            }
            persist(bytes)?;
            Ok(true)
        })();
        if !matches!(result, Ok(true)) {
            self.entries.truncate(old_len);
            self.version = old_version;
        }
        result
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
            merged_into: None,
            sticker: None,
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
pub(crate) fn decode_archived(bytes: &[u8]) -> PluginResult<Receipt> {
    if bytes.len() > 512 * 1024 {
        return Err(corrupt());
    }
    let entry: Receipt = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
    if entry.state == ReceiptState::Merged {
        if !valid_id(&entry.app_id)
            || !entry.message.valid()
            || entry.reply.is_some()
            || entry.segments.is_some()
            || !entry.progress.is_empty()
            || entry.sticker.is_some()
            || entry
                .merged_into
                .as_ref()
                .is_none_or(|id| !valid_message_id(id) || id == &entry.message.id)
        {
            return Err(corrupt());
        }
    } else {
        let mut single = Ledger {
            entries: vec![serde_json::from_slice(bytes).map_err(|_| corrupt())?],
            ..Ledger::default()
        };
        Ledger::decode(&single.encode()?)?;
    }
    Ok(entry)
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
            merged_into: None,
            sticker: None,
        }
    }
    fn round_trip(entry: Receipt) -> PluginResult<Ledger> {
        let mut ledger = Ledger {
            version: 0,
            entries: vec![entry],
            ..Ledger::default()
        };
        Ledger::decode(&ledger.encode()?)
    }

    #[test]
    fn stickers_bump_the_format_and_follow_the_text_delivery_state() {
        let with = |state, sticker| {
            let mut entry = receipt(state, None);
            entry.sticker = Some(StickerPart {
                path: "/state/stickers/blobs/a.png".into(),
                state: sticker,
            });
            round_trip(entry)
        };
        assert_eq!(with(ReceiptState::Sent, Sent).unwrap().version, 5);
        for (state, sticker) in [
            (ReceiptState::ReplyPending, Pending),
            (ReceiptState::Sent, Pending),
            (ReceiptState::Sent, Sending),
            (ReceiptState::Sent, Failed),
            (ReceiptState::Failed, Pending),
            (ReceiptState::Failed, Skipped),
        ] {
            assert!(
                with(state, sticker).is_ok(),
                "{state:?} {sticker:?} must reload"
            );
        }
        for (state, sticker) in [
            (ReceiptState::ReplyPending, Sending),
            (ReceiptState::Failed, Sent),
            (ReceiptState::Processing, Pending),
        ] {
            assert!(
                with(state, sticker).is_err(),
                "{state:?} {sticker:?} must be rejected"
            );
        }
        let mut entry = receipt(ReceiptState::Sent, None);
        entry.sticker = Some(StickerPart {
            path: String::new(),
            state: Sent,
        });
        assert!(round_trip(entry).is_err());
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
            ..Ledger::default()
        };
        let mut bytes: serde_json::Value =
            serde_json::from_slice(&v1_with_parts.encode().unwrap()).unwrap();
        bytes["version"] = 1.into();
        assert!(Ledger::decode(&serde_json::to_vec(&bytes).unwrap()).is_err());
        let mut v2_without = Ledger {
            version: 0,
            entries: vec![receipt(ReceiptState::Sent, None)],
            ..Ledger::default()
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

    #[test]
    fn merged_receipts_need_a_live_target_and_bump_the_format() {
        let mut target = receipt(ReceiptState::Sent, None);
        target.message.id = "msg-2".into();
        let mut merged = receipt(ReceiptState::Merged, None);
        merged.reply = None;
        merged.merged_into = Some("msg-2".into());
        let mut ledger = Ledger {
            version: 0,
            entries: vec![merged, target],
            ..Ledger::default()
        };
        let bytes = ledger.encode().unwrap();
        let decoded = Ledger::decode(&bytes).unwrap();
        assert_eq!(decoded.version, 4);
        for broken in [
            |l: &mut Ledger| l.entries[0].merged_into = Some("missing".into()),
            |l: &mut Ledger| l.entries[0].merged_into = None,
            |l: &mut Ledger| l.entries[0].reply = Some("回复".into()),
            |l: &mut Ledger| l.entries[1].merged_into = Some("msg-1".into()),
        ] {
            let mut copy = Ledger::decode(&bytes).unwrap();
            broken(&mut copy);
            let encoded = serde_json::to_vec(&copy).unwrap();
            assert!(Ledger::decode(&encoded).is_err());
        }
    }

    #[test]
    fn merged_batch_is_one_readable_commit_and_write_failure_keeps_the_old_ledger() {
        for fail in [false, true] {
            let mut ledger = Ledger::default();
            let before = ledger.encode().unwrap();
            let first = receipt(ReceiptState::Processing, None).message;
            let mut last = first.clone();
            last.id = "msg-2".into();
            let mut writes = 0;
            let result = ledger.insert_merged_with("app", &[first, last], |bytes| {
                writes += 1;
                let saved = Ledger::decode(&bytes).unwrap();
                assert_eq!(saved.entries[0].state, ReceiptState::Merged);
                assert_eq!(saved.entries[1].state, ReceiptState::Processing);
                if fail {
                    Err(PluginError::State("synthetic write failure".into()))
                } else {
                    Ok(())
                }
            });
            assert_eq!(writes, 1);
            if fail {
                assert!(result.is_err());
                assert_eq!(ledger.encode().unwrap(), before);
            } else {
                assert!(result.unwrap());
                Ledger::decode(&ledger.encode().unwrap()).unwrap();
            }
        }
    }
    #[derive(Default)]
    struct ArchiveMemory {
        rows: std::sync::Mutex<std::collections::BTreeMap<(String, String), Vec<u8>>>,
        fail: std::sync::atomic::AtomicBool,
    }
    impl QqReceiptArchive for ArchiveMemory {
        fn identity(&self) -> PluginResult<(String, u64)> {
            Ok(("a".repeat(32), self.rows.lock().unwrap().len() as u64))
        }
        fn store(&self, records: &[QqArchivedReceipt]) -> PluginResult<u64> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(corrupt());
            }
            let mut rows = self.rows.lock().unwrap();
            for r in records {
                let key = (r.app_id.clone(), r.message_id.clone());
                if rows.get(&key).is_some_and(|b| b != &r.payload) {
                    return Err(corrupt());
                }
                rows.insert(key, r.payload.clone());
            }
            Ok(rows.len() as u64)
        }
        fn get(&self, app: &str, id: &str) -> PluginResult<Option<Vec<u8>>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(app.into(), id.into()))
                .cloned())
        }
        fn latest_failed(&self, _app: &str, _session: &str) -> PluginResult<Option<Vec<u8>>> {
            Ok(None)
        }
    }
    #[test]
    fn archive_commits_before_hot_shrink_and_retry_preserves_unknown_receipts() {
        let db = Arc::new(ArchiveMemory::default());
        let mut ledger = Ledger::default();
        for i in 0..350 {
            let mut entry = receipt(ReceiptState::Sent, None);
            entry.message.id = format!("message-{i}");
            entry.reply = Some("x".repeat(32768));
            ledger.entries.push(entry);
        }
        ledger.entries[0].state = ReceiptState::Processing;
        ledger.entries[0].reply = None;
        ledger.entries[1].state = ReceiptState::ReplyPending;
        ledger.entries[2].sticker = Some(StickerPart {
            path: "/archive/a.png".into(),
            state: Sending,
        });
        ledger.entries[3].state = ReceiptState::Merged;
        ledger.entries[3].reply = None;
        ledger.entries[3].merged_into = Some("message-4".into());
        let before = ledger.encode().unwrap();
        Ledger::decode(&before).unwrap();
        ledger.attach_archive(Some(db.clone())).unwrap();
        db.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            ledger
                .archive_with(|_| panic!("database failed before hot write"))
                .is_err()
        );
        assert_eq!(ledger.encode().unwrap(), before);
        db.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(ledger.archive_with(|_| Err(corrupt())).is_err());
        assert_eq!(ledger.encode().unwrap(), before);
        let committed = db.identity().unwrap().1;
        assert!(committed > 300);
        let mut hot = None;
        ledger
            .archive_with(|bytes| {
                hot = Some(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            db.identity().unwrap().1,
            committed,
            "重复批次不添加行或修改冷记录"
        );
        let mut restored = Ledger::decode(hot.as_ref().unwrap()).unwrap();
        assert_eq!(restored.version, 6);
        assert!(hot.unwrap().len() < HOT_TARGET + 100_000);
        assert!(restored.attach_archive(None).is_err());
        restored.attach_archive(Some(db.clone())).unwrap();
        for i in 0..350 {
            assert!(
                restored
                    .contains("1904159860", &format!("message-{i}"))
                    .unwrap()
            );
        }
        for i in 0..3 {
            assert!(
                restored
                    .find("1904159860", &format!("message-{i}"))
                    .is_some()
            );
        }
        assert!(restored.find("1904159860", "message-4").is_none());
        let cold = restored
            .archived("1904159860", "message-3")
            .unwrap()
            .unwrap();
        assert_eq!(cold.merged_into.as_deref(), Some("message-4"));
        db.rows.lock().unwrap().clear();
        assert!(
            restored.attach_archive(Some(db)).is_err(),
            "旧数据库缺行时拒绝恢复"
        );
    }
}
