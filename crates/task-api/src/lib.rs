//! 后台任务契约：Eve 在对话中把一件需要多步完成的事派发到后台，由执行器在步数预算内独立完成，
//! 结束后私聊通知派发它的用户。
//!
//! 创建、开始、结束与每次通知尝试都先持久化。执行中进程退出时在持久化的安全边界接续，未知工具结果不自动重放；
//! 发送通知时进程退出记为结果未知，不重发。
mod evidence;
pub use evidence::{TaskEvidenceKey, TaskEvidenceReader, TaskEvidenceSnapshot};
mod delivery;
pub use delivery::{TaskDeliveryEntry, TaskDeliverySnapshot, validate_notice};
mod profiles;
mod scheduling;
pub use scheduling::{TaskSchedulingEntry, TaskSchedulingSnapshot};
mod strategy;
pub use profiles::*;
use serde::{Deserialize, Serialize};
use std::fmt;
pub use strategy::*;

pub const TASK_PLUGIN_ID: &str = "eve.task";
pub const TASK_STATE_KEY: &str = "tasks.v1";
/// 账本至多保存的任务；满了时删除最早了结（已结束且通知已了结）的一条，没有可删的就不再新增。
pub const MAX_TASKS: usize = 128;
/// 同一用户同时排队或执行中的任务上限。
pub const MAX_ACTIVE_PER_OWNER: usize = 4;
/// 同一用户 24 小时内至多派发的任务数。
pub const MAX_CREATED_PER_OWNER_PER_DAY: usize = 20;
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;
pub const MAX_TITLE_BYTES: usize = 120;
pub const MAX_INSTRUCTIONS_BYTES: usize = 8192;
/// 单次显式委派的完整上下文快照上限；超限拒绝，绝不静默截断。
pub const MAX_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
/// 保存的最终结果与失败原因上限；更长的在保存前截断并注明。
pub const MAX_RESULT_BYTES: usize = 16_384;
pub const MAX_FAILURE_BYTES: usize = 512;
/// 每个任务的步数（工具调用次数）上限的取值范围；默认值由宿主配置。
pub const MIN_STEPS: u32 = 1;
pub const MAX_STEPS: u32 = 5000;
pub const DEFAULT_STEPS: u32 = 100;
/// 宿主自动失败恢复及其经用户批准的接续任务使用此来源前缀。
pub const FAILURE_RECOVERY_SOURCE_PREFIX: &str = "failure:";

/// 到达收尾阶段且未明确完成/阻塞时，恢复任务需要用户追加预算；未知副作用不能走预算批准。
pub fn recovery_needs_step_approval(checkpoint: &eve_llm_api::TurnCheckpoint) -> bool {
    if !checkpoint.closing || !checkpoint.resumable() {
        return false;
    }
    if checkpoint.remaining.is_some() {
        return true;
    }
    let progress = checkpoint
        .messages
        .iter()
        .rev()
        .flat_map(|m| m.tool_calls.iter().rev())
        .find(|call| call.name == "task_progress");
    let concluded = progress.is_some_and(|call| checkpoint.messages.iter().flat_map(|m| &m.tool_results)
        .any(|result| result.call_id == call.id && matches!(&result.output, eve_llm_api::ToolOutput::Success(value)
            if matches!(value.get("status").and_then(serde_json::Value::as_str), Some("completed" | "blocked")))));
    !concluded
}

