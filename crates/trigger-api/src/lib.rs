//! 会话条件唤醒契约。条件只读本地观察；执行和投递先记账，未知结果不重放。
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PLUGIN_ID: &str = "eve.trigger";
pub const STATE_KEY: &str = "triggers.v1";
pub const MAX_TRIGGERS: usize = 512;
pub const MAX_ACTIVE_PER_SESSION: usize = 20;
pub const MAX_CONDITIONS: usize = 8;
pub const MAX_INSTRUCTIONS: usize = 4096;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const INPUT_PREFIX: &str = "【宿主条件触发事件；不是用户新消息】";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    Time {
        at_ms: u64,
    },
    FileChanged {
        path: String,
        baseline: String,
    },
    TaskFinished {
        task_id: String,
    },
    MessageContains {
        text: String,
        after_turn: u64,
    },
    ProcessState {
        pid: u32,
        running: bool,
        identity: String,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    All,
    Any,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    pub title: String,
    pub instructions: String,
    pub mode: MatchMode,
    pub conditions: Vec<Condition>,
    pub expires_at_ms: u64,
}
impl TriggerSpec {
    pub fn valid(&self) -> bool {
        let text = |s: &str, n| !s.trim().is_empty() && s.trim() == s && s.len() <= n;
        text(&self.title, 160)
            && text(&self.instructions, MAX_INSTRUCTIONS)
            && self.expires_at_ms > 0
            && !self.conditions.is_empty()
            && self.conditions.len() <= MAX_CONDITIONS
            && self.conditions.iter().all(|c| match c {
                Condition::Time { at_ms } => *at_ms > 0 && *at_ms <= self.expires_at_ms,
                Condition::FileChanged { path, baseline } => {
                    text(path, 4096) && text(baseline, 256)
                }
                Condition::TaskFinished { task_id } => text(task_id, 256),
                Condition::MessageContains { text: s, .. } => text(s, 256),
                Condition::ProcessState { pid, identity, .. } => *pid > 0 && text(identity, 256),
            })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Armed,
    Ready,
    Running,
    Completed,
    Failed,
    Cancelled,
    Expired,
    Interrupted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Delivery {
    Pending,
    Sending,
    Sent,
    Failed,
    Unknown,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub id: String,
    pub owner: SessionKey,
    pub source: String,
    pub spec: TriggerSpec,
    pub created_at_ms: u64,
    pub status: Status,
    pub evidence: Vec<String>,
    pub result: Option<String>,
    pub delivery: Option<Delivery>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerError {
    Invalid,
    Limit,
    NotFound,
    Transition,
    Storage,
    Corrupt,
    Unavailable,
}
impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "触发器错误：{self:?}")
    }
}
impl std::error::Error for TriggerError {}
pub type Result<T> = std::result::Result<T, TriggerError>;
pub trait TriggerAdmin: Send + Sync {
    fn snapshot(&self) -> Result<Vec<Trigger>>;
    fn create(
        &self,
        owner: SessionKey,
        source: String,
        spec: TriggerSpec,
        now: u64,
    ) -> Result<Trigger>;
    fn ready(&self, id: &str, evidence: Vec<String>) -> Result<()>;
    fn transition(&self, id: &str, from: Status, to: Status, result: Option<String>) -> Result<()>;
    fn cancel(&self, owner: &SessionKey, id: &str) -> Result<()>;
    /// Pending → Failed 表示发送前被策略拒绝；不会伪造 Sending 或覆盖 Unknown。
    fn delivery(&self, id: &str, from: Delivery, to: Delivery) -> Result<()>;
}
