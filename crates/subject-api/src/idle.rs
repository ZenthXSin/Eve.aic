//! 空闲回顾的模型输出契约；想法、判断和行动提议均不构成授权或已验证事实。
use crate::{
    MAX_SELF_INTERESTS, OpportunityAssessment, SelfInterest, SelfInterestRecord, SubjectError,
    SubjectResult, SubjectScope, validate_id, validate_self_interests,
};
use serde::{Deserialize, Serialize};

pub const IDLE_INPUT_PREFIX: &str = "eve.idle.observation.v1\n";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleChoice {
    Wait,
    Reflect,
    Act,
    Explore,
}

/// 好奇的可选线索，不是目标、验收条件或执行授权。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleExploration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interest_key: Option<String>,
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
    /// 若由自己的兴趣驱动，关联当前兴趣键；不产生执行授权。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interest_key: Option<String>,
    /// 候选的冻结验收数据；宿主必须按任务契约严格校验，不构成权限或执行授权。
    /// 旧回顾缺少此字段仍可恢复，但须先规划并保存合同，不能直接执行无合同目标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_contract: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleReflection {
    pub choice: IdleChoice,
    pub thought: String,
    pub reason: String,
    pub judgments: Vec<IdleJudgment>,
    /// 旧回顾没有兴趣更新时保持原兴趣；空列表不表示遗忘。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interest_updates: Vec<SelfInterest>,
    pub action: Option<IdleAction>,
    /// Explore 本身已足够；可以省略整个对象，也可以没有线索或既有兴趣。
    /// 缺省字段不写回旧记录，保持旧回顾的持久指纹不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<IdleExploration>,
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
            || self.interest_updates.len() > 2
            || (self.choice == IdleChoice::Act) != self.action.is_some()
            || (self.choice != IdleChoice::Explore && self.exploration.is_some())
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
        let mut interest_keys = std::collections::BTreeSet::new();
        let mut interest_topics = std::collections::BTreeSet::new();
        for interest in &self.interest_updates {
            interest.validate()?;
            if !interest_keys.insert(&interest.key)
                || !interest_topics.insert(crate::self_interest_topic_key(&interest.topic))
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        if let Some(action) = &self.action {
            action.assessment.validate()?;
            if let Some(key) = &action.interest_key {
                validate_id(key)?;
            }
            if !text(&action.title, 120)
                || !text(&action.goal, 2000)
                || action.assessment.evidence.is_empty()
                || action.assessment.score() <= 0
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        if let Some(exploration) = &self.exploration {
            if exploration.cue.as_ref().is_some_and(|cue| cue.len() > 1024) {
                return Err(SubjectError::InvalidInput);
            }
            if let Some(key) = &exploration.interest_key {
                validate_id(key)?;
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
    /// 授权配置中的探索方向，不等于 Eve 已经形成的喜好。
    pub interests: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub self_interests: Vec<SelfInterestRecord>,
    /// 当前冻结材料中，哪些来源已被某项兴趣消费；完整去重历史由存储维护。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub interest_consumed: std::collections::BTreeMap<String, Vec<String>>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub self_interests: Vec<SelfInterestRecord>,
    /// 历史已经保存在独立账本；缺失该账本时不能以当前视图重建历史。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
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
                    "focus"
                        | "task_result"
                        | "completed_conversation"
                        | "self_reflection"
                        | "exploration_experience"
                        | "exploration_reflection"
                )
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        validate_judgments(&self.judgments)?;
        if self.interest_consumed.len() > MAX_SELF_INTERESTS {
            return Err(SubjectError::InvalidInput);
        }
        for (key, evidence) in &self.interest_consumed {
            if !self.self_interests.iter().any(|r| &r.current.key == key)
                || evidence.len() > 16
                || evidence
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != evidence.len()
                || evidence.iter().any(|id| !ids.contains(id))
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        validate_self_interests(&self.self_interests)
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
