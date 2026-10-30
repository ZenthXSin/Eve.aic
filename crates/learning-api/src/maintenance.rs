//! 记忆维护回执与有来源的反思草稿，不构成授权或已验证事实。
use crate::*;
use serde::{Deserialize, Serialize};

pub const MAINTENANCE_DELAY_MS: u64 = 86_400_000;
pub const REPAIR_COOLDOWN_MS: u64 = 1_800_000;
pub const MAX_REPAIRS: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectionDraft {
    pub observations: String,
    pub actions: String,
    pub outcomes: String,
    pub lessons: String,
    pub next_checks: String,
    pub evidence_ids: Vec<String>,
}
impl ReflectionDraft {
    pub fn validate(&self, batch: &LearningBatch) -> LearningResult<()> {
        for text in [
            &self.observations,
            &self.actions,
            &self.outcomes,
            &self.lessons,
            &self.next_checks,
        ] {
            if text.trim().is_empty() || text.len() > 512 {
                return Err(LearningError::InvalidInput);
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        if self.evidence_ids.is_empty()
            || self.evidence_ids.len() > MAX_BATCH_EVIDENCE
            || self
                .evidence_ids
                .iter()
                .any(|id| !seen.insert(id) || !batch.evidence.iter().any(|e| &e.id == id))
        {
            return Err(LearningError::InvalidInput);
        }
        Ok(())
    }
}

pub struct MaintenanceExtraction {
    pub candidates: Vec<CandidateDraft>,
    pub reflection: Option<ReflectionDraft>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceAttempt {
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: JobStatus,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MaintenanceReceipt {
    pub batch_id: String,
    pub extractor_version: String,
    pub source_revision: u64,
    pub evidence_ids: Vec<String>,
    pub status: JobStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub previous_attempts: Vec<MaintenanceAttempt>,
    pub reflection: Option<ReflectionDraft>,
}
