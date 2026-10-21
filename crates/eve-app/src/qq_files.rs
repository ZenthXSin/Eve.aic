//! QQ 宿主的文件发送：模型只能报出文件名，宿主在固定目录里解析成绝对路径并校验，
//! 再交给通道上传。模型拿不到任意路径，也拿不到目录结构。
//!
//! 投递队列先写盘再交给通道；通道回执写回同一条记录。进程在写出前后退出时，记录停在
//! `Sending`，重启后记为结果未知，绝不自动重发。
use crate::AppError;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_plugin_api::PluginError;
use eve_qqbot_plugin::{
    ImageAttachment, Message, QQ_FILE_CAPTION_MAX_BYTES, QQ_FILE_NAME_MAX_BYTES, QqFileDelivery,
    QqFileMessage, QqFileResult, QqRawMessage, QqRawMessageObserver, valid_caption, valid_name,
};
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

pub(crate) const SEND_TOOL: &str = "send_file";
/// 等待队列与结果记录的上限，超过时拒绝新请求而不是悄悄丢弃。
const MAX_ENTRIES: usize = 200;
const FORMAT_VERSION: u32 = 1;

fn state(message: impl Into<String>) -> PluginError {
    PluginError::State(message.into())
}

fn failed(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Failed(message.into())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    session_id: String,
    user_id: String,
    path: String,
    name: Option<String>,
    caption: Option<String>,
    state: State,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum State {
    /// 已写盘、等通道取走。
    Pending,
    /// 已写出给通道，结果未回。
    Sending,
    Sent,
    Failed,
    /// 进程在写出前后退出，是否送达不确定。
    Unknown,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    next_id: u64,
    records: Vec<Record>,
}

struct Inner {
    ledger: Ledger,
}

/// 待发文件队列；所有变更同步落盘后才返回。
pub(crate) struct Outbox {
    directory: PathBuf,
    files: PathBuf,
    ledger: PathBuf,
    inner: Mutex<Inner>,
}

impl Outbox {
    /// 打开或新建队列；上次停在 `Sending` 的记录改为结果未知，不重发。
    pub(crate) fn open(directory: PathBuf) -> Result<Self, AppError> {
        let files = directory.join("files");
        fs::create_dir_all(&files)?;
        let ledger = directory.join("ledger.json");
        let mut value = match fs::read(&ledger) {
            Ok(bytes) => {
                if bytes.len() > 1024 * 1024 {
                    return Err("文件队列记录过大".into());
                }
                serde_json::from_slice::<Ledger>(&bytes).map_err(|_| "文件队列记录无法解析")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ledger {
                format_version: FORMAT_VERSION,
                next_id: 1,
                records: Vec::new(),
            },
            Err(error) => return Err(error.into()),
        };
        if value.format_version != FORMAT_VERSION {
            return Err("文件队列记录版本不支持".into());
        }
        let mut changed = false;
        for record in &mut value.records {
            if record.state == State::Sending {
                record.state = State::Unknown;
                changed = true;
            }
        }
        let outbox = Self {
            directory: directory.clone(),
            files,
            ledger,
            inner: Mutex::new(Inner { ledger: value }),
        };
        if changed {
            outbox.save()?;
        }
        Ok(outbox)
    }

    fn save(&self) -> Result<(), AppError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| AppError::from("文件队列锁不可用"))?;
        let bytes = serde_json::to_vec(&inner.ledger)?;
        if bytes.len() > 1024 * 1024 {
            return Err("文件队列记录过大".into());
        }
        let mut staged = tempfile::NamedTempFile::new_in(&self.directory)?;
        staged.write_all(&bytes)?;
        staged
            .persist(&self.ledger)
            .map_err(|_| AppError::from("文件队列记录保存失败"))?;
        Ok(())
    }

    /// 把模型给的文件名解析为 outbox 内的绝对路径；目录、越界与非常规文件一律拒绝。
    fn resolve(&self, name: &str) -> Result<PathBuf, ToolExecutionError> {
        if !valid_name(name) {
            return Err(failed(
                "文件名无效：不能包含路径分隔符或控制字符，也不能为空",
            ));
        }
        let relative = Path::new(name);
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(failed("文件名无效：不能包含路径"));
        }
        let path = self.files.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| failed("发件目录里没有这个文件；请先确认文件名"))?;
        if !metadata.file_type().is_file() {
            return Err(failed("只能发送普通文件，不能发送目录或链接"));
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| failed("文件路径无法解析"))?;
        let root = self
            .files
            .canonicalize()
            .map_err(|_| failed("发件目录无法解析"))?;
        if !canonical.starts_with(&root) {
            return Err(failed("文件不在发件目录内"));
        }
        Ok(canonical)
    }

    /// 写入一条待发记录并返回它的编号；落盘成功才交给通道。
    fn enqueue(
        &self,
        session: &SessionKey,
        path: PathBuf,
        name: Option<String>,
        caption: Option<String>,
    ) -> Result<String, ToolExecutionError> {
        let mut inner = self.inner.lock().map_err(|_| failed("文件队列锁不可用"))?;
        if inner.ledger.records.len() >= MAX_ENTRIES {
            return Err(failed("待发文件已满，请等前面的文件发完再试"));
        }
        let id = format!("file-{}", inner.ledger.next_id);
        inner.ledger.next_id += 1;
        inner.ledger.records.push(Record {
            id: id.clone(),
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
            path: path.to_string_lossy().into_owned(),
            name,
            caption,
            state: State::Pending,
        });
        drop(inner);
        self.save()
            .map_err(|_| failed("文件队列保存失败，没有排队"))?;
        Ok(id)
    }

    /// 最近一条记录的结果，供工具回复用户；没有时返回 None。
    fn result_of(&self, id: &str) -> Option<State> {
        let inner = self.inner.lock().ok()?;
        inner
            .ledger
            .records
            .iter()
            .find(|record| record.id == id)
            .map(|record| record.state)
    }
}

