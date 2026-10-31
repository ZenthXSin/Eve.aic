//! 内置会话插件，通过 StateStore 保存版本化历史；不调用模型或依赖文件实现。
mod archive;
mod cold;
mod strict_json;
pub use archive::FileSessionArchive;
use eve_llm_api::{ChatMessage, ChatRole};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    ServiceId, cleanup,
};
use eve_session_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Unbounded},
    sync::{Arc, Mutex, MutexGuard},
};

/// 失败输入仍可解释后续“进展如何”“换个方式再来”，但宿主告知未完成，工具调用绝不重放。
/// 取消轮不回放；确认没有启动工具的失败轮也回放输入，否则下一轮“分几次写吧”之类的接续会失去所指。
fn history_with_failures(snapshot: &SessionSnapshot, through: u64) -> Vec<ChatMessage> {
    snapshot.turns.iter().filter(|turn| turn.id > through).flat_map(|turn| match &turn.status {
            SessionTurnStatus::Completed { messages } => messages.clone(),
            SessionTurnStatus::Failed { failure } if failure.code != SessionFailureCode::Cancelled => {
                let evidence = failure.evidence.as_ref();
                let http = evidence.and_then(|e| e.provider_http_status)
                    .map_or(String::new(), |s| format!("，模型服务 HTTP {s}"));
                let reason = match evidence.and_then(|e| e.provider_failure) {
                    Some(eve_llm_api::ProviderFailureKind::Truncated) => "，回复达到单次输出上限、没有生成完".to_string(),
                    Some(kind) => format!("，类别 {kind:?}"),
                    None => String::new(),
                };
                let note = match failure.started_tools {
                    Some(0) => format!("【宿主失败记录，非模型回答】本轮未完成：{:?}{http}{reason}；没有启动工具调用，也没有发出回复正文。用户这条请求仍未完成；用户接着补充或要求换个方式时，按这条请求理解，不要声称已经完成。", failure.code),
                    started => {
                        let started = started.map_or("执行数量未知".into(), |n| format!("已启动 {n} 次工具调用"));
                        format!("【宿主失败记录，非模型回答】本轮未完成：{:?}{http}{reason}；{started}。已完成的工具结果保留在失败账本，副作用不会撤销。不得声称尚未开始、任务已经完成或仍在后台继续；先核对实际状态，不能自动重放工具。", failure.code)
                    }
                };
                vec![ChatMessage::text(ChatRole::User, turn.input.clone()), ChatMessage::text(ChatRole::Assistant, note)]
            }
            _ => vec![],
        }).collect()
}

