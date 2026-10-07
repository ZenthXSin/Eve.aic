//! 本机管理面板的公开契约；不包含 HTTP、模型调用、磁盘实现或 Kernel。
use eve_control_api::GenerationKey;
use eve_memory_api::MemoryScope;
use eve_session_api::SessionKey;
use serde::Serialize;
mod extensions;
pub use extensions::*;

pub type PanelResult<T> = Result<T, PanelError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelError {
    InvalidInput,
    NotFound,
    Stale,
    Unavailable,
    Forbidden,
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

/// 认知目标的只读投影。宿主只绑定读取句柄，HTTP 无法写入目标或触发规划。
/// 状态、来源与可见范围是固定名称；正文按字节上限截断并明确标注。
#[derive(Clone, Debug, Serialize)]
pub struct GoalSummary {
    pub id: String,
    pub revision: u64,
    /// `ready`、`waiting`、`executing`、`completed`、`cancelled` 或 `blocked`。
    /// 反思子目标 completed 只说明草稿已保存并通过结构校验，不代表父目标或现实目标完成。
    pub status: &'static str,
    pub priority: u8,
    /// `user`、`environment`、`tool`、`inference` 或 `internal`。
    pub source_kind: &'static str,
    pub source_channel: String,
    /// `public`、`user` 或 `internal`；`owner` 只在 user 时给出该用户 ID。
    pub visibility: &'static str,
    pub owner: Option<String>,
    /// 内生反思子目标所属的父目标 ID；其他目标为空。
    pub reflection_of: Option<String>,
    /// 以本目标为父目标的反思子目标数量，包含历史修订。
    pub reflections: usize,
    pub description: String,
    pub description_truncated: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct GoalPage {
    pub subject_id: String,
    /// 本页读取时的认知状态全局修订；各页不是同一事务快照。
    pub revision: u64,
    pub items: Vec<GoalSummary>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct GoalBudget {
    pub max_model_requests: u16,
    pub max_tool_calls: u16,
    pub max_attempts: u16,
    pub timeout_ms: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct GoalExecution {
    pub session_id: String,
    pub task_id: String,
    pub turn_id: Option<u64>,
    pub started_at_ms: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct GoalFeedback {
    /// `not_started`、`completed`、`failed`、`pending` 或 `unknown`。
    pub commit: &'static str,
    pub verification_met: bool,
    pub started_tools: Option<u64>,
    pub summary: String,
    pub summary_truncated: bool,
    pub at_ms: u64,
}
/// 认知状态中的来源记录；正文是保存时的摘要，用户反馈和文件片段仍属未验证的外部数据。
#[derive(Clone, Debug, Serialize)]
pub struct GoalEvent {
    pub id: String,
    /// `external_input`、`state_changed`、`drive_evaluated`、`agenda_selected` 或 `feedback`。
    pub kind: &'static str,
    pub source_kind: &'static str,
    pub source_channel: String,
    pub at_ms: u64,
    pub summary: String,
    pub summary_truncated: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReflectionDraft {
    pub summary: String,
    pub next_step: String,
    pub needs_user_input: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReflectionView {
    pub goal_id: String,
    /// 由派生输入记录读出的父目标修订；记录不可用时为空。
    pub parent_revision: Option<u64>,
    pub status: &'static str,
    /// 只有精确绑定父目标当前修订的草稿为 true，其余都是历史草稿。
    pub current: bool,
    /// `saved`：已保存并通过结构校验，建议本身仍未验证；`not_saved`：没有通过校验的草稿，
    /// 原因见 status；`unavailable`：标记已保存但会话结果缺失、格式不一致或无法读取，不显示正文。
    pub draft_state: &'static str,
    pub draft: Option<ReflectionDraft>,
}
#[derive(Clone, Debug, Serialize)]
pub struct GoalDetail {
    pub subject_id: String,
    pub revision: u64,
    pub goal: GoalSummary,
    pub verification: String,
    pub stop_condition: String,
    pub wait_reason: Option<String>,
    /// `interrupted`、`unknown_commit`、`feedback_save_failed` 或 `invalidated`。
    pub block_reason: Option<&'static str>,
    pub expires_at_ms: Option<u64>,
    pub budget: GoalBudget,
    pub execution: Option<GoalExecution>,
    pub feedback: Option<GoalFeedback>,
    /// 关联本目标的最近事件，新的在前；`events_omitted` 为更早未列出的数量。
    pub events: Vec<GoalEvent>,
    pub events_omitted: usize,
    /// 以本目标为父目标的反思子目标：当前草稿在前，其余按父目标修订从新到旧。
    pub reflections: Vec<ReflectionView>,
    /// `ok`；或 `inconsistent`：当前修订的派生记录残缺或矛盾，此时不把任何草稿标为当前。
    pub reflection_check: &'static str,
}

/// 记忆作用域摘要。作用域标识与会话页相同，属于本机操作者可见信息；不含正文。
#[derive(Clone, Debug, Serialize)]
pub struct MemoryScopeSummary {
    pub scope: MemoryScope,
    /// 只属于该作用域的修订。
    pub revision: u64,
    pub evidence: usize,
    pub confirmed: usize,
    pub revoked: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct MemoryScopePage {
    pub items: Vec<MemoryScopeSummary>,
    /// 继续分页时作为 `after` 传回；各页不是同一事务快照。
    pub next_after: Option<MemoryScope>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PreferenceVersionView {
    pub revision: u64,
    /// `confirmed` 或 `revoked`。
    pub status: &'static str,
    pub at_ms: u64,
    /// 是否为该偏好的最新版本；只有最新且确认的版本才进入对话上下文。
    pub current: bool,
    pub text: String,
    pub text_truncated: bool,
    pub evidence_id: String,
    /// `user_statement`、`completed_interaction`；引用的证据不在快照中时为 `missing`。
    pub evidence_kind: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct PreferenceView {
    pub id: String,
    pub status: &'static str,
    pub revision: u64,
    /// 当前确认、会进入对话上下文；撤销后为 false，历史与来源仍保留。
    pub effective: bool,
    pub text: String,
    pub text_truncated: bool,
    /// 历史版本，新的在前。
    pub history: Vec<PreferenceVersionView>,
}
#[derive(Clone, Debug, Serialize)]
pub struct MemoryDetail {
    pub scope: MemoryScope,
    pub revision: u64,
    pub evidence: usize,
    /// 按偏好 ID 升序；数量与历史总数受记忆存储上限约束。
    pub preferences: Vec<PreferenceView>,
}
#[derive(Clone, Debug, Serialize)]
pub struct EvidenceReference {
    pub preference_id: String,
    pub revision: u64,
    pub current: bool,
    pub effective: bool,
}
/// 单条来源正文；须由操作者显式打开。消息 ID 不返回。
#[derive(Clone, Debug, Serialize)]
pub struct MemoryEvidenceDetail {
    pub scope: MemoryScope,
    pub id: String,
    pub revision: u64,
    pub at_ms: u64,
    /// `user_statement` 或 `completed_interaction`。
    pub kind: &'static str,
    /// 用户明确声明，或已完成交互中的用户输入；有界预览并标注截断。
    pub user_text: String,
    pub user_text_truncated: bool,
    /// 仅已完成交互：当时的助手回复，是历史内容，不是已核实事实。
    pub assistant_text: Option<String>,
    pub assistant_text_truncated: bool,
    pub turn_id: Option<u64>,
    /// 引用这条证据的偏好版本，按偏好 ID 与修订排序，最多列出有限条。
    pub references: Vec<EvidenceReference>,
    pub references_total: usize,
}

/// 一次偏好提炼批次；不含输入正文。
#[derive(Clone, Debug, Serialize)]
pub struct LearningJobView {
    pub batch_id: String,
    /// `running`、`completed`、`failed` 或 `interrupted`。
    pub status: &'static str,
    /// failed 时为 `provider`、`invalid_output`、`timeout` 或 `cancelled`。
    pub failure: Option<&'static str>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub evidence: usize,
    pub candidates: usize,
}
/// 学习决策是提交前的意图，不证明偏好已写入记忆。
#[derive(Clone, Debug, Serialize)]
pub struct LearningDecisionView {
    pub sequence: u64,
    pub at_ms: u64,
    /// `confirm`、`update`、`defer` 或 `reject`。
    pub action: &'static str,
    /// update 时的目标偏好与当时的目标修订。
    pub update_preference: Option<String>,
    pub update_revision: Option<u64>,
    /// `eligible`、`evidence_threshold`、`expired`、`policy_denied`、`duplicate`、
    /// `revoked_conflict`、`manual_conflict`、`ambiguous_conflict`、`stale_evidence`、
    /// `explicit_revision_update` 或 `already_linked`。
    pub reason: &'static str,
    pub policy_version: String,
    pub memory_revision: u64,
}
/// 按记忆真实历史核对出的关联偏好。
#[derive(Clone, Debug, Serialize)]
pub struct LinkedPreference {
    pub preference_id: String,
    pub revision: u64,
    pub status: &'static str,
    pub effective: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct LearningCandidateView {
    pub id: String,
    pub batch_id: String,
    /// 模型提炼的候选正文，受学习契约上限约束；不是用户已确认的偏好。
    pub text: String,
    /// 模型自评 0..=100，不是经过校准的概率。
    pub confidence: u8,
    pub evidence_ids: Vec<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    /// 尚未保存且已过首次确认期限。
    pub expired: bool,
    /// 与 QQ `/memory-decision` 相同的核对结果；决策记录本身不算保存。
    pub saved: Option<LinkedPreference>,
    /// 该候选的决策，新的在前，最多列出有限条。
    pub decisions: Vec<LearningDecisionView>,
    pub decisions_total: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct LearningView {
    pub scope: MemoryScope,
    /// 宿主以 `--self-learning` 启动时为 true：按证据策略自动确认；否则候选需用户明确确认。
    pub autonomous: bool,
    /// 新的在前。
    pub jobs: Vec<LearningJobView>,
    /// 新的在前。
    pub candidates: Vec<LearningCandidateView>,
    pub decisions_total: usize,
}

/// 受信宿主提供服务，HTTP 实现仅消费此契约。cancel 只准入取消，不等待或声明提交完成。
/// 每页 1..=100 项；历史最多 50 轮，每段正文最多 8192 字节并明确标注截断。
/// 暴露给本机操作者；不存在通过 HTTP 指定其他 Provider、执行工具或新任务的入口。
pub trait PanelService: Send + Sync {
    fn plugins(&self) -> PanelResult<PluginList> {
        Err(PanelError::Unavailable)
    }
    fn plugin_action(&self, _request: PluginAction) -> PanelFuture<'_, OperationReceipt> {
        Box::pin(async { Err(PanelError::Unavailable) })
    }
    fn plugin_operations(&self) -> PanelResult<Vec<PluginOperation>> {
        Err(PanelError::Unavailable)
    }
    fn acknowledge_plugin_operation(&self, _id: u64) -> PanelFuture<'_, bool> {
        Box::pin(async { Err(PanelError::Unavailable) })
    }
    fn plugin_pages(&self) -> PanelResult<Vec<PluginPageLink>> {
        Err(PanelError::Unavailable)
    }
    fn plugin_page(&self, _plugin: &str, _page: &str) -> PanelResult<PluginPage> {
        Err(PanelError::Unavailable)
    }
    fn save_plugin_page(&self, _request: PageSaveRequest) -> PanelResult<PageSaved> {
        Err(PanelError::Unavailable)
    }
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
    /// ID 大于 `after` 的顶层目标，按 ID 升序，每页 1..=100 项。内生反思子目标在父目标详情中列出；
    /// 父目标缺失的反思子目标仍列在这里，不被隐藏。未启用认知的实现返回 Unavailable。
    fn goals(&self, _after: Option<&str>, _limit: usize) -> PanelResult<GoalPage> {
        Err(PanelError::Unavailable)
    }
    /// 单个目标及其反思草稿和最近来源记录；不存在时返回 NotFound。
    fn goal(&self, _id: &str) -> PanelResult<GoalDetail> {
        Err(PanelError::Unavailable)
    }
    /// 已持久保存的记忆作用域，按 (channel, session_id, user_id) 升序，每页 1..=100 项。
    /// 未开启记忆的实现返回 Unavailable；读取不创建作用域。
    fn memory_scopes(
        &self,
        _after: Option<&MemoryScope>,
        _limit: usize,
    ) -> PanelResult<MemoryScopePage> {
        Err(PanelError::Unavailable)
    }
    /// 已存在作用域的偏好与版本历史；不存在的作用域返回 NotFound，不当作空记忆。
    fn memory(&self, _scope: &MemoryScope) -> PanelResult<MemoryDetail> {
        Err(PanelError::Unavailable)
    }
    /// 已存在作用域的偏好提炼批次、候选与学习决策；未开启偏好提炼的实现返回 Unavailable。
    fn memory_learning(&self, _scope: &MemoryScope) -> PanelResult<LearningView> {
        Err(PanelError::Unavailable)
    }
    /// 同一作用域内的一条来源证据；不存在时返回 NotFound。
    fn memory_evidence(
        &self,
        _scope: &MemoryScope,
        _id: &str,
    ) -> PanelResult<MemoryEvidenceDetail> {
        Err(PanelError::Unavailable)
    }
}
