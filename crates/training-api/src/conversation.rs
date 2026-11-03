//! 从真实交流中学习接话方式；这些经验只是可修正的数据，不是事实或授权。
use eve_llm_api::{ContextScope, LlmFuture};
use serde::{Deserialize, Serialize};

pub const CONVERSATION_BATCH_MESSAGES: usize = 4;
pub const MAX_CONVERSATION_MESSAGES: usize = 16;
pub const MAX_CONVERSATION_BODY_BYTES: usize = 2048;
pub const MAX_CONVERSATION_LESSONS: usize = 24;
pub const MAX_CONVERSATION_DRAFTS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    Human,
    Eve,
}

/// 由通道提供已验证的身份与正文；speaker 是当前范围内的匿名标识。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMessage {
    pub id: String,
    pub speaker: String,
    pub role: ConversationRole,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationEvidence {
    pub message_id: String,
    pub quote: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationAssessment {
    /// 不把出现过、无人反对或成功送达视为认可。
    Observation,
    /// 对 Eve 具体表达的直接纠正；必须有 Eve 发言之后的真人证据。
    Correction,
    /// 真实反例；停用旧经验，保留证据而不是删除历史。
    Counterexample,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLessonDraft {
    pub lesson_id: Option<String>,
    pub situation: String,
    pub response: String,
    pub avoid: String,
    pub assessment: ConversationAssessment,
    pub evidence: Vec<ConversationEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLesson {
    pub id: String,
    pub situation: String,
    pub response: String,
    pub avoid: String,
    pub observations: u32,
    pub corrections: u32,
    pub counterexamples: u32,
    pub active: bool,
    pub updated_at_ms: u64,
    pub evidence: Vec<ConversationEvidence>,
    /// 已用于支持/反例计数的真人消息编号，防滚动窗口重复强化同一证据。
    pub evaluated_message_ids: Vec<String>,
}

/// 相邻真人与 Eve 发言保持顺序；不同人不拼成同一个人的意见。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationBatch {
    pub id: String,
    pub scope: ContextScope,
    pub messages: Vec<ConversationMessage>,
    pub lessons: Vec<ConversationLesson>,
    pub attempt: u8,
}

pub trait ConversationLearner: Send + Sync {
    fn learn(&self, batch: ConversationBatch) -> LlmFuture<'_, Vec<ConversationLessonDraft>>;
}