/// 只信任最后一次已取得成功回执的任务状态；历史问题不覆盖后来的完成状态。
pub fn blocked_progress(messages: &[eve_llm_api::ChatMessage]) -> Option<&str> {
    for call in messages
        .iter()
        .rev()
        .flat_map(|m| m.tool_calls.iter().rev())
    {
        if call.name != "task_progress" {
            continue;
        }
        let result = messages
            .iter()
            .flat_map(|m| &m.tool_results)
            .find(|r| r.call_id == call.id)?;
        if let eve_llm_api::ToolOutput::Success(value) = &result.output {
            return (value.get("status").and_then(serde_json::Value::as_str) == Some("blocked"))
                .then(|| value.get("remaining").and_then(serde_json::Value::as_str))
                .flatten()
                .filter(|s| !s.trim().is_empty());
        }
    }
    None
}
/// 结束后多久仍未送达就不再通知。
pub const DELIVERY_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// 每条通知至多投递尝试次数（其中至多一次主动私聊）。
pub const MAX_ATTEMPTS: usize = 6;
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
/// 内容寻址的不可变任务正文；实现须核对大小/摘要、持久化后确认，不允许覆盖。
/// 冷正文与索引共同备份；缺失或损坏不能伪装成任务不存在。
pub trait TaskArchiveStore: Send + Sync {
    fn get(&self, hash: &str) -> TaskResult<Vec<u8>>;
    fn put(&self, hash: &str, bytes: &[u8]) -> TaskResult<()>;
    /// 只供可信宿主在完整索引提交确认后回收无引用正文；不是用户工具。
    /// 索引和正文必须使用独占的同一存储；替换实现可保留历史并另设磁盘配额。
    fn compact(&self, _retained: &[String]) -> TaskResult<()> {
        Ok(())
    }
}

pub const MAX_AUTO_RECOVERIES: u32 = 3;

/// 新任务默认开启安全接续；旧任务没有检查点，保持旧的中断语义。
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskExecution {
    pub checkpoint: Option<eve_llm_api::TurnCheckpoint>,
    pub recoveries: u32,
    pub resume_at_ms: Option<u64>,
    /// 首次执行固定总期限；重试和重启不能重置。
    pub deadline_at_ms: Option<u64>,
    /// 原自主授权的绝对期限；排队、恢复及重启均不能延长。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_deadline_at_ms: Option<u64>,
}

pub type TaskResult<T> = Result<T, TaskError>;

/// 派发任务的可信会话：取自本轮宿主绑定的范围，不从模型参数读取。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOwner {
    pub session_id: String,
    pub user_id: String,
}

