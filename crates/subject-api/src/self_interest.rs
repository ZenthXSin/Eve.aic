//! Eve 自己形成的兴趣；与用户偏好、授权范围及已完成事实分开保存。
use crate::{SubjectError, SubjectResult, validate_id};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_SELF_INTERESTS: usize = 8;
pub const MAX_SELF_INTEREST_BYTES: usize = 4096;
pub const MAX_SELF_INTEREST_PAGE: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfInterestPhase {
    Curious,
    Active,
    Resting,
    Dropped,
}

impl SelfInterestPhase {
    pub fn motivates_action(&self) -> bool {
        matches!(self, Self::Curious | Self::Active)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfInterest {
    pub key: String,
    pub topic: String,
    pub phase: SelfInterestPhase,
    pub strength: u8,
    /// 对 Eve 自己有什么吸引力，或为何暂缓、放下；属于主观看法。
    pub reason: String,
    /// 想尝试的具体事情，不表示已执行，也不授予权限。
    pub want_to_try: String,
    pub evidence: Vec<String>,
}

impl SelfInterest {
    pub fn validate(&self) -> SubjectResult<()> {
        validate_id(&self.key)?;
        let text = |s: &str, limit| !s.trim().is_empty() && s.len() <= limit;
        if self.key.len() > 64
            || !text(&self.topic, 120)
            || !text(&self.reason, 384)
            || self.want_to_try.len() > 384
            || (self.phase.motivates_action() && self.want_to_try.trim().is_empty())
            || self.strength > 100
            || self.evidence.is_empty()
            || self.evidence.len() > 4
        {
            return Err(SubjectError::InvalidInput);
        }
        let mut ids = BTreeSet::new();
        for id in &self.evidence {
            validate_id(id)?;
            if !ids.insert(id) {
                return Err(SubjectError::InvalidInput);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfInterestRecord {
    pub current: SelfInterest,
    /// 时间与修订由宿主生成，不能由模型指定。
    pub revision: u64,
    pub first_recorded_at_ms: u64,
    pub updated_at_ms: u64,
    /// 当前视图携带的已消费来源；独立存储还会核对完整历史，不能仅凭此列表重放。
    pub consumed_evidence: Vec<String>,
}

pub fn self_interest_topic_key(topic: &str) -> String {
    topic
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn validate_self_interests(interests: &[SelfInterestRecord]) -> SubjectResult<()> {
    if interests.len() > MAX_SELF_INTERESTS
        || serde_json::to_vec(interests)
            .map_err(|_| SubjectError::InvalidInput)?
            .len()
            > MAX_SELF_INTEREST_BYTES
    {
        return Err(SubjectError::LimitReached);
    }
    let mut keys = BTreeSet::new();
    let mut topics = BTreeSet::new();
    for record in interests {
        record.current.validate()?;
        let mut evidence = BTreeSet::new();
        for id in &record.consumed_evidence {
            validate_id(id)?;
            if !evidence.insert(id) {
                return Err(SubjectError::InvalidInput);
            }
        }
        if record.revision == 0
            || record.first_recorded_at_ms == 0
            || record.updated_at_ms < record.first_recorded_at_ms
            || !keys.insert(&record.current.key)
            || !topics.insert(self_interest_topic_key(&record.current.topic))
            || record.consumed_evidence.is_empty()
            || record.consumed_evidence.len() > 32
            || record
                .current
                .evidence
                .iter()
                .any(|id| !evidence.contains(id))
        {
            return Err(SubjectError::InvalidInput);
        }
    }
    Ok(())
}

/// 单次读取的有界视图；不是终身兴趣数量或修订次数的上限。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfInterestPage {
    pub revision: u64,
    pub total: u64,
    pub items: Vec<SelfInterestRecord>,
    pub next_after: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfInterestHistoryPage {
    pub items: Vec<SelfInterestRecord>,
    pub next_after: Option<u64>,
}

/// 独立持久化当前认知、兴趣修订和已消费来源。只交给可信宿主。
/// commit 必须原子保存整个回顾；同一 task_id 重送相同输入只返回已存状态，
/// 不改变时间、修订或再回顾期限。来源去重覆盖完整历史，不限于上下文视图。
pub trait IdleMemoryStore: Send + Sync {
    fn snapshot(&self, scope: &crate::SubjectScope) -> SubjectResult<crate::IdleMind>;
    /// 填充已展示兴趣对当前冻结证据的消费标记；不能把完整历史放入模型上下文。
    fn prepare_observation(&self, observation: &mut crate::IdleObservation) -> SubjectResult<()>;
    fn commit(
        &self,
        observation: &crate::IdleObservation,
        result: &crate::IdleReflection,
        task_id: &str,
        now_ms: u64,
    ) -> SubjectResult<crate::IdleMind>;
    fn interests(
        &self,
        scope: &crate::SubjectScope,
        after: Option<&str>,
        limit: usize,
    ) -> SubjectResult<SelfInterestPage>;
    fn history(
        &self,
        scope: &crate::SubjectScope,
        key: &str,
        after: u64,
        limit: usize,
    ) -> SubjectResult<SelfInterestHistoryPage>;
    /// 首次接入时原子导入旧快照；不同的既有状态必须拒绝，不能覆盖。
    fn import_legacy(
        &self,
        scope: &crate::SubjectScope,
        mind: &crate::IdleMind,
    ) -> SubjectResult<()>;
}
