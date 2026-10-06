//! 统一认知状态的公开契约；不包含存储、调度、模型或执行权限实现。
mod goal_feedback;
pub use goal_feedback::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, sync::Arc};

pub const COGNITION_PLUGIN_ID: &str = "eve.cognition";
pub const COGNITION_READ_SERVICE_ID: &str = "eve.cognition.public.v1";
pub const COGNITION_FORMAT_VERSION: u32 = 1;
pub const MAX_RECORDS: usize = 256;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;
pub type CognitionResult<T> = Result<T, CognitionError>;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    User(String),
    Internal,
}
/// 只能由受信宿主绑定到读句柄；服务调用不接收调用方自报身份。
#[derive(Clone, Eq, PartialEq)]
pub enum ReadAccess {
    Public,
    User(String),
    Internal,
}
impl Visibility {
    pub fn visible_to(&self, access: &ReadAccess) -> bool {
        matches!(access, ReadAccess::Internal)
            || matches!(self, Self::Public)
            || matches!((self, access), (Self::User(a), ReadAccess::User(b)) if a == b)
    }
    /// 派生数据可以收窄可见范围，不能扩大输入的访问范围。
    pub fn restricts(&self, parent: &Self) -> bool {
        matches!(self, Self::Internal)
            || matches!(parent, Self::Public)
            || matches!((self, parent), (Self::User(a), Self::User(b)) if a == b)
    }
    pub fn validate(&self) -> CognitionResult<()> {
        if let Self::User(id) = self {
            validate_id(id)?;
        }
        Ok(())
    }
}
impl ReadAccess {
    pub fn validate(&self) -> CognitionResult<()> {
        if let Self::User(id) = self {
            validate_id(id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SourceKind {
    User,
    Environment,
    Tool,
    Inference,
    Internal,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub kind: SourceKind,
    pub channel: String,
    pub reference: String,
}
impl Source {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.channel)?;
        validate_id(&self.reference)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionBudget {
    pub max_model_requests: u16,
    pub max_tool_calls: u16,
    pub max_attempts: u16,
    pub timeout_ms: u64,
}
impl ExecutionBudget {
    pub fn validate(&self) -> CognitionResult<()> {
        if !(1..=100).contains(&self.max_model_requests)
            || self.max_tool_calls > 1000
            || !(1..=10).contains(&self.max_attempts)
            || !(1..=600_000).contains(&self.timeout_ms)
        {
            return Err(CognitionError::InvalidInput);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalStatus {
    Ready,
    Waiting,
    Executing,
    Completed,
    Cancelled,
    Blocked,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BlockReason {
    Interrupted,
    UnknownCommit,
    FeedbackSaveFailed,
    Invalidated,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionCommit {
    NotStarted,
    Completed,
    Failed,
    Pending,
    Unknown,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAttempt {
    pub attempt_id: String,
    pub session_id: String,
    pub task_id: String,
    pub turn_id: Option<u64>,
    pub started_at_ms: u64,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    pub commit: ExecutionCommit,
    pub verification_met: bool,
    pub started_tools: Option<u64>,
    pub summary: String,
    pub at_ms: u64,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub id: String,
    pub revision: u64,
    pub source: Source,
    pub visibility: Visibility,
    pub description: String,
    pub verification: String,
    pub priority: u8,
    pub budget: ExecutionBudget,
    pub stop_condition: String,
    pub expires_at_ms: Option<u64>,
    pub status: GoalStatus,
    pub wait_reason: Option<String>,
    pub block_reason: Option<BlockReason>,
    pub execution: Option<ExecutionAttempt>,
    pub feedback: Option<Feedback>,
}
impl Goal {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.id)?;
        self.source.validate()?;
        self.visibility.validate()?;
        for text in [&self.description, &self.verification, &self.stop_condition] {
            validate_text(text)?;
        }
        self.budget.validate()?;
        if self.priority > 100 || self.expires_at_ms == Some(0) {
            return Err(CognitionError::InvalidInput);
        }
        if let Some(reason) = &self.wait_reason {
            validate_text(reason)?;
        }
        if let Some(execution) = &self.execution {
            for id in [
                &execution.attempt_id,
                &execution.session_id,
                &execution.task_id,
            ] {
                validate_id(id)?;
            }
            if execution.turn_id == Some(0) || execution.started_at_ms == 0 {
                return Err(CognitionError::InvalidInput);
            }
        }
        if let Some(feedback) = &self.feedback {
            validate_text(&feedback.summary)?;
            if feedback.at_ms == 0
                || (feedback.verification_met && feedback.commit != ExecutionCommit::Completed)
            {
                return Err(CognitionError::InvalidInput);
            }
        }
        if (self.status == GoalStatus::Waiting) != self.wait_reason.is_some()
            || (self.status == GoalStatus::Blocked) != self.block_reason.is_some()
        {
            return Err(CognitionError::InvalidInput);
        }
        match self.status {
            GoalStatus::Ready | GoalStatus::Waiting => {
                if self.execution.is_some() || self.feedback.is_some() {
                    return Err(CognitionError::InvalidInput);
                }
            }
            GoalStatus::Executing => {
                if self.execution.is_none() || self.feedback.is_some() {
                    return Err(CognitionError::InvalidInput);
                }
            }
            GoalStatus::Completed => {
                if self.execution.as_ref().is_none_or(|e| e.turn_id.is_none())
                    || !self.feedback.as_ref().is_some_and(|f| {
                        f.commit == ExecutionCommit::Completed && f.verification_met
                    })
                {
                    return Err(CognitionError::InvalidInput);
                }
            }
            GoalStatus::Cancelled => {
                if self.feedback.as_ref().is_some_and(|f| {
                    matches!(
                        f.commit,
                        ExecutionCommit::Pending | ExecutionCommit::Unknown
                    )
                }) {
                    return Err(CognitionError::InvalidInput);
                }
            }
            GoalStatus::Blocked => {}
        }
        Ok(())
    }
    pub fn is_ready(&self, now_ms: u64) -> bool {
        self.status == GoalStatus::Ready
            && self.expires_at_ms.is_none_or(|expires| now_ms < expires)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Drive {
    pub id: String,
    pub visibility: Visibility,
    pub goal_ids: Vec<String>,
    pub strength: u8,
    pub reason: String,
    pub evaluated_at_ms: u64,
    pub valid_until_ms: u64,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agenda {
    pub visibility: Visibility,
    pub candidates: Vec<String>,
    pub selected: Option<String>,
    pub reason: String,
    pub valid_until_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CognitiveEventKind {
    ExternalInput,
    StateChanged,
    DriveEvaluated,
    AgendaSelected,
    Feedback,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveEvent {
    pub id: String,
    pub kind: CognitiveEventKind,
    pub source: Source,
    pub visibility: Visibility,
    pub goal_id: Option<String>,
    pub caused_by: Option<String>,
    pub at_ms: u64,
    pub summary: String,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveState {
    pub goals: BTreeMap<String, Goal>,
    pub drives: BTreeMap<String, Drive>,
    pub agenda: Option<Agenda>,
    pub events: Vec<CognitiveEvent>,
}
impl CognitiveState {
    pub fn validate(&self) -> CognitionResult<()> {
        if self.goals.len() > MAX_RECORDS
            || self.drives.len() > MAX_RECORDS
            || self.events.len() > MAX_RECORDS
        {
            return Err(CognitionError::LimitReached);
        }
        let mut executing = 0;
        for (id, goal) in &self.goals {
            goal.validate()?;
            if id != &goal.id {
                return Err(CognitionError::InvalidInput);
            }
            executing += usize::from(goal.status == GoalStatus::Executing);
        }
        if executing > 1 {
            return Err(CognitionError::Busy);
        }
        for (id, drive) in &self.drives {
            validate_id(id)?;
            drive.visibility.validate()?;
            validate_text(&drive.reason)?;
            if id != &drive.id
                || drive.strength > 100
                || drive.evaluated_at_ms == 0
                || drive.valid_until_ms <= drive.evaluated_at_ms
                || drive.goal_ids.is_empty()
            {
                return Err(CognitionError::InvalidInput);
            }
            self.validate_goal_refs(&drive.goal_ids, &drive.visibility)?;
        }
        if let Some(agenda) = &self.agenda {
            agenda.visibility.validate()?;
            validate_text(&agenda.reason)?;
            if agenda.valid_until_ms == 0 {
                return Err(CognitionError::InvalidInput);
            }
            self.validate_goal_refs(&agenda.candidates, &agenda.visibility)?;
            if agenda
                .candidates
                .iter()
                .any(|id| self.goals[id].status != GoalStatus::Ready)
            {
                return Err(CognitionError::InvalidInput);
            }
            if let Some(id) = &agenda.selected {
                self.validate_goal_refs(std::slice::from_ref(id), &agenda.visibility)?;
                let status = &self.goals[id].status;
                if !matches!(status, GoalStatus::Ready | GoalStatus::Executing)
                    || (*status == GoalStatus::Ready && !agenda.candidates.contains(id))
                {
                    return Err(CognitionError::InvalidInput);
                }
            }
        }
        let mut events = BTreeMap::<&str, &Visibility>::new();
        for event in &self.events {
            validate_id(&event.id)?;
            event.source.validate()?;
            event.visibility.validate()?;
            validate_text(&event.summary)?;
            if event.at_ms == 0 || events.contains_key(event.id.as_str()) {
                return Err(CognitionError::InvalidInput);
            }
            if let Some(id) = &event.goal_id {
                self.validate_goal_refs(std::slice::from_ref(id), &event.visibility)?;
            }
            if let Some(cause) = &event.caused_by {
                let parent = events
                    .get(cause.as_str())
                    .ok_or(CognitionError::InvalidInput)?;
                if !event.visibility.restricts(parent) {
                    return Err(CognitionError::AccessDenied);
                }
            }
            events.insert(&event.id, &event.visibility);
        }
        Ok(())
    }
    fn validate_goal_refs(&self, ids: &[String], visibility: &Visibility) -> CognitionResult<()> {
        if ids.len() > MAX_RECORDS {
            return Err(CognitionError::LimitReached);
        }
        let mut unique = std::collections::BTreeSet::new();
        for id in ids {
            if !unique.insert(id) {
                return Err(CognitionError::InvalidInput);
            }
            let goal = self.goals.get(id).ok_or(CognitionError::InvalidInput)?;
            if !visibility.restricts(&goal.visibility) {
                return Err(CognitionError::AccessDenied);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitiveSnapshot {
    pub format_version: u32,
    pub subject_id: String,
    pub revision: u64,
    pub state: CognitiveState,
}
/// 已绑定读权限的视图，正文与派生数据均按可见范围过滤。
#[derive(Clone, Eq, PartialEq)]
pub struct CognitiveView {
    pub subject_id: String,
    /// 全局修订可用于检测变更；它不表示可见记录数量。
    pub revision: u64,
    pub state: CognitiveState,
}
impl CognitiveView {
    pub fn ready_goal_ids(&self, now_ms: u64) -> Vec<String> {
        self.state
            .goals
            .iter()
            .filter(|(_, g)| g.is_ready(now_ms))
            .map(|(id, _)| id.clone())
            .collect()
    }
}
pub trait CognitionReader: Send + Sync {
    fn snapshot(&self) -> CognitionResult<CognitiveView>;
}
/// 宿主专用管理能力；不得注册到通用服务目录或传给模型/通道。
pub trait CognitionAdmin: Send + Sync {
    fn snapshot(&self) -> CognitionResult<CognitiveSnapshot>;
    fn reader(&self, access: ReadAccess) -> CognitionResult<Arc<dyn CognitionReader>>;
    /// 成功后整份状态已持久化；目标修订由实现自动增加。
    fn replace(
        &self,
        expected_revision: u64,
        state: CognitiveState,
    ) -> CognitionResult<CognitiveSnapshot>;
}
#[derive(Clone)]
pub struct CognitionReadHandle(pub Arc<dyn CognitionReader>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CognitionError {
    InvalidInput,
    AccessDenied,
    SubjectMismatch,
    StaleRevision,
    InvalidTransition,
    Busy,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
}
impl fmt::Display for CognitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "认知输入或结构无效",
            Self::AccessDenied => "认知数据访问范围不允许扩大",
            Self::SubjectMismatch => "认知主体不匹配",
            Self::StaleRevision => "认知状态修订已失效",
            Self::InvalidTransition => "目标状态变更不允许",
            Self::Busy => "认知主体已有在途目标",
            Self::Unavailable => "认知服务不可用",
            Self::CorruptState => "认知状态损坏",
            Self::UnsupportedVersion => "不支持认知状态版本",
            Self::Storage => "认知状态读取或提交失败",
            Self::LimitReached => "认知状态或计数已达上限",
        })
    }
}
impl std::error::Error for CognitionError {}
pub fn validate_id(id: &str) -> CognitionResult<()> {
    if id.is_empty() || id.len() > 256 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(CognitionError::InvalidInput);
    }
    Ok(())
}
pub fn validate_text(text: &str) -> CognitionResult<()> {
    if text.trim().is_empty() || text.len() > 8192 || text.contains('\0') {
        return Err(CognitionError::InvalidInput);
    }
    Ok(())
}

macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
redacted_debug!(
    Visibility,
    ReadAccess,
    Source,
    ExecutionAttempt,
    Feedback,
    Goal,
    Drive,
    Agenda,
    CognitiveEvent,
    CognitiveState,
    CognitiveSnapshot,
    CognitiveView
);
