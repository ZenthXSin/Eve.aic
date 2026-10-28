//! 单一 Eve 主体的可信宿主契约。所有身份、来源和范围必须由入口绑定，不能从模型参数读取。
//! 主体统一调度，不合并不同会话原文；面向会话的读取只允许完全相同的范围。
use serde::{Deserialize, Serialize};
use std::fmt;

pub const SUBJECT_PLUGIN_ID: &str = "eve.subject";
pub const SUBJECT_STATE_KEY: &str = "subject.v1";
pub const MAX_EVENTS: usize = 256;
pub const MAX_FOCI: usize = 128;
pub const MAX_PAYLOAD_BYTES: usize = 8192;
pub const MAX_CHECKPOINT_BYTES: usize = 16_384;
pub const MAX_STATE_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_DEDUPE_KEYS: usize = 4096;
/// 已了结事件被容量回收后，来源键仍保留七天；窗口内从不静默遗忘去重键。
pub const DEDUPE_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const AUTONOMY_INTERVAL_MS: u64 = 30 * 60 * 1000;
pub const MAX_LEASE_MS: u64 = 5 * 60 * 1000;
pub const MAX_RECOVERIES: u32 = 3;

pub type SubjectResult<T> = Result<T, SubjectError>;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectScope {
    pub channel: String,
    pub conversation_id: String,
    pub principal_id: String,
}