impl QqFileDelivery for Outbox {
    fn next_file(&self) -> eve_plugin_api::PluginResult<Option<QqFileMessage>> {
        let mut inner = self.inner.lock().map_err(|_| state("文件队列锁不可用"))?;
        let Some(index) = inner
            .ledger
            .records
            .iter()
            .position(|record| record.state == State::Pending)
        else {
            return Ok(None);
        };
        inner.ledger.records[index].state = State::Sending;
        let record = inner.ledger.records[index].clone();
        drop(inner);
        self.save()
            .map_err(|_| state("文件队列保存失败，未交给通道"))?;
        let session = SessionKey::new(record.session_id.clone(), record.user_id.clone())
            .map_err(|_| state("文件队列里的会话无效"))?;
        Ok(Some(QqFileMessage {
            id: record.id,
            session,
            path: record.path,
            name: record.name,
            caption: record.caption,
        }))
    }

    fn delivered(&self, id: &str, result: QqFileResult) -> eve_plugin_api::PluginResult<()> {
        let mut inner = self.inner.lock().map_err(|_| state("文件队列锁不可用"))?;
        let Some(record) = inner
            .ledger
            .records
            .iter_mut()
            .find(|record| record.id == id)
        else {
            return Err(state("文件队列里没有这条记录"));
        };
        record.state = match result {
            QqFileResult::Sent { .. } => State::Sent,
            QqFileResult::Failed { .. } | QqFileResult::NotSent => State::Failed,
        };
        drop(inner);
        self.save().map_err(|_| state("文件队列保存失败"))
    }
}

/// 当前会话最近一条消息的原始平台事件，只留一条，供调试工具读取。
pub(crate) struct RawInbox {
    inner: Mutex<Option<(String, QqRawMessage)>>,
}

impl RawInbox {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn current(&self, session_id: &str) -> Result<QqRawMessage, ToolExecutionError> {
        let inner = self.inner.lock().map_err(|_| failed("原始消息锁不可用"))?;
        inner
            .as_ref()
            .filter(|(session, _)| session == session_id)
            .map(|(_, raw)| raw.clone())
            .ok_or_else(|| failed("这个会话还没有收到可查看的原始消息"))
    }
}

