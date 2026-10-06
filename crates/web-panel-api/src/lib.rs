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

/// 本进程最近若干次实际消息判断的只读诊断；只在内存中，宿主重启后清空。
/// 不含会话、用户、任务、消息 ID、正文、模型名、端点或凭据。
#[derive(Clone, Debug, Serialize)]
pub struct JudgmentLog {
    /// 宿主启动时选择的判断方式：`off` 只有明确命令规则，`primary`/`jev` 另有自然判断。
    pub mode: &'static str,
    pub capacity: usize,
    /// 本次启动以来记录的判断总数；超出容量的最早记录被移出并计入 `evicted`。
    pub recorded_total: u64,
    pub evicted: u64,
    pub items: Vec<JudgmentView>,
    pub next_before: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct JudgmentView {
    pub sequence: u64,
    pub finished_at_unix_ms: u64,
    pub elapsed_micros: u64,
    /// `decided`、`failed` 或 `dropped`（调用方在结束前丢弃，不代表远端已停止）。
    pub result: &'static str,
    pub failure: Option<&'static str>,
    pub intents: Vec<&'static str>,
    /// `complete`、`unsupported`、`overflow`、`invalid` 或 `unreported`。
    pub coverage: &'static str,
    /// 覆盖不完整时为空，不填伪造的零。
    pub counts: Option<JudgmentCounts>,
    pub steps: Vec<JudgmentStep>,
    pub fallbacks: Vec<&'static str>,
}
/// 本地适配器的调用尝试，不是网络请求、远端收到的请求、token 或计费次数。
#[derive(Clone, Debug, Serialize)]
pub struct JudgmentCounts {
    pub rules: u64,
    pub auxiliary: u64,
    pub primary: u64,
    pub classifier_calls: u64,
    pub model_provider_calls: u64,
    pub fallbacks: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct JudgmentStep {
    /// `stage` 或 `attempt`。
    pub kind: &'static str,
    pub name: &'static str,
    /// 没有终态事件时为空，表示不可观测，不能当作成功。
    pub outcome: Option<&'static str>,
    pub elapsed_micros: Option<u64>,
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
    /// 序号小于 `before` 的最近判断，新的在前，每页 1..=50 项。未接入诊断的实现返回 Unavailable。
    fn judgments(&self, _before: Option<u64>, _limit: usize) -> PanelResult<JudgmentLog> {
        Err(PanelError::Unavailable)
    }
}