/// 排序顺序就是调度优先级；停止/取消始终先于用户输入、恢复和自主探索。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPriority {
    Stop,
    User,
    Recovery,
    Autonomy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    UserIngress,
    TaskExecutor,
    RecoverySupervisor,
    AutonomyScheduler,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectEventInput {
    pub scope: SubjectScope,
    pub source: EventSource,
    pub dedupe_key: String,
    pub priority: EventPriority,
    /// 宿主定义的事件类型，例如 message、task_completed、focus_check。
    pub kind: String,
    /// 历史数据，不得提升成系统指令；只向相同范围的执行上下文公开。
    pub payload: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
    Unknown,
}
impl EventStatus {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointBoundary {
    /// 已记录工具结果/尚未开始副作用，可从 resume_data 继续。
    Safe,
    /// 已准备执行副作用，结果尚未记录；崩溃后未知，禁止自动重放。
    EffectPending,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectCheckpoint {
    pub boundary: CheckpointBoundary,
    pub resume_data: String,
}
impl Default for SubjectCheckpoint {
    fn default() -> Self {
        Self {
            boundary: CheckpointBoundary::Safe,
            resume_data: String::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseToken {
    pub event_id: String,
    pub generation: u64,
    pub worker: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectLease {
    pub token: LeaseToken,
    pub expires_at_ms: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectEvent {
    pub id: String,
    pub input: SubjectEventInput,
    pub status: EventStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub lease: Option<SubjectLease>,
    pub checkpoint: SubjectCheckpoint,
    pub recoveries: u32,
    /// 高优先级输入到达或本范围被取消；执行器必须在下个安全边界检查。
    pub interrupt_requested: bool,
    pub cancel_requested: bool,
    pub outcome: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SubjectOutcome {
    Completed(String),
    Failed(String),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusStatus {
    Active,
    Blocked,
    Completed,
    Cancelled,
}

/// 持久目标/任务关注。全局账本管理所有关注，原文与目标始终保留来源范围。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectFocus {
    pub id: String,
    pub scope: SubjectScope,
    pub title: String,
    pub goal: String,
    pub task_id: Option<String>,
    pub status: FocusStatus,
    pub next_check_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SubjectSnapshot {
    pub events: Vec<SubjectEvent>,
    pub foci: Vec<SubjectFocus>,
}

/// 只交给可信宿主。模型工具须绑定 scope 的包装器，不能直接发布此管理能力。
pub trait SubjectAdmin: Send + Sync {
    fn enqueue(&self, input: SubjectEventInput, now_ms: u64) -> SubjectResult<SubjectEvent>;
    /// 兼容已有同步入口：原子记录已经处理的输入，不将它再次放入执行队列。
    fn observe_completed(
        &self,
        input: SubjectEventInput,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent>;
    /// 同一主体同一时刻只租出一个事件；新租约会恢复过期租约。
    fn claim_next(
        &self,
        worker: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> SubjectResult<Option<SubjectEvent>>;
    fn checkpoint(
        &self,
        token: &LeaseToken,
        checkpoint: SubjectCheckpoint,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent>;
    fn renew(&self, token: &LeaseToken, now_ms: u64, lease_ms: u64) -> SubjectResult<SubjectEvent>;
    /// 安全边界上交回执行权，使已排队的高优先级事件先执行。
    fn yield_work(&self, token: &LeaseToken, now_ms: u64) -> SubjectResult<SubjectEvent>;
    fn settle(
        &self,
        token: &LeaseToken,
        outcome: SubjectOutcome,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent>;
    /// 已知无法确认外部副作用结果时，立即释放调度租约并保留未知事件，不等待租约超时。
    fn mark_unknown(
        &self,
        token: &LeaseToken,
        reason: &str,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent>;
    /// 只取消这个精确范围的排队事件；运行事件设置合作式取消标记。
    fn cancel_scope(&self, scope: &SubjectScope, now_ms: u64) -> SubjectResult<usize>;
    fn snapshot_for(&self, scope: &SubjectScope) -> SubjectResult<SubjectSnapshot>;
    fn upsert_focus(&self, focus: SubjectFocus, now_ms: u64) -> SubjectResult<SubjectFocus>;
    /// 只排入到期关注的检查事件，不产生通知；每个关注至少间隔三十分钟。
    fn schedule_due(&self, now_ms: u64) -> SubjectResult<usize>;
    /// 只能由可信宿主根据人工核对结果了结未知事件，不能令它重新排队。
    fn resolve_unknown(
        &self,
        scope: &SubjectScope,
        id: &str,
        outcome: SubjectOutcome,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectError {
    InvalidInput,
    LimitReached,
    NotFound,
    InvalidTransition,
    LeaseLost,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
}
impl fmt::Display for SubjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "主体输入无效",
            Self::LimitReached => "主体账本容量已满",
            Self::NotFound => "没有这个主体事件",
            Self::InvalidTransition => "主体状态不允许此操作",
            Self::LeaseLost => "主体执行租约已失效",
            Self::Unavailable => "主体服务不可用",
            Self::CorruptState => "主体状态损坏；未清空",
            Self::UnsupportedVersion => "主体状态版本不支持",
            Self::Storage => "主体状态提交无法确认；请重新打开核对",
        })
    }
}
impl std::error::Error for SubjectError {}

pub fn validate_id(value: &str) -> SubjectResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(SubjectError::InvalidInput);
    }
    Ok(())
}
pub fn validate_scope(scope: &SubjectScope) -> SubjectResult<()> {
    validate_id(&scope.channel)?;
    validate_id(&scope.conversation_id)?;
    validate_id(&scope.principal_id)
}
pub fn validate_input(input: &SubjectEventInput) -> SubjectResult<()> {
    validate_scope(&input.scope)?;
    validate_id(&input.dedupe_key)?;
    validate_id(&input.kind)?;
    if input.payload.len() > MAX_PAYLOAD_BYTES {
        return Err(SubjectError::InvalidInput);
    }
    let trusted_priority = match input.source {
        EventSource::UserIngress => {
            matches!(input.priority, EventPriority::Stop | EventPriority::User)
        }
        EventSource::TaskExecutor | EventSource::RecoverySupervisor => {
            input.priority == EventPriority::Recovery
        }
        EventSource::AutonomyScheduler => input.priority == EventPriority::Autonomy,
    };
    if !trusted_priority {
        return Err(SubjectError::InvalidInput);
    }
    Ok(())
}
pub fn validate_focus(focus: &SubjectFocus) -> SubjectResult<()> {
    validate_id(&focus.id)?;
    validate_scope(&focus.scope)?;
    if focus.id.len() > 200
        || focus.title.trim().is_empty()
        || focus.title.len() > 256
        || focus.goal.trim().is_empty()
        || focus.goal.len() > MAX_PAYLOAD_BYTES
        || focus.updated_at_ms == 0
    {
        return Err(SubjectError::InvalidInput);
    }
    if let Some(id) = &focus.task_id {
        validate_id(id)?;
    }
    Ok(())
}

// 默认调试输出不含原文、任务内容和检查点。
impl fmt::Debug for SubjectEventInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubjectEventInput")
            .field("source", &self.source)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SubjectCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubjectCheckpoint")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SubjectEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubjectEvent")
            .field("id", &self.id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SubjectFocus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubjectFocus")
            .field("id", &self.id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SubjectSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubjectSnapshot")
            .field("events", &self.events.len())
            .field("foci", &self.foci.len())
            .finish()
    }
}
