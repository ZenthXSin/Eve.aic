//! 可信宿主绑定的模型会话。会话键只来自角色、作用域和持久任务锚点，不读取提示词。
use crate::{ContextScope, LlmError};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};

/// 一个长期模型会话及可选的当前轮次。
///
/// 宿主负责创建和传递此值；Provider 不能用模型名、正文或请求时随机数补造它。
/// 所有对外标识均为领域隔离的 SHA-256，不保存或发送明文用户/通道/任务标识。
/// `turn_id` 用于单轮状态，`session_id`、`thread_id` 和缓存键在换轮时不变。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SessionRecord", into = "SessionRecord")]
pub struct ModelSession {
    session_id: String,
    scope_id: Option<String>,
    thread_id: String,
    prompt_cache_key: String,
    turn_id: Option<String>,
    client_request_id: String,
    request_ordinal: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRecord {
    version: u32,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(default)]
    request_ordinal: u64,
}

impl ModelSession {
    /// 一个受信通道内某角色的长期会话。不同用户、通道或角色不能复用缓存键。
    pub fn scoped(role: &str, scope: &ContextScope) -> Result<Self, LlmError> {
        Self::construct(role, Some(scope), None)
    }

    /// 一个后台任务、探索或子代理的独立会话。anchor 必须是宿主持久 ID，
    /// 例如 `task.id`；恢复进程时使用相同 ID。不能填正文、时间戳或配置中的全局固定值。
    /// 本地非通道入口可传 None，但仍须由入口提供自己的稳定会话 ID。
    pub fn anchored(
        role: &str,
        scope: Option<&ContextScope>,
        anchor: &str,
    ) -> Result<Self, LlmError> {
        validate_anchor(anchor)?;
        Self::construct(role, scope, Some(anchor))
    }

    /// 已持久化且由宿主核对所有者的内部作业；用于没有通道 ContextScope 的业务账本。
    /// owner 是账本真实所有者，anchor 是该账本的作业 ID，二者都不进入对外明文标识。
    pub fn owned(role: &str, owner: &str, anchor: &str) -> Result<Self, LlmError> {
        validate_role(role)?;
        validate_anchor(owner)?;
        validate_anchor(anchor)?;
        Ok(Self::from_parts(
            digest("owned", &[role, owner, anchor]),
            None,
            None,
            0,
        ))
    }

    fn construct(
        role: &str,
        scope: Option<&ContextScope>,
        anchor: Option<&str>,
    ) -> Result<Self, LlmError> {
        validate_role(role)?;
        if let Some(scope) = scope {
            validate_anchor(&scope.session_id)?;
            validate_anchor(&scope.user_id)?;
        }
        let session_id = digest(
            "session",
            &[
                role,
                if scope.is_some() { "scoped" } else { "local" },
                scope.map_or("", |s| s.session_id.as_str()),
                scope.map_or("", |s| s.user_id.as_str()),
                if anchor.is_some() {
                    "anchored"
                } else {
                    "conversation"
                },
                anchor.unwrap_or(""),
            ],
        );
        Ok(Self::from_parts(session_id, scope.map(scope_id), None, 0))
    }

    /// 绑定可信持久轮次或入站事件 ID，不改变长期会话。工具往返继续沿用该轮次。
    pub fn with_turn(self, trusted_turn: &str) -> Result<Self, LlmError> {
        validate_anchor(trusted_turn)?;
        let turn_id = digest("turn", &[&self.session_id, trusted_turn]);
        Ok(Self::from_parts(
            self.session_id,
            self.scope_id,
            Some(turn_id),
            0,
        ))
    }

    /// 单轮内每次模型调用的稳定序号；传输重试必须复用同一返回值。
    /// 序号来自宿主的持久 provider_requests，不从 prompt 或响应推断。
    pub fn for_request(&self, ordinal: u64) -> Self {
        Self::from_parts(
            self.session_id.clone(),
            self.scope_id.clone(),
            self.turn_id.clone(),
            ordinal,
        )
    }

    /// 从受信父会话创建独立用途或子任务。None 表示该用途的长期会话，
    /// Some 必须是独立任务 ID；不能拿工具参数中的自报身份当锚点。
    /// 当前轮次也按子会话隔离，长期缓存键不受父轮次变化影响。
    pub fn fork(&self, role: &str, stable_anchor: Option<&str>) -> Result<Self, LlmError> {
        self.validate()?;
        validate_role(role)?;
        if let Some(anchor) = stable_anchor {
            validate_anchor(anchor)?;
        }
        let session_id = digest(
            "fork",
            &[
                &self.session_id,
                role,
                if stable_anchor.is_some() {
                    "anchored"
                } else {
                    "role"
                },
                stable_anchor.unwrap_or(""),
            ],
        );
        let turn_id = self
            .turn_id
            .as_deref()
            .map(|turn| digest("fork-turn", &[&session_id, turn]));
        Ok(Self::from_parts(
            session_id,
            self.scope_id.clone(),
            turn_id,
            self.request_ordinal,
        ))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
    pub fn prompt_cache_key(&self) -> &str {
        &self.prompt_cache_key
    }
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }
    pub fn client_request_id(&self) -> &str {
        &self.client_request_id
    }