pub const SESSION_STATE_KEY: &str = "sessions.v1";
const FORMAT_VERSION: u32 = 1;
pub const CONTEXT_RESETS_STATE_KEY: &str = "context-resets.v1";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextBoundary {
    key: SessionKey,
    through: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextResets {
    format_version: u32,
    boundaries: BTreeMap<String, ContextBoundary>,
}
impl Default for ContextResets {
    fn default() -> Self {
        Self {
            format_version: 1,
            boundaries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    sessions: BTreeMap<String, SessionSnapshot>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            sessions: BTreeMap::new(),
        }
    }
}
struct Inner {
    document: Document,
    resets: ContextResets,
    active: bool,
}
fn deactivate(inner: &mut Inner) {
    inner.active = false;
    // 停止后的句柄只能拒绝访问，原始字节仍在后端；游标不再持有整份旧正文。
    inner.document.sessions.clear();
    inner.resets.boundaries.clear();
}
struct TurnCursor {
    inner: Arc<Mutex<Inner>>,
    summary: SessionHistorySummary,
    through: usize,
    next: usize,
    pending: Option<SessionTurn>,
    failed: bool,
}
impl SessionTurnCursor for TurnCursor {
    fn summary(&self) -> &SessionHistorySummary {
        &self.summary
    }
    fn next_page(&mut self) -> SessionResult<Vec<SessionTurn>> {
        if self.failed {
            return Err(SessionError::Unavailable);
        }
        let result = (|| {
            let inner = self.inner.lock().map_err(|_| SessionError::Unavailable)?;
            if !inner.active {
                return Err(SessionError::Unavailable);
            }
            let current = inner
                .document
                .sessions
                .get(&self.summary.key.session_id)
                .ok_or(SessionError::CorruptState)?;
            if current.key != self.summary.key {
                return Err(SessionError::OwnerMismatch);
            }
            if current.revision < self.summary.revision || current.turns.len() < self.through {
                return Err(SessionError::CorruptState);
            }
            let end = self
                .next
                .saturating_add(MAX_SESSION_TURN_PAGE)
                .min(self.through);
            let page = (self.next..end)
                .map(|index| {
                    // 已终结的前缀不可更改；唯一可变的末尾 Pending 在创建时单独固定。
                    if index + 1 == self.through
                        && let Some(pending) = &self.pending
                    {
                        pending.clone()
                    } else {
                        current.turns[index].clone()
                    }
                })
                .collect();
            self.next = end;
            Ok(page)
        })();
        if result.is_err() {
            self.failed = true;
            self.pending = None;
            if let Ok(mut inner) = self.inner.lock() {
                deactivate(&mut inner);
            }
        }
        result
    }
}
struct StoredSessions {
    context: PluginContext,
    inner: Arc<Mutex<Inner>>,
}
impl StoredSessions {
    fn open(context: PluginContext) -> SessionResult<Self> {
        let mut document: Document = match context
            .state_get(SESSION_STATE_KEY)
            .map_err(|_| SessionError::Storage)?
        {
            None => Document::default(),
            Some(bytes) => {
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| SessionError::CorruptState)?;
                serde_json::from_value(value).map_err(|_| SessionError::CorruptState)?
            }
        };
        if document.format_version != FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion);
        }
        let resets: ContextResets = match context
            .state_get(CONTEXT_RESETS_STATE_KEY)
            .map_err(|_| SessionError::Storage)?
        {
            None => ContextResets::default(),
            Some(bytes) => serde_json::from_value(
                strict_json::from_slice(&bytes).map_err(|_| SessionError::CorruptState)?,
            )
            .map_err(|_| SessionError::CorruptState)?,
        };
        if resets.format_version != 1 {
            return Err(SessionError::UnsupportedVersion);
        }
        for (id, boundary) in &resets.boundaries {
            let snapshot = document
                .sessions
                .get(id)
                .ok_or(SessionError::CorruptState)?;
            if boundary.key != snapshot.key
                || boundary.through == 0
                || snapshot
                    .turns
                    .last()
                    .is_none_or(|turn| boundary.through > turn.id)
            {
                return Err(SessionError::CorruptState);
            }
        }
        let mut recovered = false;
        for (id, snapshot) in &mut document.sessions {
            snapshot
                .validate()
                .map_err(|_| SessionError::CorruptState)?;
            if id != &snapshot.key.session_id {
                return Err(SessionError::CorruptState);
            }
            if let Some(turn) = snapshot.turns.last_mut()
                && turn.status == SessionTurnStatus::Pending
            {
                turn.status = SessionTurnStatus::Interrupted;
                snapshot.revision = snapshot
                    .revision
                    .checked_add(1)
                    .ok_or(SessionError::LimitReached)?;
                recovered = true;
            }
        }
        if recovered {
            let bytes = serde_json::to_vec(&document).map_err(|_| SessionError::CorruptState)?;
            context
                .state_set(SESSION_STATE_KEY, bytes)
                .map_err(|_| SessionError::Storage)?;
        }
        Ok(Self {
            context,
            inner: Arc::new(Mutex::new(Inner {
                document,
                resets,
                active: true,
            })),
        })
    }
    fn lock(&self) -> SessionResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| SessionError::Unavailable)?;
        if !inner.active {
            return Err(SessionError::Unavailable);
        }
        Ok(inner)
    }
    fn save(&self, inner: &mut Inner, document: Document) -> SessionResult<()> {
        let bytes = serde_json::to_vec(&document).map_err(|_| SessionError::CorruptState)?;
        if self.context.state_set(SESSION_STATE_KEY, bytes).is_err() {
            // 后端可能已提交但确认丢失；旧缓存不能再覆盖实际状态或重用轮次 ID。
            deactivate(inner);
            return Err(SessionError::Storage);
        }
        inner.document = document;
        Ok(())
    }
    fn finish(&self, lease: &TurnLease, status: SessionTurnStatus) -> SessionResult<()> {
        lease.key.validate()?;
        let mut inner = self.lock()?;
        let mut next = inner.document.clone();
        let snapshot = next
            .sessions
            .get_mut(&lease.key.session_id)
            .ok_or(SessionError::StaleTurn)?;
        if snapshot.key != lease.key {
            return Err(SessionError::OwnerMismatch);
        }
        let turn = snapshot.turns.last_mut().ok_or(SessionError::StaleTurn)?;
        if turn.id != lease.turn_id || turn.status != SessionTurnStatus::Pending {
            return Err(SessionError::StaleTurn);
        }
        if let SessionTurnStatus::Completed { messages } = &status {
            validate_completed_turn(&turn.input, messages)?;
        }
        if let SessionTurnStatus::Failed { failure } = &status {
            failure.validate()?;
        }
        turn.status = status;
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        self.save(&mut inner, next)
    }
    fn close(&self) -> PluginResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("会话锁不可用".into()))?;
        deactivate(&mut inner);
        Ok(())
    }
}
impl SessionService for StoredSessions {
    fn scan_turns(&self, key: &SessionKey) -> SessionResult<Option<Box<dyn SessionTurnCursor>>> {
        key.validate()?;
        let inner = self.lock()?;
        let Some(snapshot) = inner.document.sessions.get(&key.session_id) else {
            return Ok(None);
        };
        if snapshot.key != *key {
            return Err(SessionError::OwnerMismatch);
        }
        Ok(Some(Box::new(TurnCursor {
            inner: self.inner.clone(),
            summary: SessionHistorySummary {
                key: key.clone(),
                revision: snapshot.revision,
                turn_count: u64::try_from(snapshot.turns.len())
                    .map_err(|_| SessionError::LimitReached)?,
            },
            through: snapshot.turns.len(),
            next: 0,
            pending: snapshot
                .turns
                .last()
                .filter(|t| matches!(t.status, SessionTurnStatus::Pending))
                .cloned(),
            failed: false,
        })))
    }

