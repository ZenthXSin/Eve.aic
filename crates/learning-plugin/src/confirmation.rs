//! 模型自评只是准入条件；自动确认仍绑定当前范围的真实完成证据。
use eve_learning_api::*;
use eve_memory_api::{MemorySnapshot, validate_id};

#[derive(Default)]
pub struct EvidenceConfirmationPolicy;
impl AutoConfirmationPolicy for EvidenceConfirmationPolicy {
    fn allows(
        &self,
        candidate: &PreferenceCandidate,
        batch: &LearningBatch,
        memory: &MemorySnapshot,
        now_ms: u64,
    ) -> LearningResult<bool> {
        batch.scope.validate()?;
        validate_id(&candidate.id)?;
        if memory.scope != batch.scope || candidate.batch_id != batch.id {
            return Err(LearningError::InvalidInput);
        }
        crate::validate_drafts(batch, std::slice::from_ref(&candidate.draft))?;
        if candidate.draft.evidence_ids.iter().any(|id| {
            let source = batch.evidence.iter().find(|source| source.id == *id);
            !memory
                .evidence
                .iter()
                .any(|current| Some(current) == source)
        }) {
            return Err(LearningError::InvalidInput);
        }
        Ok(candidate.created_at_ms <= now_ms
            && now_ms < candidate.expires_at_ms
            && candidate.draft.confidence >= 80
            && candidate.draft.evidence_ids.len() >= 2)
    }
}
