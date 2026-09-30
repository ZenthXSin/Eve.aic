//! 任务控制定义层；通道与判断插件只依赖这里，不依赖控制器或模型实现。
use eve_llm_api::{ChatMessage, LlmError, LlmFuture, ToolResult, TurnEvent, TurnEventSink};
use eve_session_api::{SessionError, SessionInput, SessionKey};
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, pin::Pin, sync::Arc};

pub const CONTROL_PLUGIN_ID: &str = "eve.control";
pub const CONTROL_SERVICE_ID: &str = "eve.control.requests";
pub type ControlResult<T> = Result<T, ControlError>;
pub type ControlFuture<'a, T> = Pin<Box<dyn Future<Output = ControlResult<T>> + Send + 'a>>;
pub type RunFuture<'a> = Pin<Box<dyn Future<Output = RunReport> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationKey {
    pub session: SessionKey,
    pub task_id: String,
    /// 每次控制插件启动重新生成的随机标识，避免重启后旧通道事件撞号。
    pub controller_epoch: [u8; 16],
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlInput {
    pub session: SessionInput,
    pub task_id: String,
}
impl ControlInput {
    pub fn validate(&self) -> ControlResult<()> {
        self.session
            .key
            .validate()
            .map_err(|_| ControlError::InvalidInput)?;
        if self.session.text.trim().is_empty()
            || self.task_id.is_empty()
            || self.task_id.len() > 256
            || self.task_id.trim() != self.task_id
            || self.task_id.chars().any(char::is_control)
        {
            return Err(ControlError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlPhase {
    Starting,
    Generating,
    /// 含等待并发槽位/串行队列；不表示所有工具已经执行。
    Tools,
    Committing,
    Cancelling,
    Finished,
    /// 保存失败或执行器状态未知；禁止在这个控制器中开启替代轮次。
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitState {
    NotStarted,
    Completed,
    Failed,
    Pending,
    Unknown,
}
#[derive(Clone, Debug, PartialEq)]
pub enum RunFailure {
    Execution(LlmError),
    Session(SessionError),
    Commit(SessionError),
    Delivery(LlmError),
    FailureRecord {
        storage: SessionError,
        execution: LlmError,
    },
    RunnerPanicked,
}
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    /// 取消发生在首个事件前时可能未知；代际标识始终完整。
    pub turn_id: Option<u64>,
    pub commit: CommitState,
    pub text: Option<String>,
    /// 完整生成结果；Commit 失败时保留原调用参数和配对回执，便于显式修复提交。
    pub transcript: Option<Vec<ChatMessage>>,
    /// None 表示未知，不能由此宣称没有副作用。
    pub started_tools: Option<u64>,
    pub tool_results: Vec<ToolResult>,
    pub failure: Option<RunFailure>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ControlReport {
    pub key: GenerationKey,
    /// 仅代表用户取消已准入；Completed/Delivery 仍表示回复已经提交。
    pub cancel_requested: bool,
    pub run: RunReport,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ControlSnapshot {
    pub key: GenerationKey,
    /// 当前代真实输入；补充/纠正组合不能让调用方冒充原任务上下文。
    pub input_text: String,
    pub phase: ControlPhase,
    pub cancel_requested: bool,
    /// 展示资格已撤销；完成后取消也撤销排队事件，但不改写执行结果。
    pub events_retired: bool,
    pub turn_id: Option<u64>,
    pub report: Option<ControlReport>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ControlEvent {
    pub key: GenerationKey,
    pub event: TurnEvent,
}

/// 有界通道应实现 closed。排队事件必须保留 key，在实际展示前调用 accepts；
/// 通道需串行处理“检查并展示”和取消/切换，不能在两者之间异步等待。
pub trait ControlEventSink: Send + Sync {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()>;
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
pub struct DiscardControlEvents;
impl ControlEventSink for DiscardControlEvents {
    fn emit(&self, _: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// 组合层装配内置 SessionLlmHost；控制插件不持有模型厂商或 Runtime 私有类型。
/// run 必须观察 sink.closed，并在返回前等待工具析构及最终状态提交。
pub trait ControlRunner: Send + Sync {
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelDisposition {
    Requested,
    AlreadyRequested,
    AlreadyFinished,
}
pub trait ControlService: Send + Sync {
    fn submit(
        &self,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey>;
    /// 目标已收尾后，原子比较完整代际并提交替代请求；不隐式取消。
    /// 比较与提交在同一临界区，避免过时分类覆盖刚完成的新任务。
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey>;
    /// 只发送取消信号，不宣称工具已经停止；wait 才确认收尾。
    fn cancel(&self, key: &GenerationKey) -> ControlResult<CancelDisposition>;
    /// 创建时捕获该代完成通知；之后即使新代启动，这个 Future 仍返回原代报告。
    /// 每个会话只保留最新代；新代启动后再创建旧代 wait 返回 StaleGeneration。
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport>;
    fn snapshot(&self, key: &SessionKey) -> ControlResult<Option<ControlSnapshot>>;
    /// 包括用户、会话、任务、控制器 epoch 和生成代；取消代及被替换代均不接受。
    fn accepts(&self, event: &ControlEvent) -> bool;
}
#[derive(Clone)]
pub struct ControlServiceHandle(pub Arc<dyn ControlService>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlError {
    InvalidInput,
    OwnerMismatch,
    Busy,
    Blocked,
    StaleGeneration,
    Unavailable,
    LimitReached,
}
impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "任务输入或标识无效",
            Self::OwnerMismatch => "任务所属用户不匹配",
            Self::Busy => "会话任务仍在执行或取消收尾",
            Self::Blocked => "会话提交失败或状态未知，不能开启替代轮次",
            Self::StaleGeneration => "任务生成代已失效",
            Self::Unavailable => "控制服务不可用",
            Self::LimitReached => "任务生成计数已达上限",
        })
    }
}
impl std::error::Error for ControlError {}
