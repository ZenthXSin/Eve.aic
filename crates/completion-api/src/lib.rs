//! 目标完成确认的提问记录：一份计划的所有步骤都被证据满足后，在目标所属用户下一次私聊时，
//! 随回复问一句这件事算不算完成。每份计划至多问一次（发不出去时至多再试两次）。
//!
//! 提问先保存为“发送中”再附带；发送中进程退出时记为结果未知，不再问。用户的确认写在认知状态里，
//! 这里只记录问过没有。
use serde::{Deserialize, Serialize};
use std::fmt;

pub const COMPLETION_PLUGIN_ID: &str = "eve.completion";
pub const COMPLETION_STATE_KEY: &str = "questions.v1";
/// 投递结果按 ID 前缀交回本来源。
pub const ID_PREFIX: &str = "goal-done-";
/// 账本至多保存的记录；满了时删除最早结束的一条，没有可删的就不再新增。
pub const MAX_QUESTIONS: usize = 512;
/// 同一份计划至多附带的次数：发不出去时下次私聊再试，用尽后不再问。
pub const MAX_ATTEMPTS: usize = 3;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

pub type CompletionResult<T> = Result<T, CompletionError>;

/// 要问的那份计划与目标。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionTarget {
    /// 目标所属用户；只在这位用户的私聊中问。
    pub owner: String,
    pub goal_id: String,
    pub goal_revision: u64,
    pub plan_id: String,
    pub plan_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptResult {
    /// 平台确认发送成功。
    Sent { platform_message_id: Option<String> },
    /// 平台拒绝或发送失败。
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：前面的回复片段失败或回复已过期。
    NotSent,
    /// 写出前后进程退出，是否送达不确定；不再问。
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAttempt {
    /// 附带在这条用户私聊消息的回复后。
    pub message_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub result: Option<AttemptResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum QuestionStatus {
    /// 已保存尝试，正在随回复发送。
    Delivering,
    /// 上次没有发出去，下次私聊再试。
    Retry,
    Delivered,
    /// 送达与否不确定，不再问。
    Unknown,
    /// 尝试用尽仍未送达，不再问。
    Exhausted,
}
impl QuestionStatus {
    pub fn closed(self) -> bool {
        matches!(self, Self::Delivered | Self::Unknown | Self::Exhausted)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionQuestion {
    /// 由所属用户与计划 ID 确定；以 `ID_PREFIX` 开头。
    pub id: String,
    pub target: QuestionTarget,
    pub status: QuestionStatus,
    pub attempts: Vec<QuestionAttempt>,
    pub created_at_ms: u64,
    pub closed_at_ms: Option<u64>,
}

#[derive(Clone, Default)]
pub struct CompletionSnapshot {
    pub questions: Vec<CompletionQuestion>,
}
impl CompletionSnapshot {
    pub fn for_plan(&self, owner: &str, plan_id: &str) -> Option<&CompletionQuestion> {
        let id = question_id(owner, plan_id);
        self.questions.iter().find(|question| question.id == id)
    }
    /// 这份计划此刻还可以问：从没问过，或上次没发出去且还有次数。
    pub fn askable(&self, owner: &str, plan_id: &str) -> bool {
        self.for_plan(owner, plan_id)
            .is_none_or(|question| question.status == QuestionStatus::Retry)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionError {
    InvalidInput,
    NotFound,
    /// 这份计划已经问过（送达、未知或用尽），或正在发送。
    AlreadyAsked,
    /// 记录不在可执行该操作的状态。
    InvalidTransition,
    LimitReached,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    /// 提交无法确认；实例已关闭，须重新打开核对。
    Storage,
}
impl fmt::Display for CompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "完成确认提问的输入无效",
            Self::NotFound => "没有这条完成确认提问",
            Self::AlreadyAsked => "这份计划已经问过是否完成",
            Self::InvalidTransition => "完成确认提问不在可执行该操作的状态",
            Self::LimitReached => "完成确认提问记录已达上限",
            Self::Unavailable => "完成确认提问记录不可用",
            Self::CorruptState => "完成确认提问记录损坏；未清空",
            Self::UnsupportedVersion => "不支持该完成确认提问记录版本",
            Self::Storage => "完成确认提问记录提交无法确认；须重新打开核对",
        })
    }
}
impl std::error::Error for CompletionError {}

pub fn validate_id(value: &str) -> CompletionResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(CompletionError::InvalidInput);
    }
    Ok(())
}

