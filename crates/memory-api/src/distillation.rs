//! 有来源的抽取式会话视图；不是语义归纳、已确认偏好或新事实。
use crate::{MemoryError, MemoryResult, MemorySnapshot};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DistillationOptions {
    pub max_episodes: usize,
    pub max_expression_examples: usize,
    /// JSON 视图的 UTF-8 字节预算；不包括调用方的固定说明。
    pub max_bytes: usize,
}
impl Default for DistillationOptions {
    fn default() -> Self {
        Self {
            max_episodes: 4,
            max_expression_examples: 3,
            max_bytes: 6144,
        }
    }
}
impl DistillationOptions {
    pub fn validate(self) -> MemoryResult<()> {
        if self.max_episodes > 8
            || self.max_expression_examples > 4
            || !(512..=16384).contains(&self.max_bytes)
        {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct VerbatimExcerpt {
    pub text: String,
    pub truncated: bool,
    pub source_characters: usize,
}
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ConversationEpisode {
    pub evidence_id: String,
    pub evidence_revision: u64,
    pub at_ms: u64,
    pub turn_id: u64,
    pub user: VerbatimExcerpt,
    /// 仅代表旧助手当时说过的话；不作为实际执行、用户认可或事实验证证据。
    pub assistant: VerbatimExcerpt,
}
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ExpressionExample {
    pub evidence_id: String,
    pub user: VerbatimExcerpt,
}
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ConversationDigest {
    pub kind: String,
    pub method: String,
    pub memory_revision: u64,
    pub completed_interactions: usize,
    pub episodes: Vec<ConversationEpisode>,
    pub expression_examples: Vec<ExpressionExample>,
}
pub trait ConversationDistiller: Send + Sync {
    /// 仅提供可信宿主允许且属于本快照的表达证据 ID；原始快照不修改。
    fn distill(
        &self,
        snapshot: &MemorySnapshot,
        options: DistillationOptions,
        expression_evidence_ids: &[String],
    ) -> MemoryResult<ConversationDigest>;
}