impl QqRawMessageObserver for RawInbox {
    fn record(&self, session: &SessionKey, raw: QqRawMessage) {
        if let Ok(mut inner) = self.inner.lock() {
            *inner = Some((session.session_id.clone(), raw));
        }
    }
}

pub(crate) struct ReadRawMessageTool {
    inbox: std::sync::Arc<RawInbox>,
}

impl ReadRawMessageTool {
    pub(crate) fn new(inbox: std::sync::Arc<RawInbox>) -> Self {
        Self { inbox }
    }
}

impl Tool for ReadRawMessageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_raw_message".into(),
            description: "读取当前会话最近一条 QQ 消息的原始平台事件，用于核对附件和引用实际长什么样。只读，不下载、不发送。返回有界 JSON；过大时只给字节数和截断标记。".into(),
            argument_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.qqbot.raw".into() }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_some_and(|object| object.is_empty()) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "这个工具不接受参数".into(),
            })
        }
    }

    fn execute(&self, _call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let scope = context
                .scope()
                .ok_or_else(|| failed("只有带用户身份的对话里才能读取原始消息"))?;
            let raw = self.inbox.current(&scope.session_id)?;
            Ok(json!({"bytes": raw.bytes, "truncated": raw.truncated, "raw": raw.json}))
        })
    }
}

/// 最近收到的附件，按会话保存，供模型按序号下载。只保留有限条，超出时丢掉最早的。
pub(crate) struct AttachmentInbox {
    inner: Mutex<std::collections::VecDeque<(String, Message)>>,
}

impl AttachmentInbox {
    const MAX: usize = 64;

    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub(crate) fn record(&self, session_id: &str, message: Message) {
        // 只引用了带图消息的也要记下，视觉输入会附上被引用的图片。
        if message.attachments.is_empty()
            && message
                .quoted
                .as_ref()
                .is_none_or(|quoted| quoted.attachments.is_empty())
        {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.retain(|(session, saved)| !(session == session_id && saved.id == message.id));
        inner.push_back((session_id.to_owned(), message));
        while inner.len() > Self::MAX {
            inner.pop_front();
        }
    }

    /// 本会话中原文与本轮输入一致的最近一条带附件消息。
    pub(crate) fn turn_message(&self, session_id: &str, turn_text: &str) -> Option<Message> {
        let inner = self.inner.lock().ok()?;
        inner
            .iter()
            .rev()
            .find(|(session, message)| session == session_id && message.turn_text() == turn_text)
            .map(|(_, message)| message.clone())
    }

    fn attachment(
        &self,
        session_id: &str,
        index: usize,
    ) -> Result<ImageAttachment, ToolExecutionError> {
        let inner = self.inner.lock().map_err(|_| failed("附件记录锁不可用"))?;
        let message = inner
            .iter()
            .rev()
            .find(|(session, message)| session == session_id && !message.attachments.is_empty())
            .map(|(_, message)| message);
        let Some(message) = message else {
            return Err(failed("这个会话还没有收到附件"));
        };
        message.attachment(index).cloned().ok_or_else(|| {
            failed(format!(
                "最近一条带附件的消息只有 {} 个附件，index 从 1 起",
                message.attachments.len()
            ))
        })
    }
}

impl eve_qqbot_plugin::QqAttachmentObserver for AttachmentInbox {
    fn observe(&self, session: &eve_session_api::SessionKey, message: &Message) {
        self.record(&session.session_id, message.clone());
    }
}

/// 模型可见的发送工具：只接受文件名，路径由宿主在发件目录里解析。
pub(crate) struct SendFileTool {
    outbox: std::sync::Arc<Outbox>,
}

impl SendFileTool {
    pub(crate) fn new(outbox: std::sync::Arc<Outbox>) -> Self {
        Self { outbox }
    }
}

impl Tool for SendFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SEND_TOOL.into(),
            description: format!(
                "把发件目录里的一个文件发到当前会话。只能报文件名（不能带路径），宿主在固定目录里查找；\
                 文件不存在或不是普通文件时失败，文件大小不设上限。name 是文件名（至多 {QQ_FILE_NAME_MAX_BYTES} 字节），\
                 caption 是可选的简短说明（至多 {QQ_FILE_CAPTION_MAX_BYTES} 字节）。没有排上队就不要说已经发出：\
                 只有真的上传成功后，文件才会到用户那里。"
            ),
            argument_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "caption": {"type": "string"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.qqbot.files".into(),
            }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        let invalid = |message: &str| ToolValidationError {
            message: message.into(),
        };
        let object = value.as_object().ok_or_else(|| invalid("参数必须是对象"))?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "caption"))
        {
            return Err(invalid("参数只能包含 name 与 caption"));
        }
        object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_name(name))
            .ok_or_else(|| invalid("name 必须是合法文件名，不能包含路径分隔符"))?;
        match object.get("caption") {
            None => Ok(()),
            Some(caption) if caption.as_str().is_some_and(valid_caption) => Ok(()),
            Some(_) => Err(invalid("caption 必须是简短的纯文字")),
        }
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let scope = context
                .scope()
                .ok_or_else(|| failed("只有带用户身份的对话里才能发送文件"))?;
            let session = SessionKey::new(scope.session_id.clone(), scope.user_id.clone())
                .map_err(|_| failed("当前会话标识无效"))?;
            let name = call.arguments["name"]
                .as_str()
                .ok_or_else(|| failed("缺少文件名"))?;
            let path = self.outbox.resolve(name)?;
            let caption = call
                .arguments
                .get("caption")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let id = self.outbox.enqueue(&session, path, None, caption)?;
            let _ = self.outbox.result_of(&id);
            Ok(json!({
                "queued": true,
                "id": id,
                "note": "已排进待发队列，通道会在空闲时上传。现在还没有发出；只有上传成功才算送达。"
            }))
        })
    }
}

