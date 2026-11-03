//! 消息关系与动作报告定义层，不包含判断模型、执行器或磁盘实现。
mod experience;
pub use experience::*;
mod diagnostics;
pub use diagnostics::*;
use eve_config_api::{ConfigError, ConfigField, ConfigKind, ConfigSchema, ConfigSnapshot};
use eve_control_api::{ControlPhase, ControlReport, GenerationKey};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, future::Future, pin::Pin, sync::Arc};

pub const RELATION_PLUGIN_ID: &str = "eve.message.relations";
pub const RELATION_SERVICE_ID: &str = "eve.message.judge";
pub const ROUTER_PLUGIN_ID: &str = "eve.message.router";
pub const ROUTER_SERVICE_ID: &str = "eve.message.route";
pub const MESSAGE_NAMESPACE: &str = "runtime.messages";
pub type MessageResult<T> = Result<T, MessageError>;
pub type MessageFuture<'a, T> = Pin<Box<dyn Future<Output = MessageResult<T>> + Send + 'a>>;
pub type RelationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RelationDecision, RelationError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncomingMessage {
    pub message_id: String,
    pub target: GenerationKey,
    pub text: String,
    /// 仅匹配当前代由路由器最近发出的澄清 question_id。
    pub reply_to: Option<String>,
}
impl IncomingMessage {
    pub fn validate(&self) -> MessageResult<()> {
        self.target
            .session
            .validate()
            .map_err(|_| MessageError::InvalidInput)?;
        if !valid_id(&self.message_id)
            || !valid_id(&self.target.task_id)
            || self.target.generation == 0
            || self.text.trim().is_empty()
            || self.reply_to.as_deref().is_some_and(|v| !valid_id(v))
        {
            return Err(MessageError::InvalidInput);
        }
        Ok(())
    }
}
fn valid_id(v: &str) -> bool {
    !v.is_empty() && v.len() <= 256 && v.trim() == v && !v.chars().any(char::is_control)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationInput {
    pub message: IncomingMessage,
    pub phase: ControlPhase,
    pub task_text: String,
    pub cancel_requested: bool,
    pub started_tools: Option<u64>,
    pub clarification: Option<ClarificationContext>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClarificationContext {
    pub question_id: String,
    pub source_text: String,
    pub prompt: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageIntent {
    Supplement,
    Correction,
    Answer,
    NewTask,
    Cancel,
    Continue,
    Unrelated,
    Ambiguous,
    Pause,
    Resume,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentPart {
    pub intent: MessageIntent,
    pub confidence: u8,
    /// UTF-8 字节范围；动作只采用原消息中的文字，不采用判断器生成的要求。
    pub span: Option<TextSpan>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationDecision {
    pub target: GenerationKey,
    pub message_id: String,
    pub parts: Vec<IntentPart>,
    /// 简短可见依据，不包含内部推理。
    pub explanation: String,
}
impl RelationDecision {
    pub fn validate(&self, input: &RelationInput) -> Result<(), RelationError> {
        if self.target != input.message.target
            || self.message_id != input.message.message_id
            || self.parts.is_empty()
            || self.parts.len() > 16
            || self.explanation.trim().is_empty()
            || self.explanation.len() > 4096
        {
            return Err(RelationError::Protocol);
        }
        for (i, part) in self.parts.iter().enumerate() {
            let needs_text = matches!(
                part.intent,
                MessageIntent::Supplement
                    | MessageIntent::Correction
                    | MessageIntent::Answer
                    | MessageIntent::NewTask
            );
            if part.confidence > 100 || needs_text != part.span.is_some() {
                return Err(RelationError::Protocol);
            }
            if let Some(span) = part.span {
                if input
                    .message
                    .text
                    .get(span.start..span.end)
                    .is_none_or(|v| v.trim().is_empty())
                {
                    return Err(RelationError::Protocol);
                }
                if self.parts[..i]
                    .iter()
                    .filter_map(|p| p.span)
                    .any(|s| span.start < s.end && s.start < span.end)
                {
                    return Err(RelationError::Protocol);
                }
            } else if self.parts[..i].iter().any(|p| p.intent == part.intent) {
                return Err(RelationError::Protocol);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelationError {
    Unavailable,
    Protocol,
    Timeout,
    Panicked,
}
pub trait RelationJudge: Send + Sync {
    /// 示例仅是当前范围的历史分类数据；旧实现可忽略，最终决定仍绑定当前输入。
    fn judge_with_examples(
        &self,
        input: RelationInput,
        _examples: Vec<RelationExample>,
        observer: Arc<dyn RelationObserver>,
    ) -> RelationFuture<'_> {
        self.judge_observed(input, observer)
    }
    fn judge(&self, input: RelationInput) -> RelationFuture<'_>;

    /// 兼容旧实现，但明确标记不可观测；不能把缺失诊断解释为零调用。
    fn judge_observed(
        &self,
        input: RelationInput,
        observer: Arc<dyn RelationObserver>,
    ) -> RelationFuture<'_> {
        Box::pin(async move {
            observe_relation(observer.as_ref(), RelationObservation::Unsupported);
            self.judge(input).await
        })
    }
}
#[derive(Clone)]
pub struct RelationServiceHandle(pub Arc<dyn RelationJudge>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageTicket {
    pub message: IncomingMessage,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClarifyReason {
    LowConfidence,
    Conflict,
    Ambiguous,
    Unsupported,
    MissingQuestion,
    SideEffects,
    TooLarge,
    Judge(RelationError),
}
#[derive(Clone, Debug, PartialEq)]
pub enum RouteOutcome {
    Unchanged,
    Clarify {
        reason: ClarifyReason,
        question_id: String,
        prompt: String,
        prior: Option<Box<ControlReport>>,
    },
    Cancelled {
        prior: Box<ControlReport>,
    },
    Replaced {
        generation: GenerationKey,
        prior: Box<ControlReport>,
    },
    Blocked {
        prior: Option<Box<ControlReport>>,
    },
    Stale {
        prior: Option<Box<ControlReport>>,
    },
    ControlFailed {
        error: eve_control_api::ControlError,
        prior: Box<ControlReport>,
    },
    Stopped {
        prior: Option<Box<ControlReport>>,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct RouteReport {
    pub message: IncomingMessage,
    pub decision: Option<RelationDecision>,
    pub outcome: RouteOutcome,
}
pub trait MessageService: Send + Sync {
    /// 接受后由插件持有执行 Future；等待者离开不取消已接受的控制动作。
    /// 相同 message_id 和相同完整输入返回同票据；不同内容拒绝覆盖。
    fn submit(
        &self,
        message: IncomingMessage,
        sink: Arc<dyn eve_control_api::ControlEventSink>,
    ) -> MessageResult<MessageTicket>;
    /// 可信运行证据独立于消息正文传递，不参与消息序列化，也不得从正文重建。
    /// 同一消息 ID 携带不同 metadata 时必须拒绝，不能覆盖已受理请求的证据。
    /// 旧实现拒绝带 metadata 的请求，避免静默丢失证据；无 metadata 时转发 submit。
    fn submit_with_metadata(
        &self,
        message: IncomingMessage,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        sink: Arc<dyn eve_control_api::ControlEventSink>,
    ) -> MessageResult<MessageTicket> {
        if metadata.is_some() {
            return Err(MessageError::Unavailable);
        }
        self.submit(message, sink)
    }
    fn wait(&self, ticket: &MessageTicket) -> MessageFuture<'static, RouteReport>;
}
#[derive(Clone)]
pub struct MessageServiceHandle(pub Arc<dyn MessageService>);
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageError {
    InvalidInput,
    Unavailable,
    Conflict,
    LimitReached,
    Configuration,
}
impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "消息输入或目标标识无效",
            Self::Unavailable => "消息服务不可用",
            Self::Conflict => "消息标识已经用于不同输入",
            Self::LimitReached => "消息记录或输入大小已达上限",
            Self::Configuration => "消息配置不可用或无效",
        })
    }
}
impl std::error::Error for MessageError {}

pub fn message_schema() -> ConfigSchema {
    let specs = [
        ("confidence_threshold", 80, 1, 100),
        ("judge_timeout_ms", 2000, 1, 60000),
        ("max_input_bytes", 65536, 1, 1048576),
        ("max_tracked_messages", 256, 1, 65536),
    ];
    let mut fields: BTreeMap<_, _> = specs
        .into_iter()
        .map(|(name, default, min, max)| {
            let mut field = ConfigField::new(
                ConfigKind::Integer {
                    minimum: Some(min),
                    maximum: Some(max),
                },
                Some(default.into()),
            );
            field.environment = Some(format!("EVE_MESSAGE_{}", name.to_ascii_uppercase()));
            (name.into(), field)
        })
        .collect();
    let mut experience = ConfigField::new(ConfigKind::Boolean, Some(true.into()));
    experience.environment = Some("EVE_MESSAGE_EXPERIENCE_ENABLED".into());
    fields.insert("experience_enabled".into(), experience);
    ConfigSchema {
        namespace: MESSAGE_NAMESPACE.into(),
        version: 1,
        fields,
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageConfig {
    pub experience_enabled: bool,
    pub confidence_threshold: u8,
    pub judge_timeout_ms: u64,
    pub max_input_bytes: usize,
    pub max_tracked_messages: usize,
}
impl TryFrom<&ConfigSnapshot> for MessageConfig {
    type Error = ConfigError;
    fn try_from(s: &ConfigSnapshot) -> Result<Self, ConfigError> {
        if s.namespace != MESSAGE_NAMESPACE || s.schema_version != 1 {
            return Err(ConfigError::InvalidSchema(MESSAGE_NAMESPACE.into()));
        }
        for (name, field) in message_schema().fields {
            field.validate_value(
                &name,
                s.values
                    .get(&name)
                    .ok_or_else(|| ConfigError::MissingValue(name.clone()))?,
            )?;
        }
        Ok(Self {
            experience_enabled: s.get("experience_enabled")?,
            confidence_threshold: s.get("confidence_threshold")?,
            judge_timeout_ms: s.get("judge_timeout_ms")?,
            max_input_bytes: s.get("max_input_bytes")?,
            max_tracked_messages: s.get("max_tracked_messages")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_control_api::{ControlReviewEvidence, ControlRunMetadata, DiscardControlEvents};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct LegacyService(AtomicUsize);
    impl MessageService for LegacyService {
        fn submit(
            &self,
            message: IncomingMessage,
            _: Arc<dyn eve_control_api::ControlEventSink>,
        ) -> MessageResult<MessageTicket> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(MessageTicket { message })
        }

        fn wait(&self, _: &MessageTicket) -> MessageFuture<'static, RouteReport> {
            Box::pin(async { Err(MessageError::Unavailable) })
        }
    }

    #[test]
    fn legacy_service_rejects_metadata_without_submitting_and_forwards_none() {
        let legacy = LegacyService::default();
        let service: &dyn MessageService = &legacy;
        let message = IncomingMessage {
            message_id: "message".into(),
            target: GenerationKey {
                session: serde_json::from_value(serde_json::json!({
                    "session_id": "session", "user_id": "user"
                }))
                .unwrap(),
                task_id: "task".into(),
                controller_epoch: [1; 16],
                generation: 1,
            },
            text: "真实消息".into(),
            reply_to: None,
        };
        let metadata = ControlRunMetadata {
            scope: serde_json::from_value(serde_json::json!({
                "session_id": "session", "user_id": "user"
            }))
            .unwrap(),
            task_id: "task".into(),
            review: ControlReviewEvidence::ChannelAnchor("message".into()),
        };
        assert_eq!(
            service.submit_with_metadata(
                message.clone(),
                Some(metadata),
                Arc::new(DiscardControlEvents)
            ),
            Err(MessageError::Unavailable)
        );
        assert_eq!(legacy.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .submit_with_metadata(message.clone(), None, Arc::new(DiscardControlEvents))
                .unwrap(),
            MessageTicket { message }
        );
        assert_eq!(legacy.0.load(Ordering::SeqCst), 1);
    }
}