    fn reset_context(&self, key: Option<&SessionKey>) -> SessionResult<usize> {
        let mut inner = self.lock()?;
        if let Some(key) = key {
            key.validate()?;
            if inner
                .document
                .sessions
                .get(&key.session_id)
                .is_some_and(|s| s.key != *key)
            {
                return Err(SessionError::OwnerMismatch);
            }
        }
        let mut next = inner.resets.clone();
        let mut affected = 0;
        for (id, snapshot) in &inner.document.sessions {
            if key.is_none_or(|key| key == &snapshot.key)
                && let Some(last) = snapshot.turns.last()
            {
                next.boundaries.insert(
                    id.clone(),
                    ContextBoundary {
                        key: snapshot.key.clone(),
                        through: last.id,
                    },
                );
                affected += 1;
            }
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| SessionError::CorruptState)?;
        if self
            .context
            .state_set(CONTEXT_RESETS_STATE_KEY, bytes)
            .is_err()
        {
            // 写入结果可能不确定，停止旧句柄，不能继续携带可能已清空的上下文。
            deactivate(&mut inner);
            return Err(SessionError::Storage);
        }
        inner.resets = next;
        Ok(affected)
    }
    fn list_keys(&self, after: Option<&str>, limit: usize) -> SessionResult<Vec<SessionKey>> {
        let inner = self.lock()?;
        if !(1..=100).contains(&limit)
            || after.is_some_and(|cursor| {
                cursor.is_empty() || cursor.len() > 256 || cursor.chars().any(char::is_control)
            })
        {
            return Err(SessionError::InvalidInput);
        }
        let start = after.map_or(Unbounded, Excluded);
        Ok(inner
            .document
            .sessions
            .range::<str, _>((start, Unbounded))
            .take(limit)
            .map(|(_, snapshot)| snapshot.key.clone())
            .collect())
    }
    fn snapshot(&self, key: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
        key.validate()?;
        let inner = self.lock()?;
        let snapshot = inner.document.sessions.get(&key.session_id);
        if snapshot.is_some_and(|s| s.key != *key) {
            return Err(SessionError::OwnerMismatch);
        }
        Ok(snapshot.cloned())
    }
    fn begin(&self, input: SessionInput) -> SessionResult<StartedTurn> {
        input.key.validate()?;
        if input.text.trim().is_empty() {
            return Err(SessionError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let through = inner
            .resets
            .boundaries
            .get(&input.key.session_id)
            .map_or(0, |b| b.through);
        let mut next = inner.document.clone();
        let snapshot = next
            .sessions
            .entry(input.key.session_id.clone())
            .or_insert_with(|| SessionSnapshot {
                key: input.key.clone(),
                revision: 0,
                turns: vec![],
            });
        if snapshot.key != input.key {
            return Err(SessionError::OwnerMismatch);
        }
        if snapshot
            .turns
            .last()
            .is_some_and(|turn| turn.status == SessionTurnStatus::Pending)
        {
            return Err(SessionError::Busy);
        }
        let id = snapshot.turns.last().map_or(Ok(1), |turn| {
            turn.id.checked_add(1).ok_or(SessionError::LimitReached)
        })?;
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        snapshot.turns.push(SessionTurn {
            id,
            input: input.text,
            status: SessionTurnStatus::Pending,
        });
        let result = StartedTurn {
            lease: TurnLease {
                key: input.key,
                turn_id: id,
            },
            revision: snapshot.revision,
            history: history_with_failures(snapshot, through),
        };
        self.save(&mut inner, next)?;
        Ok(result)
    }
    fn complete(&self, lease: &TurnLease, messages: Vec<ChatMessage>) -> SessionResult<()> {
        self.finish(lease, SessionTurnStatus::Completed { messages })
    }
    fn fail(&self, lease: &TurnLease, failure: SessionFailure) -> SessionResult<()> {
        self.finish(lease, SessionTurnStatus::Failed { failure })
    }
}

pub struct SessionPlugin {
    manifest: PluginManifest,
    archive: Option<Arc<dyn SessionArchiveStore>>,
}
impl SessionPlugin {
    pub fn with_archive(mut self, archive: Arc<dyn SessionArchiveStore>) -> Self {
        self.archive = Some(archive);
        self
    }
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(SESSION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            archive: None,
        })
    }
}
impl Plugin for SessionPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            if let Some(archive) = &self.archive {
                let service = Arc::new(
                    cold::ColdSessions::open(context.clone(), archive.clone())
                        .map_err(|error| PluginError::State(error.to_string()))?,
                );
                let to_close = service.clone();
                context.cleanup(cleanup(move || async move { to_close.close() }))?;
                context.provide_service(
                    ServiceId::new(SESSION_SERVICE_ID)?,
                    SessionServiceHandle(service),
                )?;
                return Ok(None);
            }
            let service = Arc::new(
                StoredSessions::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = service.clone();
            context.cleanup(cleanup(move || async move { to_close.close() }))?;
            context.provide_service(
                ServiceId::new(SESSION_SERVICE_ID)?,
                SessionServiceHandle(service),
            )?;
            Ok(None)
        })
    }
}
