//! 后台任务契约：Eve 在对话中把一件需要多步完成的事派发到后台，由执行器在步数预算内独立完成，
//! 结束后私聊通知派发它的用户。
//!
//! 创建、开始、结束与每次通知尝试都先持久化。执行中进程退出时记为“已中断”，不自动重跑；
//! 发送通知时进程退出记为结果未知，不重发。
use serde::{Deserialize, Serialize};
use std::fmt;

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
/// 保存的最终结果与失败原因上限；更长的在保存前截断并注明。
pub const MAX_RESULT_BYTES: usize = 16_384;
pub const MAX_FAILURE_BYTES: usize = 512;
/// 每个任务的步数（工具调用次数）上限的取值范围；默认值由宿主配置。
pub const MIN_STEPS: u32 = 1;
pub const MAX_STEPS: u32 = 500;
pub const DEFAULT_STEPS: u32 = 50;
/// 结束后多久仍未送达就不再通知。
pub const DELIVERY_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// 每条通知至多投递尝试次数（其中至多一次主动私聊）。
pub const MAX_ATTEMPTS: usize = 6;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

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
    /// 交给执行器的完整说明；执行器看不到对话，须写清楚背景与要交付什么。
    pub instructions: String,
    /// 步数预算：至多调用多少次工具。
    pub max_steps: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskStatus {
    Queued,
    Running,
    /// 执行器给出了最终结果。
    Completed,
    /// 执行失败（模型或工具不可用、超时等），保存原因。
    Failed,
    /// 用户取消。
    Cancelled,
    /// 执行中进程退出；不自动重跑。
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

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
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
}
impl Task {
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

#[derive(Clone, Eq, PartialEq)]
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
        self.tasks
            .iter()
            .filter(|task| task.notice_pending())
            .filter(|task| {
                task.notice
                    .as_ref()
                    .is_some_and(|notice| notice.proactive_attempts() == 0)
            })
            .filter(|task| !self.sending(&task.owner.user_id))
            .min_by_key(|task| (task.finished_at_ms, task.id.clone()))
    }
    /// 可以随这位用户的私聊回复附带的通知；这条消息没有附带过。
    pub fn due_passive(&self, user_id: &str, message_id: &str) -> Option<&Task> {
        if self.sending(user_id) {
            return None;
        }
        self.tasks
            .iter()
            .filter(|task| task.owner.user_id == user_id && task.notice_pending())
            .filter(|task| {
                task.notice
                    .as_ref()
                    .is_some_and(|notice| !notice.carried_by(message_id))
            })
            .min_by_key(|task| (task.finished_at_ms, task.id.clone()))
    }
    /// 结束后投递窗口已过仍在等待的通知。
    pub fn overdue(&self, now_ms: u64) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(move |task| {
            task.notice_pending()
                && task
                    .finished_at_ms
                    .is_some_and(|at| now_ms.saturating_sub(at) > DELIVERY_WINDOW_MS)
        })
    }
    fn sending(&self, user_id: &str) -> bool {
        self.tasks.iter().any(|task| {
            task.owner.user_id == user_id
                && task
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.status == NoticeStatus::Delivering)
        })
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait TaskAdmin: Send + Sync {
    fn snapshot(&self) -> TaskResult<TaskSnapshot>;
    /// 保存一条排队任务。同一用户、同一来源只创建一条：重复执行时返回原记录。
    fn create(
        &self,
        owner: &TaskOwner,
        spec: TaskSpec,
        source: &str,
        now_ms: u64,
    ) -> TaskResult<Task>;
    /// 排队 → 执行中；保存后执行器才开始。
    fn start(&self, id: &str, now_ms: u64) -> TaskResult<Task>;
    /// 执行中 → 结束，保存结果与用量；完成与失败生成一条等待投递的通知。
    fn finish(&self, id: &str, end: TaskEnd, usage: TaskUsage, now_ms: u64) -> TaskResult<Task>;
    /// 用户取消自己的任务：排队中的直接取消；执行中的记下取消请求，由执行器停止后结束。
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

/// 标题单行、非空、至多 120 字节；说明非空、至多 8192 字节；步数在 1 至 500 之间。
pub fn validate_spec(spec: &TaskSpec) -> TaskResult<()> {
    validate_text(&spec.title, MAX_TITLE_BYTES)?;
    if spec.title.contains('\n') {
        return Err(TaskError::InvalidInput);
    }
    validate_text(&spec.instructions, MAX_INSTRUCTIONS_BYTES)?;
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