    /// 组合层传来的会话必须仍属于当前可信范围。
    pub fn validate_scope(&self, scope: Option<&ContextScope>) -> Result<(), LlmError> {
        self.validate()?;
        if self.scope_id != scope.map(scope_id) {
            return Err(LlmError::Configuration(
                "模型会话与当前可信作用域不匹配".into(),
            ));
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), LlmError> {
        if !opaque_id(&self.session_id)
            || self
                .scope_id
                .as_deref()
                .is_some_and(|scope| !opaque_id(scope))
            || self.turn_id.as_deref().is_some_and(|turn| !opaque_id(turn))
            || *self
                != Self::from_parts(
                    self.session_id.clone(),
                    self.scope_id.clone(),
                    self.turn_id.clone(),
                    self.request_ordinal,
                )
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn from_parts(
        session_id: String,
        scope_id: Option<String>,
        turn_id: Option<String>,
        request_ordinal: u64,
    ) -> Self {
        let thread_id = digest("thread", &[&session_id]);
        let prompt_cache_key = digest("prompt-cache", &[&session_id]);
        let client_request_id = digest(
            "request",
            &[
                &session_id,
                turn_id.as_deref().unwrap_or(""),
                &request_ordinal.to_string(),
            ],
        );
        Self {
            session_id,
            scope_id,
            thread_id,
            prompt_cache_key,
            turn_id,
            client_request_id,
            request_ordinal,
        }
    }
}

impl From<ModelSession> for SessionRecord {
    fn from(value: ModelSession) -> Self {
        Self {
            version: 1,
            session_id: value.session_id,
            scope_id: value.scope_id,
            turn_id: value.turn_id,
            request_ordinal: value.request_ordinal,
        }
    }
}

impl TryFrom<SessionRecord> for ModelSession {
    type Error = LlmError;
    fn try_from(record: SessionRecord) -> Result<Self, Self::Error> {
        if record.version != 1 {
            return Err(invalid());
        }
        let session = Self::from_parts(
            record.session_id,
            record.scope_id,
            record.turn_id,
            record.request_ordinal,
        );
        session.validate()?;
        Ok(session)
    }
}

fn validate_role(role: &str) -> Result<(), LlmError> {
    if role.is_empty()
        || role.len() > 96
        || !role
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_anchor(anchor: &str) -> Result<(), LlmError> {
    if anchor.trim().is_empty() || anchor.len() > 4096 || anchor.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(())
}

fn opaque_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn scope_id(scope: &ContextScope) -> String {
    digest("scope", &[&scope.session_id, &scope.user_id])
}

fn invalid() -> LlmError {
    LlmError::Configuration("模型会话元数据无效；需要可信角色、作用域和持久锚点".into())
}

fn digest(domain: &str, fields: &[&str]) -> String {
    let mut digest = Context::new(&SHA256);
    for field in std::iter::once("eve.model-session.v1")
        .chain(std::iter::once(domain))
        .chain(fields.iter().copied())
    {
        digest.update(&(field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ContextScope {
        ContextScope {
            session_id: "qq:dm:private-session".into(),
            user_id: "private-user-123".into(),
        }
    }

    #[test]
    fn role_scope_and_task_are_distinct_while_turns_keep_long_session() {
        let main = ModelSession::scoped("main", &scope()).unwrap();
        let first = main.clone().with_turn("1").unwrap();
        let second = main.clone().with_turn("2").unwrap();
        assert_eq!(first.session_id(), second.session_id());
        assert_eq!(first.thread_id(), second.thread_id());
        assert_eq!(first.prompt_cache_key(), second.prompt_cache_key());
        assert_ne!(first.turn_id(), second.turn_id());
        assert_ne!(first.client_request_id(), second.client_request_id());
        assert_ne!(main, ModelSession::scoped("review", &scope()).unwrap());
        let mut other = scope();
        other.user_id.push('2');
        assert_ne!(main, ModelSession::scoped("main", &other).unwrap());
        other = scope();
        other.session_id.push('2');
        assert_ne!(main, ModelSession::scoped("main", &other).unwrap());
        let task = ModelSession::anchored("task", Some(&scope()), "task-a").unwrap();
        assert_ne!(task, main);
        assert_ne!(
            task,
            ModelSession::anchored("task", Some(&scope()), "task-b").unwrap()
        );
        assert_ne!(
            task,
            ModelSession::anchored("exploration", Some(&scope()), "task-a").unwrap()
        );
    }

    #[test]
    fn request_attempts_and_child_roles_are_explicit_and_restorable() {
        let main = ModelSession::scoped("main", &scope())
            .unwrap()
            .with_turn("12")
            .unwrap();
        let first = main.for_request(1);
        assert_eq!(first, main.for_request(1));
        assert_ne!(
            first.client_request_id(),
            main.for_request(2).client_request_id()
        );
        assert_eq!(
            first.prompt_cache_key(),
            main.for_request(2).prompt_cache_key()
        );
        let child = main.fork("review", None).unwrap();
        assert_ne!(main.session_id(), child.session_id());
        assert_ne!(main.turn_id(), child.turn_id());
        assert_ne!(child, main.fork("review", Some("other-task")).unwrap());
        let serialized = serde_json::to_string(&first).unwrap();
        assert!(!serialized.contains(&scope().user_id));
        assert!(!serialized.contains(&scope().session_id));
        assert_eq!(first, serde_json::from_str(&serialized).unwrap());
    }

    #[test]
    fn framing_prevents_scope_field_collisions_and_invalid_records_fail() {
        let a = ContextScope {
            session_id: "ab".into(),
            user_id: "c".into(),
        };
        let b = ContextScope {
            session_id: "a".into(),
            user_id: "bc".into(),
        };
        assert_ne!(
            ModelSession::scoped("main", &a).unwrap(),
            ModelSession::scoped("main", &b).unwrap()
        );
        assert!(ModelSession::scoped("main\nforged", &a).is_err());
        assert!(ModelSession::anchored("task", Some(&a), " ").is_err());
        assert!(
            serde_json::from_value::<ModelSession>(serde_json::json!({
                "version": 1, "session_id": "raw-private-id", "request_ordinal": 0
            }))
            .is_err()
        );
        let mut value = serde_json::to_value(ModelSession::scoped("main", &a).unwrap()).unwrap();
        value["version"] = serde_json::json!(2);
        assert!(serde_json::from_value::<ModelSession>(value).is_err());
    }

    #[test]
    fn restored_and_forked_bindings_reject_a_different_trusted_scope() {
        let session = ModelSession::scoped("main", &scope()).unwrap();
        let session: ModelSession =
            serde_json::from_value(serde_json::to_value(session).unwrap()).unwrap();
        let mut other = scope();
        other.user_id.push('2');
        for session in [session.clone(), session.fork("review", None).unwrap()] {
            session.validate_scope(Some(&scope())).unwrap();
            assert!(session.validate_scope(Some(&other)).is_err());
            assert!(session.validate_scope(None).is_err());
        }
        let local = ModelSession::anchored("local", None, "persisted-local-session").unwrap();
        local.validate_scope(None).unwrap();
        assert!(local.validate_scope(Some(&scope())).is_err());
    }

    #[test]
    fn tool_models_keep_parent_task_isolation_and_do_not_trust_textual_identity() {
        use crate::ToolExecutionContext;
        let tool_context = |task: &str, turn: &str, call: &str| {
            ToolExecutionContext::new()
                .with_scope(Some(scope()))
                .with_call_id(call.into())
                .with_model_session(
                    ModelSession::anchored("task", Some(&scope()), task)
                        .unwrap()
                        .with_turn(turn)
                        .unwrap(),
                )
        };
        let review =
            |context: ToolExecutionContext| context.model_session_for("review").unwrap().unwrap();
        let first = review(tool_context("task-a", "turn-a", "call-a"));
        let second = review(tool_context("task-a", "turn-a", "call-b"));
        let next_turn = review(tool_context("task-a", "turn-b", "call-a"));
        let other_task = review(tool_context("task-b", "turn-a", "call-a"));
        assert_eq!(first.session_id(), second.session_id());
        assert_eq!(first.session_id(), next_turn.session_id());
        assert_ne!(first.turn_id(), second.turn_id());
        assert_ne!(first.turn_id(), next_turn.turn_id());
        assert_ne!(first.session_id(), other_task.session_id());
        assert_eq!(first, review(tool_context("task-a", "turn-a", "call-a")));
        assert!(
            ToolExecutionContext::new()
                .model_session_for("review")
                .unwrap()
                .is_none()
        );
        let mut other_scope = scope();
        other_scope.session_id.push('2');
        assert!(
            tool_context("task-a", "turn-a", "call-a")
                .with_scope(Some(other_scope))
                .model_session_for("review")
                .is_err()
        );
    }
}
