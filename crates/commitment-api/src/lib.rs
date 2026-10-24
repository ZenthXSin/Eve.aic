//! Eve 自己答应别人的事：答应时记下，做完或决定不做时了结。
//!
//! 约定属于答应时的那个对话参与者（`owner`），由宿主按可信身份决定在哪些会话里可见与可了结；
//! 账本不判断身份，也不从对话文字推断谁是谁。
use serde::{Deserialize, Serialize};
use std::fmt;

pub const COMMITMENT_PLUGIN_ID: &str = "eve.commitment";
pub const COMMITMENT_STATE_KEY: &str = "commitments.v1";
/// 账本至多条数；满了先淘汰最早了结的记录，全是未完成时拒绝新约定。
pub const MAX_COMMITMENTS: usize = 1024;
/// 每个参与者至多未完成的约定。
pub const MAX_OPEN_PER_OWNER: usize = 20;
pub const MAX_TEXT_BYTES: usize = 300;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

pub type CommitmentResult<T> = Result<T, CommitmentError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitmentStatus {
    Open,
    Done,
    Dropped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commitment {
    pub id: String,
    pub owner: String,
    pub text: String,
    /// 记下这条约定的来源（例如工具调用的摘要）；同一来源只记一条。
    pub source: String,
    pub status: CommitmentStatus,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<u64>,
    /// 了结时的简短说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 只交给可信宿主。读取与了结都由宿主给出允许的参与者集合。
pub trait CommitmentLedger: Send + Sync {
    /// 记下一条约定；同一参与者同一来源重复调用返回原记录。
    fn record(
        &self,
        owner: &str,
        text: &str,
        source: &str,
        now_ms: u64,
    ) -> CommitmentResult<Commitment>;
    /// 了结一条未完成的约定；约定的参与者必须在 `owners` 中。
    fn settle(
        &self,
        owners: &[String],
        id: &str,
        status: CommitmentStatus,
        note: Option<&str>,
        now_ms: u64,
    ) -> CommitmentResult<Commitment>;
    /// `owners` 的全部约定，按记下时间从新到旧。
    fn list(&self, owners: &[String]) -> CommitmentResult<Vec<Commitment>>;
    /// 账本里出现过的全部参与者，供宿主判断哪些与当前发言者是同一个人。
    fn owners(&self) -> CommitmentResult<Vec<String>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitmentError {
    InvalidInput,
    NotFound,
    /// 已经了结，不能再改。
    AlreadySettled,
    /// 此人未完成的约定已满，或账本里全是未完成的约定。
    LimitReached,
    Storage,
    CorruptState,
    UnsupportedVersion,
    Unavailable,
}
impl fmt::Display for CommitmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "约定内容无效",
            Self::NotFound => "没有这条约定",
            Self::AlreadySettled => "这条约定已经了结",
            Self::LimitReached => "约定已满",
            Self::Storage => "约定账本写入失败；账本已关闭，重启后以磁盘为准",
            Self::CorruptState => "约定账本已损坏；保留原字节",
            Self::UnsupportedVersion => "约定账本格式版本不受支持；保留原字节",
            Self::Unavailable => "约定账本不可用",
        })
    }
}
impl std::error::Error for CommitmentError {}

pub fn validate_text(value: &str) -> CommitmentResult<()> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(CommitmentError::InvalidInput);
    }
    Ok(())
}

pub fn validate_owner(value: &str) -> CommitmentResult<()> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(CommitmentError::InvalidInput);
    }
    Ok(())
}