/// 模型派发时给出的任务。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    /// 简短标题，用于列表与通知。
    pub title: String,
    /// 交给执行器的完整说明；无论是否携带上下文，均须写清任务边界与交付物。
    pub instructions: String,
    /// 步数预算：至多调用多少次工具。
    pub max_steps: u32,
    /// 可信宿主捕获的会话快照；模型只能决定是否携带，不能指定正文或身份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<DelegatedContext>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedContext {
    pub owner: TaskOwner,
    /// ChatMessage 数组的 JSON；作为历史数据提供，不能提升为接收者系统指令。
    pub messages_json: String,
}
impl fmt::Debug for DelegatedContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DelegatedContext")
            .field("bytes", &self.messages_json.len())
            .finish_non_exhaustive()
    }
}
pub fn validate_delegated_context(context: &DelegatedContext) -> TaskResult<()> {
    validate_owner(&context.owner)?;
    if context.messages_json.is_empty() || context.messages_json.len() > MAX_CONTEXT_BYTES {
        return Err(TaskError::InvalidInput);
    }
    let messages: Vec<eve_llm_api::ChatMessage> =
        serde_json::from_str(&context.messages_json).map_err(|_| TaskError::InvalidInput)?;
    if messages.is_empty() {
        return Err(TaskError::InvalidInput);
    }
    eve_llm_api::ModelRequest {
        messages,
        tools: vec![],
    }
    .validate()
    .map_err(|_| TaskError::InvalidInput)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskStatus {
    Queued,
    Running,
    /// 执行器给出了最终结果。
    Completed,
    /// 本次执行已停止，等待用户补充信息或解除阻塞；不自动接续。
    Blocked,
    /// 执行失败（模型或工具不可用、超时等），保存原因。
    Failed,
    /// 用户取消。
    Cancelled,
    /// 无法安全接续的执行中任务；保留记录，不自动重放。
    Interrupted,
}
impl TaskStatus {
    pub fn finished(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// 一次执行实际用掉的步数与模型请求数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUsage {
    pub tool_calls: u32,
    pub model_requests: u32,
}

/// 执行器回报的结局。
#[derive(Clone, Eq, PartialEq)]
pub enum TaskEnd {
    Completed {
        result: String,
    },
    Blocked {
        result: String,
    },
    Failed {
        reason: String,
    },
    /// 只有用户已请求取消的执行中任务才能以取消结束。
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryChannel {
    /// 主动私聊。
    Proactive,
    /// 随用户这条私聊消息的被动回复附带。
    Passive { message_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryResult {
    Sent {
        platform_message_id: Option<String>,
    },
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：没有可用的私聊路由、前面的回复片段失败或回复已过期。
    NotSent,
    /// 写出前后进程退出，是否送达不确定；不重发。
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryAttempt {
    pub channel: DeliveryChannel,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub result: Option<DeliveryResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NoticeStatus {
    /// 等待下一个投递时机。
    Pending,
    /// 已保存投递尝试，正在发送。
    Delivering,
    Delivered,
    /// 投递窗口已过或尝试已用尽。
    Expired,
    /// 送达与否不确定；不重发。
    Unknown,
}
impl NoticeStatus {
    pub fn settled(self) -> bool {
        matches!(self, Self::Delivered | Self::Expired | Self::Unknown)
    }
}

/// 结束通知：完成、失败与中断的任务各有一条；取消的任务没有。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub status: NoticeStatus,
    pub attempts: Vec<DeliveryAttempt>,
    pub closed_at_ms: Option<u64>,
}
impl Notice {
    pub fn proactive_attempts(&self) -> usize {
        self.attempts
            .iter()
            .filter(|attempt| attempt.channel == DeliveryChannel::Proactive)
            .count()
    }
    pub fn carried_by(&self, message_id: &str) -> bool {
        self.attempts.iter().any(|attempt| {
            matches!(&attempt.channel, DeliveryChannel::Passive { message_id: id } if id == message_id)
        })
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub owner: TaskOwner,
    pub spec: TaskSpec,
    /// 派发来源（本次工具调用的摘要）；同一来源只创建一条。
    pub source: String,
    pub created_at_ms: u64,
    pub status: TaskStatus,
    /// 执行中收到取消请求；执行器据此停止并以取消结束。
    pub cancel_requested: bool,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub usage: Option<TaskUsage>,
    pub result: Option<String>,
    pub failure: Option<String>,
    pub notice: Option<Notice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<TaskExecution>,
}
impl Task {
    pub fn needs_step_approval(&self) -> bool {
        self.source.starts_with(FAILURE_RECOVERY_SOURCE_PREFIX)
            && self
                .execution
                .as_ref()
                .and_then(|e| e.checkpoint.as_ref())
                .is_some_and(recovery_needs_step_approval)
    }
    /// 已结束且通知（如有）已了结；容量满时可以腾出。
    pub fn settled_at(&self) -> Option<u64> {
        if !self.status.finished() {
            return None;
        }
        match &self.notice {
            None => self.finished_at_ms,
            Some(notice) if notice.status.settled() => notice.closed_at_ms.or(self.finished_at_ms),
            Some(_) => None,
        }
    }
    /// 通知正在等待投递。
    pub fn notice_pending(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|notice| notice.status == NoticeStatus::Pending)
    }
}

/// 只读任务目录；不携带委派上下文、检查点正文、结果或通知回执。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSummary {
    pub id: String,
    pub owner: TaskOwner,
    pub title: String,
    pub instructions: String,
    pub created_at_ms: u64,
    pub status: TaskStatus,
}
impl From<&Task> for TaskSummary {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            owner: task.owner.clone(),
            title: task.spec.title.clone(),
            instructions: task.spec.instructions.clone(),
            created_at_ms: task.created_at_ms,
            status: task.status,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct TaskSnapshot {
    pub tasks: Vec<Task>,
}
impl TaskSnapshot {
    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }
    pub fn for_owner<'a>(&'a self, user_id: &'a str) -> impl Iterator<Item = &'a Task> {
        self.tasks
            .iter()
            .filter(move |task| task.owner.user_id == user_id)
    }
    /// 排队中的任务，最早派发的在前。
    pub fn queued(&self) -> Vec<&Task> {
        let mut queued: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Queued)
            .collect();
        queued.sort_by_key(|task| (task.created_at_ms, task.id.clone()));
        queued
    }
    pub fn running(&self) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
    }
    /// 可以主动私聊通知的任务：通知等待中、还没有主动私聊过、这位用户没有正在发送的通知；取最早结束的一条。
    pub fn due_proactive(&self) -> Option<&Task> {
        delivery::due_proactive(&self.tasks)
    }
    /// 可以随这位用户的私聊回复附带的通知；这条消息没有附带过。
    pub fn due_passive(&self, user_id: &str, message_id: &str) -> Option<&Task> {
        delivery::due_passive(&self.tasks, user_id, message_id)
    }
    /// 结束后投递窗口已过仍在等待的通知。
    pub fn overdue(&self, now_ms: u64) -> impl Iterator<Item = &Task> {
        delivery::overdue(&self.tasks, now_ms)
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait TaskAdmin: Send + Sync {
    fn snapshot(&self) -> TaskResult<TaskSnapshot>;
    /// 同一时点、显式指定完整身份的任务证据视图；不将局部数据冒充完整快照。
    /// 原生实现只复制匹配的任务，旧实现保留完整快照兼容路径。
    fn evidence_snapshot(&self, keys: &[TaskEvidenceKey]) -> TaskResult<TaskEvidenceSnapshot> {
        TaskEvidenceSnapshot::select(&self.snapshot()?.tasks, keys)
    }

    /// 可信宿主调度所需的完整轻量目录；不携带任务说明、上下文、检查点、结果或通知正文。
    /// 旧实现默认从完整快照转换；原生实现直接借用账本元数据。
    fn scheduling_snapshot(&self) -> TaskResult<TaskSchedulingSnapshot> {
        let directory = TaskSchedulingSnapshot {
            tasks: self
                .snapshot()?
                .tasks
                .iter()
                .map(TaskSchedulingEntry::from)
                .collect(),
        };
        directory.validate()?;
        Ok(directory)
    }
    /// 可信宿主的通知目录；不含任务说明、委派上下文、检查点和结果正文。
    /// 旧实现保留完整快照转换，原生实现直接借用账本元数据。
    fn delivery_snapshot(&self) -> TaskResult<TaskDeliverySnapshot> {
        let directory = TaskDeliverySnapshot {
            tasks: self
                .snapshot()?
                .tasks
                .iter()
                .map(TaskDeliveryEntry::from)
                .collect(),
        };
        directory.validate()?;
        Ok(directory)
    }
    /// owner 必须来自可信宿主；返回同一完整身份的轻量目录，不创建任务或重领预算。
    fn summaries_for(&self, owner: &TaskOwner) -> TaskResult<Vec<TaskSummary>> {
        validate_owner(owner)?;
        Ok(self
            .snapshot()?
            .tasks
            .iter()
            .filter(|task| task.owner == *owner)
            .map(TaskSummary::from)
            .collect())
    }
    /// 按可信身份读取一个完整任务；未知 ID 与其他身份的任务均为 None。
    fn get_owned(&self, owner: &TaskOwner, id: &str) -> TaskResult<Option<Task>> {
        validate_owner(owner)?;
        validate_id(id)?;
        Ok(self
            .snapshot()?
            .tasks
            .into_iter()
            .find(|task| task.id == id && task.owner == *owner))
    }

    /// 可选的宿主策略装配：在首次执行前固定选择；普通任务默认不使用策略。
    fn strategy_guidance(&self, _task: &Task) -> TaskResult<Option<TaskStrategyGuidance>> {
        Ok(None)
    }
    /// 只结算已保存的任务结果；不执行任务、不重放工具。
    fn reconcile_strategies(&self) -> TaskResult<()> {
        Ok(())
    }
    /// 保存一条排队任务。同一用户、同一来源只创建一条：重复执行时返回原记录。
    fn create(
        &self,
        owner: &TaskOwner,
        spec: TaskSpec,
        source: &str,
        now_ms: u64,
    ) -> TaskResult<Task>;
    /// 在同一个持久提交中创建有授权绝对期限的任务。自定义实现默认拒绝，不回退为无期限创建。
    fn create_bounded(
        &self,
        _owner: &TaskOwner,
        _spec: TaskSpec,
        _source: &str,
        _deadline_at_ms: u64,
        _now_ms: u64,
    ) -> TaskResult<Task> {
        Err(TaskError::InvalidTransition)
    }
    /// 排队 → 执行中；保存后执行器才开始。
    fn start(&self, id: &str, now_ms: u64) -> TaskResult<Task>;
    /// 工具前/后及模型请求前的持久检查点；保存失败不得继续执行。
    fn checkpoint(
        &self,
        id: &str,
        checkpoint: eve_llm_api::TurnCheckpoint,
        deadline_at_ms: u64,
        now_ms: u64,
    ) -> TaskResult<Task>;
    /// 支持按步骤运行的检查点；None 表示无任务总时长截止，授权期限仍必须保留。
    /// 未实现此能力的替换后端失败关闭，不能假报已经保存。
    fn checkpoint_with_deadline(
        &self,
        id: &str,
        checkpoint: eve_llm_api::TurnCheckpoint,
        deadline_at_ms: Option<u64>,
        now_ms: u64,
    ) -> TaskResult<Task> {
        match deadline_at_ms {
            Some(deadline) => self.checkpoint(id, checkpoint, deadline, now_ms),
            None => Err(TaskError::InvalidTransition),
        }
    }
    /// 安全边界上安排有限次自动恢复；不清空回执、不补充步数、不新建任务。
    fn retry(&self, id: &str, now_ms: u64) -> TaskResult<Task>;
    /// 可信条件已满足后接续同一阻塞任务；保留原预算、期限、回执和任务 ID。
    fn resume_blocked(
        &self,
        _owner: &TaskOwner,
        _id: &str,
        _evidence: &str,
        _now_ms: u64,
    ) -> TaskResult<Task> {
        Err(TaskError::InvalidTransition)
    }
    /// 执行中 → 结束，保存结果与用量；完成与失败生成一条等待投递的通知。
    fn finish(&self, id: &str, end: TaskEnd, usage: TaskUsage, now_ms: u64) -> TaskResult<Task>;
    /// 用户取消自己的任务：排队或阻塞中的直接取消；执行中的记下取消请求，由执行器停止后结束。
    fn cancel(&self, user_id: &str, id: &str, now_ms: u64) -> TaskResult<Task>;
    /// 保存一次通知投递尝试并进入发送中；返回后才可发送。主动私聊每条至多一次。
    fn claim(&self, id: &str, channel: DeliveryChannel, now_ms: u64) -> TaskResult<Task>;
    /// 保存投递结果：送达即了结；没有写出或平台拒绝时回到等待，尝试用尽记为过期。
    fn record_delivery(&self, id: &str, result: DeliveryResult, now_ms: u64) -> TaskResult<Task>;
    /// 投递窗口已过仍未送达：通知记为过期。
    fn expire(&self, id: &str, now_ms: u64) -> TaskResult<Task>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskError {
    InvalidInput,
    /// 这位用户同时进行的任务、24 小时内的派发数或账本已达上限。
    LimitReached,
    NotFound,
    InvalidTransition,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    /// 提交无法确认；实例已关闭，须重新打开核对。
    Storage,
}
impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "后台任务输入无效",
            Self::LimitReached => "后台任务已达上限",
            Self::NotFound => "没有这个后台任务",
            Self::InvalidTransition => "后台任务状态不允许此操作",
            Self::Unavailable => "后台任务不可用",
            Self::CorruptState => "后台任务记录损坏；未清空",
            Self::UnsupportedVersion => "不支持该后台任务记录版本",
            Self::Storage => "后台任务提交无法确认；须重新打开核对",
        })
    }
}
impl std::error::Error for TaskError {}

pub fn validate_id(value: &str) -> TaskResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(TaskError::InvalidInput);
    }
    Ok(())
}

