//! 模型自评只是准入条件；自动决定仍绑定真实完成证据和全部偏好历史。
use eve_learning_api::*;
use eve_memory_api::MemorySnapshot;

#[derive(Default)]
pub struct EvidenceConfirmationPolicy;
impl AutoConfirmationPolicy for EvidenceConfirmationPolicy {
    fn version(&self) -> &str {
        crate::decision::EVIDENCE_POLICY_VERSION
    }

    fn allows(
        &self,
        candidate: &PreferenceCandidate,
        batch: &LearningBatch,
        memory: &MemorySnapshot,
        now_ms: u64,
    ) -> LearningResult<bool> {
        // 旧布尔宿主只有新增确认能力，不能把定向更正误解成另建一条偏好。
        Ok(matches!(
            self.decide(candidate, batch, memory, now_ms)?.action,
            LearningDecisionAction::Confirm
        ))
    }

    fn decide(
        &self,
        candidate: &PreferenceCandidate,
        batch: &LearningBatch,
        memory: &MemorySnapshot,
        now_ms: u64,
    ) -> LearningResult<LearningDecision> {
        crate::decision::evidence_decision(candidate, batch, memory, now_ms, self.version())
    }
}
