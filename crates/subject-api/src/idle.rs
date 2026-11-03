//! 空闲回顾的模型输出契约；想法、判断和行动提议均不构成授权或已验证事实。
use crate::{OpportunityAssessment, SubjectError, SubjectResult, SubjectScope, validate_id};
use serde::{Deserialize, Serialize};

pub const IDLE_INPUT_PREFIX: &str = "eve.idle.observation.v1\n";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleChoice {
    Wait,
    Reflect,
    Act,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleJudgment {
    pub key: String,
    pub statement: String,
    pub confidence: u8,
    pub evidence: Vec<String>,
    /// 修改已有判断时说明新证据如何改变旧看法；旧版本仍在任务回执中。
    pub revision_reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleAction {
    pub title: String,
    pub goal: String,
    pub assessment: OpportunityAssessment,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleReflection {
    pub choice: IdleChoice,
    pub thought: String,
    pub reason: String,
    pub judgments: Vec<IdleJudgment>,
    pub action: Option<IdleAction>,
    /// 下一次回顾的间隔，三十分钟至一天；不改变原授权截止。
    pub revisit_after_minutes: u32,
}

impl IdleReflection {
    pub fn validate(&self) -> SubjectResult<()> {
        let text = |s: &str, limit| !s.trim().is_empty() && s.len() <= limit;
        if !text(&self.thought, 1024)
            || !text(&self.reason, 768)
            || !(30..=1440).contains(&self.revisit_after_minutes)
            || self.judgments.len() > 4
            || (self.choice == IdleChoice::Act) != self.action.is_some()
            || serde_json::to_vec(self)
                .map_err(|_| SubjectError::InvalidInput)?
                .len()
                > 6000
        {
            return Err(SubjectError::InvalidInput);
        }
        let mut keys = std::collections::BTreeSet::new();
        for judgment in &self.judgments {
            validate_id(&judgment.key)?;
            if !keys.insert(&judgment.key)
                || !text(&judgment.statement, 512)
                || !text(&judgment.revision_reason, 512)
                || judgment.confidence > 100
                || judgment.evidence.is_empty()
                || judgment.evidence.len() > 8
            {
                return Err(SubjectError::InvalidInput);
            }
            for evidence in &judgment.evidence {
                validate_id(evidence)?;
            }
        }
        if let Some(action) = &self.action {
            action.assessment.validate()?;
            if !text(&action.title, 120)
                || !text(&action.goal, 2000)
                || action.assessment.evidence.is_empty()
                || action.assessment.score() <= 0
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleEvidence {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub excerpt: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleObservation {
    pub scope: SubjectScope,
    pub revision: u64,
    pub interests: Vec<String>,
    pub evidence: Vec<IdleEvidence>,
    pub judgments: Vec<IdleJudgment>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleMind {
    pub revision: u64,
    pub last_task: String,
    pub next_at_ms: u64,
    pub thought: String,
    pub reason: String,
    pub judgments: Vec<IdleJudgment>,
}

impl IdleObservation {
    pub fn validate(&self) -> SubjectResult<()> {
        crate::validate_scope(&self.scope)?;
        if self.evidence.len() > 16
            || self.judgments.len() > 6
            || self.interests.is_empty()
            || self.interests.len() > 8
            || self
                .interests
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 120)
        {
            return Err(SubjectError::InvalidInput);
        }
        let mut ids = std::collections::BTreeSet::new();
        for evidence in &self.evidence {
            validate_id(&evidence.id)?;
            if !ids.insert(&evidence.id)
                || evidence.text.len() > 16384
                || !matches!(
                    evidence.kind.as_str(),
                    "focus" | "task_result" | "completed_conversation"
                )
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        validate_judgments(&self.judgments)
    }
}
pub fn validate_judgments(judgments: &[IdleJudgment]) -> SubjectResult<()> {
    let mut keys = std::collections::BTreeSet::new();
    for judgment in judgments {
        validate_id(&judgment.key)?;
        if !keys.insert(&judgment.key)
            || judgment.statement.trim().is_empty()
            || judgment.statement.len() > 512
            || judgment.revision_reason.trim().is_empty()
            || judgment.revision_reason.len() > 512
            || judgment.confidence > 100
            || judgment.evidence.is_empty()
            || judgment.evidence.len() > 8
        {
            return Err(SubjectError::InvalidInput);
        }
        for id in &judgment.evidence {
            validate_id(id)?;
        }
    }
    Ok(())
}