fn validate_text(value: &str, limit: usize) -> TaskResult<()> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > limit
        || value.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(TaskError::InvalidInput);
    }
    Ok(())
}

pub fn validate_owner(owner: &TaskOwner) -> TaskResult<()> {
    validate_id(&owner.session_id)?;
    validate_id(&owner.user_id)
}

/// 标题单行、非空、至多 120 字节；说明非空、至多 8192 字节；步数在 1 至 5000 之间。
pub fn validate_spec(spec: &TaskSpec) -> TaskResult<()> {
    validate_text(&spec.title, MAX_TITLE_BYTES)?;
    if spec.title.contains('\n') {
        return Err(TaskError::InvalidInput);
    }
    validate_text(&spec.instructions, MAX_INSTRUCTIONS_BYTES)?;
    if let Some(context) = &spec.context {
        validate_delegated_context(context)?;
    }
    if !(MIN_STEPS..=MAX_STEPS).contains(&spec.max_steps) {
        return Err(TaskError::InvalidInput);
    }
    Ok(())
}

/// 截断到不超过 `limit` 字节的字符边界；截断时注明。
pub fn bounded(text: &str, limit: usize) -> String {
    const MARK: &str = "…（已截断）";
    let text = text.trim();
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit.saturating_sub(MARK.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", text[..end].trim_end())
}

fn digest(parts: &[&str]) -> String {
    use ring::digest::{Context, SHA256};
    let mut context = Context::new(&SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let digest: String = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    digest[..32].to_string()
}

/// 同一用户的同一来源只创建一个任务；重复派发得到同一 ID。
pub fn task_id(user_id: &str, source: &str) -> String {
    format!("task-{}", digest(&["eve.task:v1", user_id, source]))
}

impl fmt::Debug for TaskSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskSpec")
            .field("max_steps", &self.max_steps)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("id", &self.id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for TaskSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskSnapshot")
            .field("tasks", &self.tasks.len())
            .finish()
    }
}
impl fmt::Debug for TaskEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Completed { .. } => "Completed",
            Self::Failed { .. } => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Blocked { .. } => "Blocked",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(steps: u32) -> TaskSpec {
        TaskSpec {
            title: "整理资料".into(),
            instructions: "读取文件并总结".into(),
            max_steps: steps,
            context: None,
        }
    }

    #[test]
    fn specs_ids_and_bounds() {
        assert!(validate_spec(&spec(DEFAULT_STEPS)).is_ok());
        assert!(validate_spec(&spec(MAX_STEPS)).is_ok());
        for bad in [0, MAX_STEPS + 1] {
            assert_eq!(validate_spec(&spec(bad)), Err(TaskError::InvalidInput));
        }
        let mut two_lines = spec(1);
        two_lines.title = "一\n二".into();
        assert!(validate_spec(&two_lines).is_err());
        assert_eq!(task_id("u", "s"), task_id("u", "s"));
        assert_ne!(task_id("u", "s"), task_id("v", "s"));
        assert!(task_id("u", "s").starts_with("task-"));
        assert_eq!(bounded("短", 10), "短");
        let long = bounded(&"长".repeat(100), 40);
        assert!(long.len() <= 40 && long.ends_with("（已截断）"));
    }
}
