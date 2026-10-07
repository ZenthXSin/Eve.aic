//! 自主偏好学习的可审计意图；不代表确认、更正或记忆写入已完成。
use crate::{LearningError, LearningResult, MAX_BATCH_EVIDENCE};
use eve_memory_api::validate_id;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt};

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LearningDecisionAction {
    Confirm,
    /// 仅针对当前已确认偏好的指定修订；宿主还须检查来源屏障并执行记忆 CAS。
    Update {
        preference_id: String,
        expected_revision: u64,
    },
    Defer,
    Reject,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DecisionReason {
    Eligible,
    EvidenceThreshold,
    Expired,
    PolicyDenied,
    Duplicate,
    RevokedConflict,
    ManualConflict,
    AmbiguousConflict,
    StaleEvidence,
    ExplicitRevisionUpdate,
    AlreadyLinked,
}

/// 绑定确切候选、策略版本和当时记忆修订的判断，不携带候选正文。
/// 来源 ID 只是可核对引用；必须由宿主核对真实批次、当前记忆与作用域。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningDecision {
    pub candidate_id: String,
    pub batch_id: String,
    pub policy_version: String,
    pub memory_revision: u64,
    pub evidence_ids: Vec<String>,
    pub action: LearningDecisionAction,
    pub reason: DecisionReason,
}

impl LearningDecision {
    /// 只校验形状与基本动作一致性；不授予任何记忆写能力。
    pub fn validate(&self) -> LearningResult<()> {
        for id in [&self.candidate_id, &self.batch_id, &self.policy_version] {
            validate_id(id).map_err(|_| LearningError::InvalidInput)?;
        }
        if self.memory_revision == 0
            || self.evidence_ids.is_empty()
            || self.evidence_ids.len() > MAX_BATCH_EVIDENCE
        {
            return Err(LearningError::InvalidInput);
        }
        let mut evidence_ids = BTreeSet::new();
        for id in &self.evidence_ids {
            validate_id(id).map_err(|_| LearningError::InvalidInput)?;
            if !evidence_ids.insert(id) {
                return Err(LearningError::InvalidInput);
            }
        }
        match &self.action {
            LearningDecisionAction::Confirm if self.reason == DecisionReason::Eligible => {}
            LearningDecisionAction::Update {
                preference_id,
                expected_revision,
            } if self.reason == DecisionReason::ExplicitRevisionUpdate => {
                validate_id(preference_id).map_err(|_| LearningError::InvalidInput)?;
                if *expected_revision == 0 {
                    return Err(LearningError::InvalidInput);
                }
            }
            LearningDecisionAction::Defer | LearningDecisionAction::Reject
                if !matches!(
                    self.reason,
                    DecisionReason::Eligible | DecisionReason::ExplicitRevisionUpdate
                ) => {}
            _ => return Err(LearningError::InvalidInput),
        }
        Ok(())
    }

    /// 幂等键比较所有决策字段，但不包含读取时的记忆修订。
    /// 连续重放同一判断时复用该候选最新记录，原 record 的时间和 memory_revision 保持不变。
    pub fn same_outcome(&self, other: &Self) -> bool {
        self.candidate_id == other.candidate_id
            && self.batch_id == other.batch_id
            && self.policy_version == other.policy_version
            && self.evidence_ids == other.evidence_ids
            && self.action == other.action
            && self.reason == other.reason
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningDecisionRecord {
    /// 从 1 开始，在单个作用域内连续递增。
    pub sequence: u64,
    pub at_ms: u64,
    pub decision: LearningDecision,
}

macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
redacted_debug!(
    LearningDecisionAction,
    LearningDecision,
    LearningDecisionRecord
);