impl QuestionTarget {
    pub fn validate(&self) -> CompletionResult<()> {
        for id in [&self.owner, &self.goal_id, &self.plan_id] {
            validate_id(id)?;
        }
        if self.goal_revision == 0 || self.plan_revision == 0 {
            return Err(CompletionError::InvalidInput);
        }
        Ok(())
    }
}

impl CompletionQuestion {
    /// 状态与尝试记录一致：发送中恰有一次未结束的尝试，其余尝试都已有结果。
    pub fn validate(&self) -> CompletionResult<()> {
        self.target.validate()?;
        if self.id != question_id(&self.target.owner, &self.target.plan_id)
            || self.created_at_ms == 0
            || self.attempts.is_empty()
            || self.attempts.len() > MAX_ATTEMPTS
            || self.status.closed() != self.closed_at_ms.is_some()
        {
            return Err(CompletionError::InvalidInput);
        }
        let (last, earlier) = self
            .attempts
            .split_last()
            .ok_or(CompletionError::InvalidInput)?;
        for attempt in self.attempts.iter() {
            validate_id(&attempt.message_id)?;
            if attempt.started_at_ms == 0
                || attempt.result.is_some() != attempt.finished_at_ms.is_some()
            {
                return Err(CompletionError::InvalidInput);
            }
        }
        if earlier.iter().any(|attempt| {
            !matches!(
                attempt.result,
                Some(AttemptResult::Failed { .. } | AttemptResult::NotSent)
            )
        }) {
            return Err(CompletionError::InvalidInput);
        }
        let consistent = match self.status {
            QuestionStatus::Delivering => last.result.is_none(),
            QuestionStatus::Delivered => matches!(last.result, Some(AttemptResult::Sent { .. })),
            QuestionStatus::Unknown => last.result == Some(AttemptResult::Unknown),
            QuestionStatus::Retry => {
                self.attempts.len() < MAX_ATTEMPTS
                    && matches!(
                        last.result,
                        Some(AttemptResult::Failed { .. } | AttemptResult::NotSent)
                    )
            }
            QuestionStatus::Exhausted => {
                self.attempts.len() == MAX_ATTEMPTS
                    && matches!(
                        last.result,
                        Some(AttemptResult::Failed { .. } | AttemptResult::NotSent)
                    )
            }
        };
        if !consistent {
            return Err(CompletionError::InvalidInput);
        }
        Ok(())
    }
}

/// 同一用户同一计划的提问 ID 固定，重复认领不会产生第二条记录。
pub fn question_id(owner: &str, plan_id: &str) -> String {
    use ring::digest::{Context, SHA256};
    let mut context = Context::new(&SHA256);
    for part in ["eve.completion:v1", owner, plan_id] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let hex: String = context
        .finish()
        .as_ref()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{ID_PREFIX}{hex}")
}

/// 只交给可信宿主；不发布给模型或不受信插件。
pub trait CompletionLedger: Send + Sync {
    fn snapshot(&self) -> CompletionResult<CompletionSnapshot>;
    /// 先保存“发送中”再附带。从没问过时新建记录；上次没发出去且还有次数时再试一次；
    /// 其余情况返回 `AlreadyAsked`，不写入。
    fn claim(
        &self,
        target: &QuestionTarget,
        message_id: &str,
        now_ms: u64,
    ) -> CompletionResult<CompletionQuestion>;
    /// 记录这次附带的结果；只接受发送中的记录。
    fn record(
        &self,
        id: &str,
        result: AttemptResult,
        now_ms: u64,
    ) -> CompletionResult<CompletionQuestion>;
}

impl fmt::Debug for CompletionQuestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CompletionQuestion(<redacted>)")
    }
}
impl fmt::Debug for CompletionSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CompletionSnapshot(<redacted>)")
    }
}
