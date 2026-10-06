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

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", deny_unknown_fields)]
pub enum SessionTurnStatus {
    Pending,
    Completed { messages: Vec<ChatMessage> },
    Failed { failure: SessionFailure },
    Interrupted,
}

impl<'de> Deserialize<'de> for SessionTurnStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // serde 的带内部标签 unit variant 会忽略其余字段；使用空 struct
        // variant 解析，确保 Pending/Interrupted 也严格拒绝未知字段。
        #[derive(Deserialize)]
        #[serde(tag = "state", deny_unknown_fields)]
        enum Wire {
            Pending {},
            Completed { messages: Vec<ChatMessage> },
            Failed { failure: SessionFailure },
            Interrupted {},
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Pending {} => Self::Pending,
            Wire::Completed { messages } => Self::Completed { messages },
            Wire::Failed { failure } => Self::Failed { failure },
            Wire::Interrupted {} => Self::Interrupted,
        })
    }
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
        || middle.as_chunks::<2>().0.iter().any(|pair| {
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
    /// 分页列出本服务已有历史的完整可信身份，不返回正文或诊断，也不创建或重放轮次。
    /// 按 session_id 升序，after 是独占游标，可不对应已有会话；Some 游标须非空、
    /// UTF-8 字节数不超过 256 且无控制字符，允许空格。limit 必须为 1..=100。
    /// 单页是读取时的快照，跨页不提供事务快照；调用者以末项 session_id 继续。
    /// 停止后的句柄不可用；旧的自定义实现默认不支持枚举。
    fn list_keys(&self, _after: Option<&str>, _limit: usize) -> SessionResult<Vec<SessionKey>> {
        Err(SessionError::Unavailable)
    }
    fn snapshot(&self, key: &SessionKey) -> SessionResult<Option<SessionSnapshot>>;
    /// 成功后输入与 Pending 已提交；同一会话只允许一个在途轮次。
    fn begin(&self, input: SessionInput) -> SessionResult<StartedTurn>;
    /// 保存完整轮次后才报告完成；失败不改变当前状态。
    fn complete(&self, lease: &TurnLease, messages: Vec<ChatMessage>) -> SessionResult<()>;
    fn fail(&self, lease: &TurnLease, failure: SessionFailure) -> SessionResult<()>;
}
#[derive(Clone)]
pub struct SessionServiceHandle(pub Arc<dyn SessionService>);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_statuses_reject_unknown_fields_and_roundtrip() {
        for status in [
            SessionTurnStatus::Pending,
            SessionTurnStatus::Interrupted,
            SessionTurnStatus::Failed {
                failure: SessionFailure {
                    code: SessionFailureCode::Cancelled,
                    started_tools: None,
                },
            },
            SessionTurnStatus::Completed { messages: vec![] },
        ] {
            let mut encoded = serde_json::to_value(&status).unwrap();
            assert_eq!(
                serde_json::from_value::<SessionTurnStatus>(encoded.clone()).unwrap(),
                status
            );
            encoded["unknown"] = json!(true);
            assert!(serde_json::from_value::<SessionTurnStatus>(encoded).is_err());
        }
    }

    #[test]
    fn invalid_user_ids_and_incomplete_batches_are_rejected() {
        for id in ["", " leading", "trailing ", "control\n", &"x".repeat(257)] {
            assert_eq!(SessionKey::new(id, "u"), Err(SessionError::InvalidInput));
            assert_eq!(SessionKey::new("s", id), Err(SessionError::InvalidInput));
        }
        assert!(SessionKey::new("中文会话", "中文用户").is_ok());
        let valid = vec![
            ChatMessage::text(ChatRole::User, "问题"),
            ChatMessage::text(ChatRole::Assistant, "回复"),
        ];
        assert!(validate_completed_turn("问题", &valid).is_ok());
        let mut incomplete = valid;
        incomplete.insert(1, ChatMessage::text(ChatRole::Assistant, "多余消息"));
        assert_eq!(
            validate_completed_turn("问题", &incomplete),
            Err(SessionError::InvalidInput)
        );
    }
}

#[cfg(test)]
mod enumeration_compatibility {
    use super::*;

    struct CustomSessions;
    impl SessionService for CustomSessions {
        fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            Ok(None)
        }
        fn begin(&self, _: SessionInput) -> SessionResult<StartedTurn> {
            Err(SessionError::Unavailable)
        }
        fn complete(&self, _: &TurnLease, _: Vec<ChatMessage>) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
        fn fail(&self, _: &TurnLease, _: SessionFailure) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
    }

    #[test]
    fn existing_custom_service_can_omit_enumeration() {
        let custom: &dyn SessionService = &CustomSessions;
        assert_eq!(custom.list_keys(None, 10), Err(SessionError::Unavailable));
    }
}
