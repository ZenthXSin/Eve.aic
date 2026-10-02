//! 有界认知循环公开契约；执行权限和实际预算由受信宿主装配。
use eve_cognition_api::{CognitionError, ExecutionAttempt, Goal, ReadAccess, SourceKind};
use eve_control_api::{ControlReport, GenerationKey};
use eve_llm_api::BudgetUsage;
use std::{fmt, future::Future, pin::Pin, sync::Arc};

pub const LOOP_PLUGIN_ID: &str = "eve.cognition.loop";
pub const LOOP_STATUS_SERVICE_ID: &str = "eve.cognition.loop.status.v1";
pub const LOOP_WAKE_EVENT_ID: &str = "eve.cognition.wake.v1";
pub const LOOP_FEEDBACK_EVENT_ID: &str = "eve.cognition.feedback.v1";
pub type LoopResult<T> = Result<T, LoopError>;
pub type LoopFuture<'a, T> = Pin<Box<dyn Future<Output = LoopResult<T>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoopError {
    InvalidInput,
    Unavailable,
    Cognition(CognitionError),
    Execution,
    Verification,
    LimitReached,
}
impl From<CognitionError> for LoopError {
    fn from(error: CognitionError) -> Self {
        Self::Cognition(error)
    }
}
impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "认知循环输入无效",
            Self::Unavailable => "认知循环不可用",
            Self::Cognition(_) => "认知循环状态读取或保存失败",
            Self::Execution => "认知执行状态不可用",
            Self::Verification => "认知验证未通过",
            Self::LimitReached => "认知循环预算已耗尽",
        })
    }
}
impl std::error::Error for LoopError {}

#[derive(Clone)]
pub struct AllowedSource {
    pub kind: SourceKind,
    pub channel: String,
}
#[derive(Clone)]
pub struct ExecutionScope {
    pub subject_id: String,
    pub access: ReadAccess,
    pub sources: Vec<AllowedSource>,
}
impl ExecutionScope {
    pub fn permits(&self, goal: &Goal) -> bool {
        goal.visibility.visible_to(&self.access)
            && self.sources.iter().any(|source| {
                source.kind == goal.source.kind && source.channel == goal.source.channel
            })
    }
    pub fn validate(&self) -> LoopResult<()> {
        eve_cognition_api::validate_id(&self.subject_id)?;
        self.access.validate()?;
        if self.sources.is_empty() || self.sources.len() > 32 {
            return Err(LoopError::InvalidInput);
        }
        for source in &self.sources {
            eve_cognition_api::validate_id(&source.channel)?;
        }
        Ok(())
    }
}
impl fmt::Debug for ExecutionScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExecutionScope(<redacted>)")
    }
}
#[derive(Clone, Debug)]
pub struct LoopOptions {
    pub scope: ExecutionScope,
    pub poll_interval_ms: u64,
    /// 单次启动最多准入多少个目标，每个目标最多一次尝试；重启不恢复旧尝试。
    pub max_executions: u16,
}
impl LoopOptions {
    pub fn validate(&self) -> LoopResult<()> {
        self.scope.validate()?;
        if !(10..=60_000).contains(&self.poll_interval_ms)
            || !(1..=32).contains(&self.max_executions)
        {
            return Err(LoopError::InvalidInput);
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct RankedGoal {
    pub goal_id: String,
    pub strength: u8,
    pub reason: String,
}
/// 只接收宿主已按来源、范围、状态与验证能力过滤的候选。
pub trait DrivePolicy: Send + Sync {
    fn rank(&self, goals: &[Goal], now_ms: u64) -> LoopResult<Vec<RankedGoal>>;
}
#[derive(Clone)]
pub struct GoalExecutionReport {
    pub control: ControlReport,
    pub usage: BudgetUsage,
}
impl fmt::Debug for GoalExecutionReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GoalExecutionReport(<redacted>)")
    }
}
/// submit 必须在实际执行前安装独立预算；wait 返回前确认工具/会话收尾。
/// 未知状态不能伪造为零调用；返回错误时不自动重新提交。
pub trait GoalExecutor: Send + Sync {
    fn submit(&self, goal: &Goal, attempt: &ExecutionAttempt) -> LoopResult<GenerationKey>;
    fn cancel(&self, key: &GenerationKey) -> LoopResult<()>;
    fn wait(&self, key: &GenerationKey) -> LoopFuture<'static, GoalExecutionReport>;
}
pub trait GoalVerifier: Send + Sync {
    fn supports(&self, goal: &Goal) -> bool;
    fn verify(&self, goal: &Goal, report: &GoalExecutionReport) -> LoopResult<bool>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeReason {
    StateChanged,
    Timer,
    Startup,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopStats {
    pub wakes: u64,
    pub coalesced_wakes: u64,
    pub evaluations: u64,
    pub submitted: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub blocked: u64,
    pub idle_ticks: u64,
    pub model_requests: u64,
    pub admitted_tool_calls: u64,
    pub started_tools: u64,
    pub feedback_save_failures: u64,
    pub notification_failures: u64,
    pub last_wakeup_to_admission_us: u64,
    pub last_cancel_settle_us: u64,
    pub active: bool,
}
pub trait LoopStatus: Send + Sync {
    fn stats(&self) -> LoopResult<LoopStats>;
}
#[derive(Clone)]
pub struct LoopStatusHandle(pub Arc<dyn LoopStatus>);
/// 仅宿主持有；不发布到通用服务目录。
pub trait LoopControl: LoopStatus {
    /// false 表示已有待处理信号，被合并而不是新增队列项。
    fn wake(&self, reason: WakeReason) -> LoopResult<bool>;
    /// 只发送取消；shutdown 返回后才确认后台执行和状态收尾。
    fn cancel_current(&self) -> LoopResult<bool>;
    /// 先关闭唤醒，再取消并等待；必须在停止 Kernel 之前调用。
    fn shutdown(&self) -> LoopFuture<'static, ()>;
}
