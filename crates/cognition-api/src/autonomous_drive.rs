use crate::{CognitionResult, ExecutionBudget, Source, Visibility, validate_id, validate_text};
use serde::{Deserialize, Serialize};

/// 持久化的内生驱动候选；来源和权限由受信宿主绑定，模型只能提供描述建议。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutonomousDrive {
    pub id: String,
    pub source: Source,
    pub visibility: Visibility,
    pub description: String,
    pub evidence: Vec<Source>,
    pub priority: u8,
    pub budget: ExecutionBudget,
    pub stop_condition: String,
    pub status: AutonomousDriveStatus,
    pub attempts: u16,
    pub cooldown_until_ms: Option<u64>,
    pub last_progress_at_ms: Option<u64>,
    pub valid_until_ms: Option<u64>,
}

impl AutonomousDrive {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.id)?;
        self.source.validate()?;
        self.visibility.validate()?;
        validate_text(&self.description)?;
        validate_text(&self.stop_condition)?;
        self.budget.validate()?;
        if self.priority > 100 || self.evidence.len() > 16 || self.attempts > self.budget.max_attempts {
            return Err(crate::CognitionError::InvalidInput);
        }
        if self.cooldown_until_ms == Some(0)
            || self.last_progress_at_ms == Some(0)
            || self.valid_until_ms == Some(0)
        {
            return Err(crate::CognitionError::InvalidInput);
        }
        for evidence in &self.evidence {
            evidence.validate()?;
        }
        Ok(())
    }

    pub fn is_runnable(&self, now_ms: u64) -> bool {
        matches!(self.status, AutonomousDriveStatus::Candidate | AutonomousDriveStatus::Ready)
            && self.attempts < self.budget.max_attempts
            && self.cooldown_until_ms.is_none_or(|until| now_ms >= until)
            && self.valid_until_ms.is_none_or(|until| now_ms < until)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AutonomousDriveStatus {
    Candidate,
    Ready,
    Active,
    Waiting,
    Blocked,
    Completed,
    Cancelled,
}
