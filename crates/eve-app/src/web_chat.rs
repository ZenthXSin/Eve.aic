//! Web 是独立的可信通道。HTTP 不接收身份、权限、任意 SessionKey 或工具调用。
use eve_control_api::{
    CommitState, ControlInput, ControlPhase, ControlService, ControlSnapshot, DiscardControlEvents,
    RunFailure,
};
use eve_llm_api::{ContextScope, LlmError};
use eve_session_api::{SessionInput, SessionKey};
use eve_web_panel_api::*;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) const USER: &str = "web:owner";
pub(crate) const PREFIX: &str = "web:chat:";
const RECEIPT_LIMIT: u64 = 16 * 1024 * 1024;
pub(crate) fn is_owner(scope: &ContextScope) -> bool {
    scope.user_id == USER
        && scope
            .session_id
            .strip_prefix(PREFIX)
            .is_some_and(valid_chat_id)
}

pub(crate) fn key(id: &str) -> PanelResult<SessionKey> {
    if !valid_chat_id(id) {
        return Err(PanelError::InvalidInput);
    }
    SessionKey::new(format!("{PREFIX}{id}"), USER).map_err(|_| PanelError::InvalidInput)
}
pub(crate) struct WebContext {
    pub inner: Arc<dyn eve_llm_api::ContextAssembler>,
    pub web: Option<Arc<dyn eve_llm_api::ContextAssembler>>,
}
impl eve_llm_api::ContextAssembler for WebContext {
    fn assemble(
        &self,
        input: eve_llm_api::TurnInput,
    ) -> eve_llm_api::LlmFuture<'_, eve_llm_api::ContextSnapshot> {
        self.inner.assemble(input)
    }
    fn assemble_scoped(
        &self,
        input: eve_llm_api::TurnInput,
        scope: Option<ContextScope>,
    ) -> eve_llm_api::LlmFuture<'_, eve_llm_api::ContextSnapshot> {
        self.assemble_with_model_session(input, scope, None)
    }
    fn assemble_with_model_session(
        &self,
        input: eve_llm_api::TurnInput,
        scope: Option<ContextScope>,
        model_session: Option<eve_llm_api::ModelSession>,
    ) -> eve_llm_api::LlmFuture<'_, eve_llm_api::ContextSnapshot> {
        Box::pin(async move {
            let owner = scope.as_ref().is_some_and(is_owner);
            let source = if owner {
                self.web.as_ref().unwrap_or(&self.inner)
            } else {
                &self.inner
            };
            let mut context = source
                .assemble_with_model_session(input, scope, model_session.clone())
                .await?;
            if owner {
                context.profile.push_str("\n宿主认证事实：当前通道为 Web，当前用户是主人；身份由面板专用访问令牌与服务端绑定，不来自用户自述。可以直接交流和使用实际已装配工具。\n");
            }
            Ok(context)
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    fingerprint: String,
    #[serde(default)]
    local_control: bool,
    #[serde(default)]
    memory_recorded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learning_input: Option<eve_training_api::ConversationMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learning_input_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learning_output_at_ms: Option<u64>,
    state: ChatState,
}
pub(crate) struct WebChat {
    control: Arc<dyn ControlService>,
    directory: PathBuf,
    claims: PathBuf,
    open: std::sync::atomic::AtomicBool,
    admission: Mutex<std::collections::BTreeMap<String, eve_control_api::GenerationKey>>,
    business: Option<Arc<crate::qq_business::BusinessTool>>,
    recovery: Option<Arc<dyn eve_qqbot_plugin::QqFailureRecovery>>,
    ingress: Option<Arc<crate::web_ingress::RawMessages>>,
    subject_admission: Option<Arc<dyn eve_qqbot_plugin::QqAdmission>>,
    memory: Option<crate::web_memory::Observer>,
    training: Option<Arc<dyn eve_training_api::TrainingService>>,
    learning_stopped: std::sync::atomic::AtomicBool,
}
fn unavailable<T>(_: T) -> PanelError {
    PanelError::Unavailable
}
fn observed_at_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|time| time.as_millis().try_into().ok())
}
fn fingerprint(text: &str) -> String {
    digest(&SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn learning_binding(
    conversation_id: &str,
    request_id: &str,
    original_fingerprint: &str,
    message: &eve_training_api::ConversationMessage,
) -> String {
    // 和已保存的请求 claim 交叉绑定；/bind 原文不为校验目的重新保存。
    fingerprint(
        &serde_json::to_string(&(conversation_id, request_id, original_fingerprint, message))
            .expect("交流消息可序列化"),
    )
}
impl Receipt {
    fn learning_valid(&self) -> bool {
        match (&self.learning_input, &self.learning_input_binding) {
            (None, None) => true,
            (Some(message), Some(binding)) => {
                let Some(metadata) = &message.metadata else {
                    return false;
                };
                let original = match metadata.source_kind.as_deref() {
                    Some("web_input") => fingerprint(&message.text) == self.fingerprint,
                    Some("web_redacted_input") => message.text == "/bind [已隐藏]",
                    _ => false,
                };
                message.id == self.state.request_id
                    && message.speaker == fingerprint(USER)
                    && message.role == eve_training_api::ConversationRole::Human
                    && eve_training_api::conversation_message_valid(message)
                    && metadata.sequence.is_none()
                    && metadata.fragment.is_none()
                    && metadata.context.is_none()
                    && metadata.source_at_ms.is_none()
                    && metadata.reply_to.is_none()
                    && metadata.turn_id.is_none()
                    && metadata.activity.as_ref().is_some_and(|activity| {
                        activity.phase == "received"
                            && activity.elapsed_ms.is_none()
                            && activity.tool_name.is_none()
                    })
                    && original
                    && *binding
                        == learning_binding(
                            &self.state.conversation_id,
                            &self.state.request_id,
                            &self.fingerprint,
                            message,
                        )
            }
            _ => false,
        }
    }
}
fn read_bound(path: &Path, limit: u64) -> PanelResult<Vec<u8>> {
    let file = fs::File::open(path).map_err(unavailable)?;
    let metadata = file.metadata().map_err(unavailable)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(PanelError::Unavailable);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len() + 1).map_err(unavailable)?);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > limit {
        return Err(PanelError::Unavailable);
    }
    Ok(bytes)
}
fn control_error(e: eve_control_api::ControlError) -> PanelError {
    match e {
        eve_control_api::ControlError::Busy | eve_control_api::ControlError::StaleGeneration => {
            PanelError::Stale
        }
        eve_control_api::ControlError::InvalidInput => PanelError::InvalidInput,
        _ => PanelError::Unavailable,
    }
}
impl WebChat {
    pub fn open(control: Arc<dyn ControlService>, root: &Path) -> PanelResult<Self> {
        let directory = root.join("web-chat-requests");
        let claims = root.join("web-chat-claims");
        let marker = root.join("web-chat.initialized.v1");
        let initialized = match fs::symlink_metadata(&marker) {
            Ok(metadata) => {
                if !metadata.file_type().is_file()
                    || metadata.len() != 13
                    || read_bound(&marker, 13)? != b"{\"version\":1}"
                {
                    return Err(PanelError::Unavailable);
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(PanelError::Unavailable),
        };
        for path in [&directory, &claims] {
            match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    if !metadata.file_type().is_dir()
                        || (!initialized
                            && fs::read_dir(path).map_err(unavailable)?.next().is_some())
                    {
                        return Err(PanelError::Unavailable);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !initialized => {
                    fs::create_dir(path).map_err(unavailable)?
                }
                Err(_) => return Err(PanelError::Unavailable),
            }
        }
        if !initialized {
            let mut temporary = tempfile::NamedTempFile::new_in(root).map_err(unavailable)?;
            temporary
                .write_all(b"{\"version\":1}")
                .map_err(unavailable)?;
            temporary.as_file().sync_all().map_err(unavailable)?;
            temporary.persist_noclobber(&marker).map_err(unavailable)?;
            #[cfg(unix)]
            fs::File::open(root)
                .and_then(|f| f.sync_all())
                .map_err(unavailable)?;
        }
        if !fs::symlink_metadata(&directory)
            .map_err(unavailable)?
            .file_type()
            .is_dir()
        {
            return Err(PanelError::Unavailable);
        }
        Ok(Self {
            control,
            directory,
            claims,
            open: std::sync::atomic::AtomicBool::new(true),
            admission: Mutex::new(Default::default()),
            business: None,
            recovery: None,
            ingress: None,
            subject_admission: None,
            memory: None,
            training: None,
            learning_stopped: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub(crate) fn with_business(mut self, business: Arc<crate::qq_business::BusinessTool>) -> Self {
        self.business = Some(business);
        self
    }
    pub(crate) fn with_recovery(
        mut self,
        recovery: Arc<dyn eve_qqbot_plugin::QqFailureRecovery>,
    ) -> Self {
        self.recovery = Some(recovery);
        self
    }
    pub(crate) fn with_ingress(mut self, ingress: Arc<crate::web_ingress::RawMessages>) -> Self {
        self.ingress = Some(ingress);
        self
    }
    pub(crate) fn with_admission(
        mut self,
        admission: Arc<dyn eve_qqbot_plugin::QqAdmission>,
    ) -> Self {
        self.subject_admission = Some(admission);
        self
    }
    pub(crate) fn with_memory(
        mut self,
        memory: Arc<dyn eve_memory_api::MemoryAdmin>,
        sessions: Arc<dyn eve_session_api::SessionService>,
    ) -> Self {
        self.memory = Some(crate::web_memory::Observer { memory, sessions });
        self
    }
    pub(crate) fn with_training(
        mut self,
        training: Arc<dyn eve_training_api::TrainingService>,
    ) -> Self {
        self.training = Some(training);
        self
    }
    fn observe_memory(&self, receipt: &mut Receipt) -> PanelResult<()> {
        // 已持久可见回复的学习不依赖完整轮次记忆导入是否成功。
        self.observe_learning(receipt)?;
        self.record_memory(receipt)
    }
    fn record_memory(&self, receipt: &mut Receipt) -> PanelResult<()> {
        if receipt.local_control || receipt.memory_recorded || receipt.state.status != "completed" {
            return Ok(());
        }
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let error = match memory.record(&receipt.state, &receipt.fingerprint) {
            Ok(()) => {
                receipt.memory_recorded = true;
                None
            }
            Err(error) => {
                eprintln!(
                    "EVE_WEB_MEMORY_UNCONFIRMED request_id={} error={error}",
                    receipt.state.request_id
                );
                Some(format!("交流记忆尚未确认保存：{error}"))
            }
        };
        if receipt.memory_recorded || receipt.state.memory_error != error {
            receipt.state.memory_error = error;
            self.save(receipt, false)?;
        }
        Ok(())
    }

    fn observe_learning(&self, receipt: &Receipt) -> PanelResult<()> {
        self.observe_learning_records(receipt, true, true)
    }
    fn observe_learning_records(
        &self,
        receipt: &Receipt,
        input: bool,
        output: bool,
    ) -> PanelResult<()> {
        let Some(training) = &self.training else {
            return Ok(());
        };
        if self
            .learning_stopped
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let session = key(&receipt.state.conversation_id)?;
        let scope = ContextScope {
            session_id: session.session_id,
            user_id: session.user_id,
        };
        // 独立训练存储按消息 ID 幂等；不为学习进度另写业务回执。
        // 原回执持续保留全部输入与已提交输出，恢复只重投观察，不运行模型或工具。
        if input && let Some(message) = &receipt.learning_input {
            if training
                .observe_conversation(&scope, message.clone())
                .is_err()
            {
                self.learning_stopped
                    .store(true, std::sync::atomic::Ordering::Release);
                eprintln!("EVE_WEB_CONVERSATION observation_stopped");
                return Ok(());
            }
        }
        if output
            && !receipt.local_control
            && receipt.state.status == "completed"
            && let Some(text) = &receipt.state.text
        {
            let metadata = eve_training_api::ConversationMetadata {
                observed_at_ms: receipt.learning_output_at_ms,
                reply_to: Some(receipt.state.request_id.clone()),
                turn_id: receipt.state.turn_id,
                source_kind: Some("web_visible_response".into()),
                activity: Some(eve_training_api::ConversationActivity {
                    phase: "completed".into(),
                    elapsed_ms: None,
                    tool_name: None,
                }),
                ..Default::default()
            };
            if crate::conversation_learning::observe_with_metadata(
                training.as_ref(),
                &scope,
                &format!("eve:{}", receipt.state.request_id),
                "eve",
                eve_training_api::ConversationRole::Eve,
                text,
                Some(metadata),
            )
            .is_err()
            {
                self.learning_stopped
                    .store(true, std::sync::atomic::Ordering::Release);
                eprintln!("EVE_WEB_CONVERSATION observation_stopped");
                return Ok(());
            }
        }
        Ok(())
    }

    fn learning_input(
        &self,
        request: &ChatSubmit,
    ) -> Option<eve_training_api::ConversationMessage> {
        self.training.as_ref()?;
        let private = request.text.split_whitespace().next() == Some("/bind");
        let text = if private {
            "/bind [已隐藏]"
        } else {
            &request.text
        };
        let message = eve_training_api::ConversationMessage {
            id: request.request_id.clone(),
            speaker: fingerprint(USER),
            role: eve_training_api::ConversationRole::Human,
            text: text.into(),
            metadata: Some(eve_training_api::ConversationMetadata {
                observed_at_ms: observed_at_ms(),
                source_kind: Some(
                    if private {
                        "web_redacted_input"
                    } else {
                        "web_input"
                    }
                    .into(),
                ),
                activity: Some(eve_training_api::ConversationActivity {
                    phase: "received".into(),
                    elapsed_ms: None,
                    tool_name: None,
                }),
                ..Default::default()
            }),
        };
        if !eve_training_api::conversation_message_valid(&message) {
            eprintln!("EVE_WEB_CONVERSATION invalid_input_not_observed");
            return None;
        }
        Some(message)
    }
    /// 恢复已落盘的输入和完成回执，不重发输入、模型或工具。
    /// 只排序小型时间索引；正文逐条读取，不用恢复时刻补猜原先时序。
    pub(crate) fn recover_memory(&self) -> PanelResult<()> {
        if self.memory.is_none() && self.training.is_none() {
            return Ok(());
        }
        let _active = self.admission.lock().map_err(unavailable)?;
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.claims).map_err(unavailable)? {
            let name = entry.map_err(unavailable)?.file_name();
            let Some((conversation_id, request_id)) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|name| name.split_once('-'))
            else {
                return Err(PanelError::Unavailable);
            };
            let receipt = self
                .load(&ChatLookup {
                    conversation_id: conversation_id.into(),
                    request_id: request_id.into(),
                })?
                .ok_or(PanelError::Unavailable)?;
            if self.training.is_some() {
                if let Some(message) = &receipt.learning_input {
                    records.push((
                        conversation_id.to_owned(),
                        request_id.to_owned(),
                        message.metadata.as_ref().and_then(|m| m.observed_at_ms),
                        false,
                    ));
                }
                if !receipt.local_control
                    && receipt.state.status == "completed"
                    && receipt.state.text.is_some()
                {
                    records.push((
                        conversation_id.to_owned(),
                        request_id.to_owned(),
                        receipt.learning_output_at_ms,
                        true,
                    ));
                }
            }
        }
        records.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.2.is_none().cmp(&right.2.is_none()))
                .then_with(|| left.2.cmp(&right.2))
                .then_with(|| left.3.cmp(&right.3))
                .then_with(|| left.1.cmp(&right.1))
        });
        for (conversation_id, request_id, _, output) in records {
            let receipt = self
                .load(&ChatLookup {
                    conversation_id,
                    request_id,
                })?
                .ok_or(PanelError::Unavailable)?;
            self.observe_learning_records(&receipt, !output, output)?;
        }
        if self.memory.is_some() {
            for entry in fs::read_dir(&self.claims).map_err(unavailable)? {
                let name = entry.map_err(unavailable)?.file_name();
                let Some((conversation_id, request_id)) = name
                    .to_str()
                    .and_then(|name| name.strip_suffix(".json"))
                    .and_then(|name| name.split_once('-'))
                else {
                    return Err(PanelError::Unavailable);
                };
                let mut receipt = self
                    .load(&ChatLookup {
                        conversation_id: conversation_id.into(),
                        request_id: request_id.into(),
                    })?
                    .ok_or(PanelError::Unavailable)?;
                self.record_memory(&mut receipt)?;
            }
        }
        Ok(())
    }
    fn cancel_active(&self, session: &SessionKey) -> PanelResult<()> {
        if let Some(snapshot) = self.control.snapshot(session).map_err(control_error)?
            && snapshot.report.is_none()
        {
            self.control.cancel(&snapshot.key).map_err(control_error)?;
        }
        Ok(())
    }
    pub fn info(&self) -> ChatInfo {
        ChatInfo {
            role: "owner",
            user_id: USER,
            session_prefix: PREFIX,
            max_input_bytes: MAX_CHAT_INPUT_BYTES,
        }
    }
    fn path(&self, id: &ChatLookup) -> PathBuf {
        self.directory
            .join(format!("{}-{}.json", id.conversation_id, id.request_id))
    }
    fn load(&self, id: &ChatLookup) -> PanelResult<Option<Receipt>> {
        id.validate()?;
        let path = self.path(id);
        let claim_path = self
            .claims
            .join(path.file_name().ok_or(PanelError::Unavailable)?);
        let claim = match fs::symlink_metadata(&claim_path) {
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() == 64 => {
                Some(read_bound(&claim_path, 64)?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            _ => return Err(PanelError::Unavailable),
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && claim.is_none() => {
                return Ok(None);
            }
            Err(_) => return Err(PanelError::Unavailable),
        };
        if !metadata.file_type().is_file() || metadata.len() > RECEIPT_LIMIT {
            return Err(PanelError::Unavailable);
        }
        let bytes = read_bound(&path, RECEIPT_LIMIT)?;
        let receipt: Receipt = serde_json::from_slice(&bytes).map_err(unavailable)?;
        if receipt.version != 1
            || claim.as_deref() != Some(receipt.fingerprint.as_bytes())
            || !matches!(
                receipt.state.status.as_str(),
                "accepted"
                    | "generating"
                    | "tools"
                    | "committing"
                    | "cancelling"
                    | "completed"
                    | "failed"
                    | "unknown"
                    | "rejected"
            )
            || (receipt.state.status == "completed"
                && (receipt.state.text.is_none()
                    || (!receipt.local_control && receipt.state.generation.is_none())))
            || (receipt.local_control && receipt.state.generation.is_some())
            || (receipt.memory_recorded
                && (receipt.local_control
                    || receipt.state.status != "completed"
                    || receipt.state.memory_error.is_some()))
            || receipt.state.conversation_id != id.conversation_id
            || receipt.state.request_id != id.request_id
            || receipt.fingerprint.len() != 64
            || !receipt.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            || !receipt.learning_valid()
            || receipt.state.generation.as_ref().is_some_and(|g| {
                g.generation == 0
                    || g.session != key(&id.conversation_id).expect("已验证编号")
                    || g.task_id != format!("web-request:{}", id.request_id)
            })
        {
            return Err(PanelError::Unavailable);
        }
        Ok(Some(receipt))
    }
    fn save(&self, receipt: &Receipt, first: bool) -> PanelResult<()> {
        struct Limited<W>(W, u64);
        impl<W: Write> Write for Limited<W> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() as u64 > self.1 {
                    return Err(std::io::Error::other("Web 回执超过上限"));
                }
                let n = self.0.write(bytes)?;
                self.1 -= n as u64;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(unavailable)?;
        serde_json::to_writer(Limited(&mut temporary, RECEIPT_LIMIT), receipt)
            .map_err(unavailable)?;
        temporary.as_file().sync_all().map_err(unavailable)?;
        let path = self.path(&ChatLookup {
            conversation_id: receipt.state.conversation_id.clone(),
            request_id: receipt.state.request_id.clone(),
        });
        if first {
            let claim_path = self
                .claims
                .join(path.file_name().ok_or(PanelError::Unavailable)?);
            let mut claim = tempfile::NamedTempFile::new_in(&self.claims).map_err(unavailable)?;
            claim
                .write_all(receipt.fingerprint.as_bytes())
                .map_err(unavailable)?;
            claim.as_file().sync_all().map_err(unavailable)?;
            claim.persist_noclobber(claim_path).map_err(unavailable)?;
            #[cfg(unix)]
            fs::File::open(&self.claims)
                .and_then(|f| f.sync_all())
                .map_err(unavailable)?;
            temporary.persist_noclobber(path).map_err(unavailable)?;
        } else {
            temporary.persist(path).map_err(unavailable)?;
        }
        #[cfg(unix)]
        fs::File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(unavailable)?;
        Ok(())
    }
    fn update(
        &self,
        mut receipt: Receipt,
        snapshot: Option<ControlSnapshot>,
    ) -> PanelResult<ChatState> {
        if matches!(
            receipt.state.status.as_str(),
            "completed" | "failed" | "rejected"
        ) {
            self.observe_memory(&mut receipt)?;
            return Ok(receipt.state);
        }
        let Some(snapshot) = snapshot.filter(|s| {
            s.key.task_id == format!("web-request:{}", receipt.state.request_id)
                && receipt
                    .state
                    .generation
                    .as_ref()
                    .is_none_or(|g| g == &s.key)
        }) else {
            receipt.state.status = "unknown".into();
            receipt.state.failure = Some(
                "实例已重启或执行状态无法确认；会话记录保留，未自动重发。请先核对已执行结果".into(),
            );
            return Ok(receipt.state);
        };
        receipt.state.generation = Some(snapshot.key.clone());
        receipt.state.turn_id = snapshot.turn_id;
        receipt.state.status = match snapshot.phase {
            ControlPhase::Starting => "accepted",
            ControlPhase::Generating => "generating",
            ControlPhase::Tools => "tools",
            ControlPhase::Committing => "committing",
            ControlPhase::Cancelling => "cancelling",
            ControlPhase::Finished => "failed",
            ControlPhase::Blocked => "unknown",
        }
        .into();
        if let Some(report) = snapshot.report {
            receipt.state.turn_id = report.run.turn_id;
            receipt.state.started_tools = report.run.started_tools;
            receipt.state.tool_receipts = report.run.tool_results.len();
            if report.run.commit == CommitState::Completed && report.run.failure.is_none() {
                receipt.state.status = "completed".into();
                receipt.state.text = report.run.text;
                receipt.learning_output_at_ms = observed_at_ms();
            } else {
                receipt.state.status = if matches!(
                    report.run.commit,
                    CommitState::Pending | CommitState::Unknown
                ) {
                    "unknown"
                } else {
                    "failed"
                }
                .into();
                receipt.state.failure = Some(
                    match report.run.failure {
                        Some(RunFailure::Execution(LlmError::Cancelled)) => {
                            "已停止本轮；已发生的操作和回执保留"
                        }
                        Some(RunFailure::Execution(LlmError::Protocol(_))) => {
                            "模型回复协议检查失败；已有工具回执保留，未自动重跑"
                        }
                        Some(RunFailure::Execution(LlmError::ProviderTimeout)) => {
                            "模型请求超时；已有工具回执保留，未自动重跑"
                        }
                        Some(RunFailure::Commit(_) | RunFailure::FailureRecord { .. }) => {
                            "结果或失败记录保存未能确认；原记录保留，请先核对，未自动重跑"
                        }
                        _ => "本轮未能完成；原记录与已执行工具回执保留，未自动重跑",
                    }
                    .into(),
                );
            }
            self.save(&receipt, false)?;
            self.observe_memory(&mut receipt)?;
        }
        Ok(receipt.state)
    }
    pub fn submit(self: &Arc<Self>, request: ChatSubmit) -> PanelResult<ChatState> {
        request.validate()?;
        let mut active = self.admission.lock().map_err(unavailable)?;
        if !self.open.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PanelError::Unavailable);
        }
        let lookup = request.lookup();
        let fingerprint = fingerprint(&request.text);
        let session = key(&request.conversation_id)?;
        if let Some(receipt) = self.load(&lookup)? {
            if receipt.fingerprint != fingerprint {
                return Err(PanelError::Stale);
            }
            return self.update(
                receipt,
                self.control.snapshot(&session).map_err(control_error)?,
            );
        }
        let learning_input = self.learning_input(&request);
        let learning_input_binding = learning_input.as_ref().map(|message| {
            learning_binding(
                &request.conversation_id,
                &request.request_id,
                &fingerprint,
                message,
            )
        });
        // 可信 HTTP 原文的明确缩权先落意图再执行，不能被同会话在途模型审核挡住。
        // 这条路径不授权新业务；重复请求只返回原回执，崩溃窗口保留 unknown。
        if let Some(business) = &self.business
            && crate::qq_business::BusinessTool::is_restriction_input(&request.text)
        {
            let mut receipt = Receipt {
                learning_input,
                learning_input_binding,
                learning_output_at_ms: None,
                version: 1,
                fingerprint,
                local_control: true,
                memory_recorded: false,
                state: ChatState {
                    conversation_id: request.conversation_id,
                    request_id: request.request_id,
                    status: "accepted".into(),
                    generation: None,
                    turn_id: None,
                    text: None,
                    failure: None,
                    memory_error: None,
                    started_tools: Some(0),
                    tool_receipts: 0,
                },
            };
            let scope = ContextScope {
                session_id: session.session_id.clone(),
                user_id: session.user_id.clone(),
            };
            let originals = (|| {
                self.save(&receipt, true)?;
                self.observe_learning(&receipt)?;
                let ingress = self.ingress.as_ref().ok_or(PanelError::Unavailable)?;
                ingress
                    .record(&session, &receipt.state.request_id, &request.text)
                    .map_err(unavailable)?;
                eve_llm_api::ReviewUserMessageSource::read_at(
                    ingress.as_ref(),
                    &scope,
                    &receipt.state.request_id,
                )
                .map_err(unavailable)?
                .ok_or(PanelError::Unavailable)
            })();
            let originals = match originals {
                Ok(originals) => originals,
                Err(error) => {
                    let _ = business.restrict_missing(
                        &scope,
                        &format!("web-request:{}", receipt.state.request_id),
                        &request.text,
                    );
                    if self.cancel_active(&session).is_err() {
                        eprintln!("EVE_WEB_CHAT_RESTRICTION_CANCEL_UNCONFIRMED");
                    }
                    return Err(error);
                }
            };
            let restricted = business.restrict_input(
                &scope,
                &format!("web-request:{}", receipt.state.request_id),
                &originals,
            );
            // 已认证主人明确缩权；即使业务审计或后台结算失败，仍停止前台。
            let cancelled = self.cancel_active(&session);
            match restricted {
                Ok(true) => {
                    if cancelled.is_ok() {
                        receipt.state.status = "completed".into();
                        receipt.state.text = Some(
                            "当前会话业务权限已关闭；已请求停止在途执行，已有操作及回执保留。"
                                .into(),
                        );
                    } else {
                        receipt.state.status = "failed".into();
                        receipt.state.failure = Some(
                            "当前会话业务权限已关闭；前台停止尚未确认，已有操作及回执保留。".into(),
                        );
                    }
                }
                Ok(false) => {
                    receipt.state.status = "rejected".into();
                    receipt.state.failure = Some("当前入口身份未通过缩权核验，未执行。".into());
                }
                Err(_) => {
                    receipt.state.status = "failed".into();
                    receipt.state.failure =
                        Some("缩权或取消结算未能确认；业务保持关闭，原记录保留。".into());
                }
            }
            self.save(&receipt, false)?;
            return Ok(receipt.state);
        }
        let mut unreadable = false;
        active.retain(
            |_, generation| match self.control.snapshot(&generation.session) {
                Ok(Some(snapshot)) if snapshot.key == *generation => {
                    snapshot.report.is_none() || snapshot.phase == ControlPhase::Blocked
                }
                _ => {
                    unreadable = true;
                    true
                }
            },
        );
        if unreadable {
            return Err(PanelError::Unavailable);
        }
        if active.len() >= 16 {
            return Err(PanelError::Stale);
        }
        if let Some(old) = self.control.snapshot(&session).map_err(control_error)? {
            if old.report.is_none() {
                return Err(PanelError::Stale);
            }
            if let Some(id) = old.key.task_id.strip_prefix("web-request:") {
                let previous = ChatLookup {
                    conversation_id: request.conversation_id.clone(),
                    request_id: id.into(),
                };
                let receipt = self.load(&previous)?.ok_or(PanelError::Unavailable)?;
                self.update(receipt, Some(old))?;
            }
        }
        let mut receipt = Receipt {
            learning_input,
            learning_input_binding,
            learning_output_at_ms: None,
            version: 1,
            fingerprint,
            local_control: false,
            memory_recorded: false,
            state: ChatState {
                conversation_id: request.conversation_id,
                request_id: request.request_id,
                status: "accepted".into(),
                generation: None,
                turn_id: None,
                text: None,
                failure: None,
                memory_error: None,
                started_tools: None,
                tool_receipts: 0,
            },
        };
        let ticket = match &self.subject_admission {
            Some(admission) => {
                let source = format!("web-request:{}", receipt.state.request_id);
                match admission.begin(&session, &source).map_err(unavailable)? {
                    Some(ticket) => Some(ticket),
                    None => {
                        // 保存终态，使同请求重试稳定；撤回无人消费的主体队头。
                        receipt.state.status = "rejected".into();
                        receipt.state.failure =
                            Some("主体正在执行其他工作，本次输入未执行；稍后可发送新请求。".into());
                        let saved = self.save(&receipt, true);
                        admission.abandon(&session, &source).map_err(unavailable)?;
                        saved?;
                        self.observe_learning(&receipt)?;
                        return Ok(receipt.state);
                    }
                }
            }
            None => None,
        };
        if let Err(error) = self.save(&receipt, true) {
            if let (Some(admission), Some(ticket)) = (&self.subject_admission, &ticket) {
                admission
                    .finish(ticket, eve_qqbot_plugin::QqAdmissionResult::Rejected)
                    .map_err(unavailable)?;
            }
            return Err(error);
        }
        self.observe_learning(&receipt)?;
        if let Some(ingress) = &self.ingress
            && let Err(error) = ingress.record(&session, &receipt.state.request_id, &request.text)
        {
            if let (Some(admission), Some(ticket)) = (&self.subject_admission, &ticket) {
                admission
                    .finish(ticket, eve_qqbot_plugin::QqAdmissionResult::Rejected)
                    .map_err(unavailable)?;
            }
            return Err(unavailable(error));
        }
        let metadata = self
            .ingress
            .as_ref()
            .map(|_| eve_control_api::ControlRunMetadata {
                scope: ContextScope {
                    session_id: session.session_id.clone(),
                    user_id: session.user_id.clone(),
                },
                task_id: format!("web-request:{}", receipt.state.request_id),
                review: eve_control_api::ControlReviewEvidence::ChannelAnchor(
                    receipt.state.request_id.clone(),
                ),
            });
        if let Some(training) = &self.training {
            let scope = ContextScope {
                session_id: session.session_id.clone(),
                user_id: session.user_id.clone(),
            };
            if training
                .observe_user_message(&scope, &receipt.state.request_id, &request.text)
                .is_err()
            {
                eprintln!("EVE_WEB_EXPRESSION observation_failed");
            }
        }
        let submitted = self.control.submit_with_metadata(
            ControlInput {
                session: SessionInput {
                    key: session,
                    text: request.text,
                },
                task_id: format!("web-request:{}", receipt.state.request_id),
            },
            metadata,
            Arc::new(DiscardControlEvents),
        );
        let admitted = if let (Some(admission), Some(ticket)) = (&self.subject_admission, &ticket) {
            let outcome = match &submitted {
                Ok(_) => eve_qqbot_plugin::QqAdmissionResult::Started,
                Err(
                    eve_control_api::ControlError::Unavailable
                    | eve_control_api::ControlError::Blocked,
                ) => eve_qqbot_plugin::QqAdmissionResult::Unknown,
                Err(_) => eve_qqbot_plugin::QqAdmissionResult::Rejected,
            };
            admission.finish(ticket, outcome).map_err(unavailable)
        } else {
            Ok(())
        };
        let generation = match submitted {
            Ok(generation) => generation,
            Err(error) => {
                let unknown = matches!(
                    error,
                    eve_control_api::ControlError::Unavailable
                        | eve_control_api::ControlError::Blocked
                );
                receipt.state.status = if unknown { "unknown" } else { "rejected" }.into();
                receipt.state.failure = Some(if unknown {
                    "控制服务受理状态尚未确认，原请求保留且未自动重发。".into()
                } else {
                    format!("本次输入未执行：{error}；稍后可发送新请求。")
                });
                self.save(&receipt, false)?;
                admitted?;
                return Ok(receipt.state);
            }
        };
        active.insert(receipt.state.conversation_id.clone(), generation.clone());
        receipt.state.generation = Some(generation.clone());
        // 结果落盘不依赖浏览器轮询；等待句柄在新代启动前捕获，永远只结算原请求。
        let wait = self.control.wait(&generation);
        let service = self.clone();
        let lookup = ChatLookup {
            conversation_id: receipt.state.conversation_id.clone(),
            request_id: receipt.state.request_id.clone(),
        };
        tokio::spawn(async move {
            let report = match wait.await {
                Ok(report) => report,
                Err(_) => {
                    eprintln!("EVE_WEB_CHAT_COMPLETION_UNCONFIRMED");
                    return;
                }
            };
            if let Some(admission) = &service.subject_admission
                && admission
                    .completed(
                        &report.key.session,
                        &report.key.task_id,
                        !matches!(
                            report.run.commit,
                            CommitState::Pending | CommitState::Unknown
                        ),
                    )
                    .is_err()
            {
                eprintln!("EVE_WEB_CHAT_SUBJECT_COMPLETION_UNCONFIRMED");
            }
            if let Some(recovery) = &service.recovery
                && report.run.failure.is_some()
                && recovery.recover(&report).is_err()
            {
                eprintln!("EVE_WEB_CHAT_RECOVERY_UNCONFIRMED");
            }
            let snapshot = ControlSnapshot {
                key: report.key.clone(),
                input_text: String::new(),
                phase: if matches!(
                    report.run.commit,
                    CommitState::Pending | CommitState::Unknown
                ) {
                    ControlPhase::Blocked
                } else {
                    ControlPhase::Finished
                },
                cancel_requested: report.cancel_requested,
                events_retired: false,
                turn_id: report.run.turn_id,
                report: Some(report),
            };
            let saved = (|| -> PanelResult<()> {
                let mut active = service.admission.lock().map_err(unavailable)?;
                let receipt = service.load(&lookup)?.ok_or(PanelError::Unavailable)?;
                let generation = snapshot.key.clone();
                let state = service.update(receipt, Some(snapshot))?;
                if matches!(state.status.as_str(), "completed" | "failed")
                    && service.open.load(std::sync::atomic::Ordering::Acquire)
                {
                    match service.control.retire_finished(&generation) {
                        Ok(()) | Err(eve_control_api::ControlError::StaleGeneration) => {
                            if active.get(&lookup.conversation_id) == Some(&generation) {
                                active.remove(&lookup.conversation_id);
                            }
                        }
                        Err(eve_control_api::ControlError::Unavailable) => {}
                        Err(_) => return Err(PanelError::Unavailable),
                    }
                }
                Ok(())
            })();
            if saved.is_err() {
                eprintln!("EVE_WEB_CHAT_COMPLETION_SAVE_FAILED");
            }
        });
        // control 已受理后，观察任务与 shutdown 跟踪先建立；后续存储或准入结算
        // 失败只取消这一代，仍由已经持有的等待句柄保存真实结局。
        if let Err(error) = admitted.and_then(|_| self.save(&receipt, false)) {
            self.control.cancel(&generation).map_err(control_error)?;
            return Err(error);
        }
        Ok(receipt.state)
    }
    pub fn read(&self, request: ChatLookup) -> PanelResult<ChatState> {
        request.validate()?;
        let _admission = self.admission.lock().map_err(unavailable)?;
        let receipt = self.load(&request)?.ok_or(PanelError::NotFound)?;
        self.update(
            receipt,
            self.control
                .snapshot(&key(&request.conversation_id)?)
                .map_err(control_error)?,
        )
    }
    pub fn cancel(&self, request: ChatLookup) -> PanelResult<ChatState> {
        let current = self.read(request.clone())?;
        if !matches!(
            current.status.as_str(),
            "completed" | "failed" | "unknown" | "rejected"
        ) && let Some(generation) = current.generation
        {
            self.control.cancel(&generation).map_err(control_error)?;
        }
        self.read(request)
    }
    pub async fn shutdown(&self) -> PanelResult<()> {
        self.open.store(false, std::sync::atomic::Ordering::Release);
        let active: Vec<_> = self
            .admission
            .lock()
            .map_err(unavailable)?
            .values()
            .cloned()
            .collect();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut waits = vec![];
        for generation in active {
            let wait = self.control.wait(&generation);
            self.control.cancel(&generation).map_err(control_error)?;
            waits.push((generation, wait));
        }
        for (generation, wait) in waits {
            tokio::time::timeout_at(deadline, wait)
                .await
                .map_err(unavailable)?
                .map_err(control_error)?;
            let request_id = generation
                .task_id
                .strip_prefix("web-request:")
                .ok_or(PanelError::Unavailable)?;
            let conversation_id = generation
                .session
                .session_id
                .strip_prefix(PREFIX)
                .ok_or(PanelError::Unavailable)?;
            self.read(ChatLookup {
                conversation_id: conversation_id.into(),
                request_id: request_id.into(),
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "web_chat_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn owner_identity_requires_both_host_bound_fields_and_valid_conversation_id() {
        let key = key(&"a".repeat(32)).unwrap();
        let scope = ContextScope {
            session_id: key.session_id,
            user_id: key.user_id,
        };
        assert!(is_owner(&scope));
        for bad in [
            ContextScope {
                user_id: "owner".into(),
                ..scope.clone()
            },
            ContextScope {
                session_id: "qq:other".into(),
                ..scope.clone()
            },
            ContextScope {
                session_id: format!("{PREFIX}../other"),
                ..scope.clone()
            },
        ] {
            assert!(!is_owner(&bad));
        }
    }
}
