use crate::{CognitionResult, ExecutionBudget, Source, Visibility, validate_id, validate_text};
use serde::{Deserialize, Serialize};

/// 持久化的内生驱动候选；来源和权限由受信宿主绑定，模型只能提供描述建议。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
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
        if self.priority > 100
            || self.evidence.len() > 16
            || self.attempts > self.budget.max_attempts
        {
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
        matches!(
            self.status,
            AutonomousDriveStatus::Ready
        ) && self.attempts < self.budget.max_attempts
            && self.cooldown_until_ms.is_none_or(|until| now_ms >= until)
            && self.valid_until_ms.is_none_or(|until| now_ms < until)
    }

    /// 返回固定审核器在当前时刻建议的生命周期状态；不执行模型或工具。
    pub fn reviewed_status(&self, now_ms: u64) -> Option<AutonomousDriveStatus> {
        if now_ms == 0
            || !matches!(
                self.status,
                AutonomousDriveStatus::Candidate
                    | AutonomousDriveStatus::Ready
                    | AutonomousDriveStatus::Waiting
            )
        {
            return None;
        }
        if self.valid_until_ms.is_some_and(|until| now_ms >= until) {
            return Some(AutonomousDriveStatus::Blocked);
        }
        if self.status == AutonomousDriveStatus::Waiting {
            return self
                .cooldown_until_ms
                .filter(|until| now_ms >= *until)
                .map(|_| AutonomousDriveStatus::Ready);
        }
        if self.cooldown_until_ms.is_some_and(|until| now_ms < until) {
            return Some(AutonomousDriveStatus::Waiting);
        }
        Some(AutonomousDriveStatus::Ready)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn drive() -> AutonomousDrive {
        AutonomousDrive {
            id: "drive-1".into(),
            source: Source {
                kind: crate::SourceKind::Environment,
                channel: "test".into(),
                reference: "event-1".into(),
            },
            visibility: Visibility::Internal,
            description: "推进一个可验证维护步骤".into(),
            evidence: vec![],
            priority: 80,
            budget: ExecutionBudget {
                max_model_requests: 1,
                max_tool_calls: 1,
                max_attempts: 2,
                timeout_ms: 1_000,
            },
            stop_condition: "receipt".into(),
            status: AutonomousDriveStatus::Ready,
            attempts: 0,
            cooldown_until_ms: None,
            last_progress_at_ms: None,
            valid_until_ms: Some(10_000),
        }
    }

    #[test]
    fn runnable_drive_respects_status_budget_cooldown_and_expiry() {
        let mut value = drive();
        assert!(value.validate().is_ok());
        assert!(!value.is_runnable(1_000));
        value.status = AutonomousDriveStatus::Ready;
        assert!(value.is_runnable(1_000));
        value.cooldown_until_ms = Some(2_000);
        assert!(!value.is_runnable(1_000));
        assert!(value.is_runnable(2_000));
        value.attempts = value.budget.max_attempts;
        assert!(!value.is_runnable(2_000));
        value.attempts = 0;
        value.status = AutonomousDriveStatus::Blocked;
        assert!(!value.is_runnable(2_000));
    }

    #[test]
    fn review_status_applies_expiry_and_cooldown_deterministically() {
        let mut value = drive();
        value.status = AutonomousDriveStatus::Candidate;
        assert_eq!(
            value.reviewed_status(1_000),
            Some(AutonomousDriveStatus::Ready)
        );
        value.cooldown_until_ms = Some(2_000);
        assert_eq!(
            value.reviewed_status(1_000),
            Some(AutonomousDriveStatus::Waiting)
        );
        value.valid_until_ms = Some(2_000);
        assert_eq!(
            value.reviewed_status(2_000),
            Some(AutonomousDriveStatus::Blocked)
        );
        assert_eq!(value.reviewed_status(0), None);
    }

    #[test]
    fn invalid_drive_rejects_zero_budget_and_empty_description() {
        let mut value = drive();
        value.budget.max_attempts = 0;
        assert!(value.validate().is_err());
        let mut value = drive();
        value.description.clear();
        assert!(value.validate().is_err());
    }
}
