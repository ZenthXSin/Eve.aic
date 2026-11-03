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
pub(crate) struct OwnerAuthorization;
impl eve_developer_api::DeveloperAuthorization for OwnerAuthorization {
    fn is_operator(&self, scope: &ContextScope) -> bool {
        is_owner(scope)
    }
    fn allows_workspace_development(&self, scope: &ContextScope) -> bool {
        is_owner(scope)
    }
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
        Box::pin(async move {
            let owner = scope.as_ref().is_some_and(is_owner);
            let source = if owner {
                self.web.as_ref().unwrap_or(&self.inner)
            } else {
                &self.inner
            };
            let mut context = source.assemble_scoped(input, scope).await?;
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
    state: ChatState,
}
pub(crate) struct WebChat {
    control: Arc<dyn ControlService>,
    directory: PathBuf,
    claims: PathBuf,
    open: std::sync::atomic::AtomicBool,
    admission: Mutex<std::collections::BTreeMap<String, eve_control_api::GenerationKey>>,
}
fn unavailable<T>(_: T) -> PanelError {
    PanelError::Unavailable
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
        })
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
                && (receipt.state.text.is_none() || receipt.state.generation.is_none()))
            || receipt.state.conversation_id != id.conversation_id
            || receipt.state.request_id != id.request_id
            || receipt.fingerprint.len() != 64
            || !receipt.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
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
        let fingerprint: String = digest(&SHA256, request.text.as_bytes())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
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
            version: 1,
            fingerprint,
            state: ChatState {
                conversation_id: request.conversation_id,
                request_id: request.request_id,
                status: "accepted".into(),
                generation: None,
                turn_id: None,
                text: None,
                failure: None,
                started_tools: None,
                tool_receipts: 0,
            },
        };
        self.save(&receipt, true)?;
        let generation = self
            .control
            .submit(
                ControlInput {
                    session: SessionInput {
                        key: session,
                        text: request.text,
                    },
                    task_id: format!("web-request:{}", receipt.state.request_id),
                },
                Arc::new(DiscardControlEvents),
            )
            .map_err(control_error)?;
        active.insert(receipt.state.conversation_id.clone(), generation.clone());
        receipt.state.generation = Some(generation.clone());
        self.save(&receipt, false)?;
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
mod identity_tests {
    use super::*;
    use eve_developer_api::DeveloperAuthorization;
    #[test]
    fn owner_identity_requires_both_host_bound_fields_and_valid_conversation_id() {
        let key = key(&"a".repeat(32)).unwrap();
        let scope = ContextScope {
            session_id: key.session_id,
            user_id: key.user_id,
        };
        assert!(OwnerAuthorization.is_operator(&scope));
        assert!(OwnerAuthorization.allows_workspace_development(&scope));
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
            assert!(!OwnerAuthorization.is_operator(&bad));
        }
    }
}