pub(crate) struct ReadAttachmentTool {
    inbox: std::sync::Arc<AttachmentInbox>,
}

impl ReadAttachmentTool {
    pub(crate) fn new(inbox: std::sync::Arc<AttachmentInbox>) -> Self {
        Self { inbox }
    }
    const MAX_BYTES: u64 = 1024 * 1024;
    const TEXT_LIMIT: usize = 16 * 1024;
}

impl Tool for ReadAttachmentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_attachment".into(),
            description: "下载当前会话最近一条带附件消息里的第 index 个附件（从 1 起），只取本条已记录的链接。                文本类附件返回开头内容，其他类型只返回类型、文件名和大小。图片没有视觉解析，不能据此声称看见了图片内容。".into(),
            argument_schema: json!({"type":"object","properties":{"index":{"type":"integer","minimum":1,"maximum":4}},"required":["index"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.qqbot.attachments".into() }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        let index = value.get("index").and_then(Value::as_u64);
        if value.as_object().is_some_and(|o| o.len() == 1) && matches!(index, Some(1..=4)) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "index 必须是 1 到 4 的整数".into(),
            })
        }
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let scope = context
                .scope()
                .ok_or_else(|| failed("只有带用户身份的对话里才能读取附件"))?;
            let index = call.arguments["index"]
                .as_u64()
                .ok_or_else(|| failed("缺少 index"))? as usize;
            let attachment = self.inbox.attachment(&scope.session_id, index)?;
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|_| failed("下载客户端不可用"))?
                .get(&attachment.url)
                .send()
                .await
                .map_err(|_| failed("附件下载失败，链接可能已过期"))?;
            if !response.status().is_success() {
                return Err(failed(format!(
                    "附件下载被拒绝（HTTP {}）",
                    response.status().as_u16()
                )));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|_| failed("附件内容读取失败"))?;
            if bytes.len() as u64 > Self::MAX_BYTES {
                return Err(failed(format!(
                    "附件超过 {} 字节，没有读取",
                    Self::MAX_BYTES
                )));
            }
            let mut result = json!({"content_type": attachment.content_type, "bytes": bytes.len(), "filename": attachment.filename});
            if attachment.content_type.starts_with("text/")
                || attachment.content_type == "application/json"
            {
                let text = String::from_utf8_lossy(&bytes);
                let truncated = text.len() > Self::TEXT_LIMIT;
                result["text"] = json!(text.chars().take(Self::TEXT_LIMIT).collect::<String>());
                result["truncated"] = json!(truncated);
            } else {
                result["note"] = json!(
                    "非文本附件只记录了类型和大小，没有解析内容；图片也不能据此声称已经看见。"
                );
            }
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox() -> (tempfile::TempDir, Outbox) {
        let directory = tempfile::tempdir().expect("temp dir");
        let outbox = Outbox::open(directory.path().to_path_buf()).expect("open");
        (directory, outbox)
    }

    #[test]
    fn only_plain_files_inside_the_outbox_are_accepted() {
        let (directory, outbox) = outbox();
        fs::write(directory.path().join("files/report.txt"), b"hello").expect("write");
        assert!(outbox.resolve("report.txt").is_ok());
        for name in ["../ledger.json", "sub/report.txt", "", "missing.txt"] {
            assert!(outbox.resolve(name).is_err(), "{name}");
        }
        fs::create_dir_all(directory.path().join("files/subdir")).expect("dir");
        assert!(outbox.resolve("subdir").is_err());
    }

    #[test]
    fn enqueue_persists_before_returning_and_restart_marks_unknown() {
        let (directory, outbox) = outbox();
        fs::write(directory.path().join("files/a.txt"), b"x").expect("write");
        let session = SessionKey::new("qq:s", "qq:u").expect("session");
        let path = outbox.resolve("a.txt").expect("resolve");
        outbox.enqueue(&session, path, None, None).expect("enqueue");
        let taken = outbox.next_file().expect("next").expect("one");
        assert_eq!(taken.id, "file-1");
        drop(outbox);
        let reopened = Outbox::open(directory.path().to_path_buf()).expect("reopen");
        assert_eq!(reopened.result_of("file-1"), Some(State::Unknown));
        assert!(reopened.next_file().expect("next").is_none());
    }

    #[test]
    fn delivery_result_is_recorded() {
        let (directory, outbox) = outbox();
        fs::write(directory.path().join("files/a.txt"), b"x").expect("write");
        let session = SessionKey::new("qq:s", "qq:u").expect("session");
        let path = outbox.resolve("a.txt").expect("resolve");
        outbox.enqueue(&session, path, None, None).expect("enqueue");
        let taken = outbox.next_file().expect("next").expect("one");
        outbox
            .delivered(
                &taken.id,
                QqFileResult::Sent {
                    platform_message_id: None,
                },
            )
            .expect("delivered");
        assert_eq!(outbox.result_of("file-1"), Some(State::Sent));
    }

    fn sample_message(id: &str, url: &str) -> Message {
        Message {
            id: id.into(),
            scope: "c2c".into(),
            target_id: "user-1".into(),
            user_id: "user-1".into(),
            text: "看这个".into(),
            attachments: vec![ImageAttachment {
                content_type: "text/plain".into(),
                url: url.into(),
                filename: Some("note.txt".into()),
            }],
            quoted: None,
        }
    }

    #[test]
    fn inbox_keeps_the_latest_message_and_rejects_a_bad_index() {
        let inbox = AttachmentInbox::new();
        assert!(inbox.attachment("qq:s", 1).is_err());
        inbox.record("qq:s", sample_message("m1", "https://media.example/a.txt"));
        inbox.record(
            "qq:other",
            sample_message("m2", "https://media.example/other.txt"),
        );
        inbox.record(
            "qq:s",
            sample_message("m3", "https://media.example/latest.txt"),
        );
        let found = inbox.attachment("qq:s", 1).expect("latest");
        assert_eq!(found.url, "https://media.example/latest.txt");
        assert!(inbox.attachment("qq:s", 2).is_err());
        assert!(inbox.attachment("qq:nobody", 1).is_err());
    }
}
