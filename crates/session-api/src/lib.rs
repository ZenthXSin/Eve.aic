//! 会话历史与轮次记录的公开契约，不包含存储、模型调用或运行时实现。
use eve_llm_api::{ChatMessage, ChatRole, ModelRequest};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

pub const SESSION_PLUGIN_ID: &str = "eve.session";
pub const SESSION_SERVICE_ID: &str = "eve.session.history";
pub type SessionResult<T> = Result<T, SessionError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKey {
    pub session_id: String,
    pub user_id: String,
}
impl SessionKey {
    pub fn new(session_id: impl Into<String>, user_id: impl Into<String>) -> SessionResult<Self> {
        let key = Self {
            session_id: session_id.into(),
            user_id: user_id.into(),
        };
        key.validate()?;
        Ok(key)
    }
    pub fn validate(&self) -> SessionResult<()> {
        if [&self.session_id, &self.user_id].iter().any(|id| {
            id.is_empty()
                || id.len() > 256
                || id.trim() != id.as_str()
                || id.chars().any(char::is_control)
        }) {
            return Err(SessionError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInput {
    pub key: SessionKey,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnLease {
    pub key: SessionKey,
    pub turn_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionFailureCode {
    Context,
    Provider,
    ProviderTimeout,
    Protocol,
    Unsupported,
    RoundLimit,
    Backend,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFailure {
    pub code: SessionFailureCode,
    /// None 表示执行数量未知，不能据此断言没有副作用。
    pub started_tools: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", deny_unknown_fields)]
pub enum SessionTurnStatus {
    Pending,
    Completed { messages: Vec<ChatMessage> },
    Failed { failure: SessionFailure },
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTurn {
    pub id: u64,
    pub input: String,
    pub status: SessionTurnStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub key: SessionKey,
    pub revision: u64,
    pub turns: Vec<SessionTurn>,
}
impl SessionSnapshot {
    /// 只回放完成的整轮，失败或中断输入不会被伪装成成功对话。
    pub fn history(&self) -> Vec<ChatMessage> {
        self.turns
            .iter()
            .flat_map(|turn| match &turn.status {
                SessionTurnStatus::Completed { messages } => messages.clone(),
                _ => vec![],
            })
            .collect()
    }
    pub fn validate(&self) -> SessionResult<()> {
        self.key.validate()?;
        let mut revision = 0u64;
        for (index, turn) in self.turns.iter().enumerate() {
            if turn.id != index as u64 + 1 || turn.input.trim().is_empty() {
                return Err(SessionError::CorruptState);
            }
            revision = revision.checked_add(1).ok_or(SessionError::LimitReached)?;
            match &turn.status {
                SessionTurnStatus::Pending => {
                    if index + 1 != self.turns.len() {
                        return Err(SessionError::CorruptState);
                    }
                }
                SessionTurnStatus::Completed { messages } => {
                    validate_completed_turn(&turn.input, messages)?;
                    revision = revision.checked_add(1).ok_or(SessionError::LimitReached)?;
                }
                _ => {
                    revision = revision.checked_add(1).ok_or(SessionError::LimitReached)?;
                }
            }
        }
        if revision != self.revision {
            return Err(SessionError::CorruptState);
        }
        Ok(())
    }
}

/// 整轮记录只含用户输入、配对的调用/结果与最终回复，不保存系统提示。
pub fn validate_completed_turn(input: &str, messages: &[ChatMessage]) -> SessionResult<()> {
    if input.trim().is_empty() || messages.len() < 2 {
        return Err(SessionError::InvalidInput);
    }
    let first = &messages[0];
    let last = messages.last().expect("length checked");
    if first.role != ChatRole::User
        || first.text.as_deref() != Some(input)
        || last.role != ChatRole::Assistant
        || last.text.is_none()
        || !last.tool_calls.is_empty()
    {
        return Err(SessionError::InvalidInput);
    }
    let middle = &messages[1..messages.len() - 1];
    if !middle.len().is_multiple_of(2)
        || middle.chunks_exact(2).any(|pair| {
            pair[0].role != ChatRole::Assistant
                || pair[0].tool_calls.is_empty()
                || pair[1].role != ChatRole::Tool
        })
    {
        return Err(SessionError::InvalidInput);
    }
    ModelRequest {
        messages: messages.to_vec(),
        tools: vec![],
    }
    .validate()
    .map_err(|_| SessionError::InvalidInput)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartedTurn {
    pub lease: TurnLease,
    pub revision: u64,
    pub history: Vec<ChatMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionError {
    InvalidInput,
    OwnerMismatch,
    Busy,
    StaleTurn,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
}
impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "会话输入或完整轮次无效",
            Self::OwnerMismatch => "会话用户不匹配",
            Self::Busy => "会话已有在途轮次",
            Self::StaleTurn => "轮次引用已失效",
            Self::Unavailable => "会话服务不可用",
            Self::CorruptState => "会话状态损坏",
            Self::UnsupportedVersion => "不支持会话状态版本",
            Self::Storage => "会话状态读取或提交失败",
            Self::LimitReached => "会话计数已达上限",
        })
    }
}
impl std::error::Error for SessionError {}

pub trait SessionService: Send + Sync {
    fn snapshot(&self, key: &SessionKey) -> SessionResult<Option<SessionSnapshot>>;
    /// 成功后输入与 Pending 已提交；同一会话只允许一个在途轮次。
    fn begin(&self, input: SessionInput) -> SessionResult<StartedTurn>;
    /// 保存完整轮次后才报告完成；失败不改变当前状态。
    fn complete(&self, lease: &TurnLease, messages: Vec<ChatMessage>) -> SessionResult<()>;
    fn fail(&self, lease: &TurnLease, failure: SessionFailure) -> SessionResult<()>;
}
#[derive(Clone)]
pub struct SessionServiceHandle(pub Arc<dyn SessionService>);
