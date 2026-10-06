//! 本机管理面板的公开契约；不包含 HTTP、模型调用、磁盘实现或 Kernel。
use eve_control_api::GenerationKey;
use eve_session_api::SessionKey;
use serde::Serialize;

pub type PanelResult<T> = Result<T, PanelError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelError {
    InvalidInput,
    NotFound,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug, Serialize)]
pub struct PanelStatus {
    pub started_at_unix_ms: u64,
    pub qq: ChannelStatus,
}
#[derive(Clone, Debug, Serialize)]
pub struct ChannelStatus {
    pub ready: bool,
    pub closed: bool,
    pub terminal_error: bool,
    pub received: u64,
    pub completed: u64,
    pub sent: u64,
    pub failed: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// 只用于继续本次分页；各页不是同一事务快照。
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    pub key: SessionKey,
    pub revision: u64,
    pub turn_count: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct TaskSummary {
    pub key: GenerationKey,
    pub phase: eve_control_api::ControlPhase,
    pub cancel_requested: bool,
    pub events_retired: bool,
    pub turn_id: Option<u64>,
    /// 固定状态名，不包含底层错误、工具参数、原始结果或诊断。
    pub commit: Option<&'static str>,
    pub started_tools: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SessionDetail {
    pub session: SessionHistory,
    pub control: Option<TaskSummary>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SessionHistory {
    pub key: SessionKey,
    pub revision: u64,
    pub turns: Vec<PanelTurn>,
    pub next_before: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PanelTurn {
    pub id: u64,
    pub input: String,
    pub input_truncated: bool,
    pub status: PanelTurnStatus,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state")]
pub enum PanelTurnStatus {
    Pending,
    Interrupted,
    Failed {
        code: &'static str,
        started_tools: Option<u64>,
    },
    Completed {
        messages: Vec<PanelMessage>,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct PanelMessage {
    pub role: &'static str,
    pub text: String,
    pub truncated: bool,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelStatus {
    Requested,
    AlreadyRequested,
    AlreadyFinished,
}

/// 受信宿主提供服务，HTTP 实现仅消费此契约。cancel 只准入取消，不等待或声明提交完成。
/// 每页 1..=100 项；历史最多 50 轮，每段正文最多 8192 字节并明确标注截断。
/// 暴露给本机操作者；不存在通过 HTTP 指定其他 Provider、执行工具或新任务的入口。
pub trait PanelService: Send + Sync {
    fn status(&self) -> PanelResult<PanelStatus>;
    fn sessions(&self, after: Option<&str>, limit: usize) -> PanelResult<Page<SessionSummary>>;
    fn tasks(&self, after: Option<&str>, limit: usize) -> PanelResult<Page<TaskSummary>>;
    fn session(
        &self,
        key: &SessionKey,
        before: Option<u64>,
        limit: usize,
    ) -> PanelResult<SessionDetail>;
    fn cancel(&self, target: &GenerationKey) -> PanelResult<CancelStatus>;
}
