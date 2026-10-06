//! 只读记忆召回契约：由可信宿主绑定作用域，保留命中内容的原始来源。

use crate::{MemoryError, MemoryResult, MemoryScope, validate_id, validate_text};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

pub const MAX_RECALL_QUERY_BYTES: usize = 1024;
pub const MAX_RECALL_RESULTS: usize = 8;
pub const DEFAULT_RECALL_RESULTS: usize = 5;
pub const MAX_RECALL_EXCERPT_BYTES: usize = 512;
pub const MAX_RECALL_RESPONSE_BYTES: usize = 16_384;

/// 查询只选择已经绑定的记忆，不授权写入、跨作用域读取或外部搜索。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecallRequest {
    pub query: String,
    pub limit: usize,
}

impl MemoryRecallRequest {
    pub fn validate(&self) -> MemoryResult<()> {
        validate_text(&self.query, MAX_RECALL_QUERY_BYTES)?;
        if self.query.chars().any(char::is_control)
            || self.limit == 0
            || self.limit > MAX_RECALL_RESULTS
        {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}

/// 已完成交互中被召回的字段；助手的旧回复不等同于已确认偏好。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecallField {
    User,
    Assistant,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum MemoryRecallSource {
    /// 只允许当前仍有效的已确认偏好；确认权限由既有宿主策略负责。
    /// 版本指向该偏好及其当前证据，
    /// `at_ms` 是该偏好版本的记录时间。
    ConfirmedPreference {
        preference_id: String,
        preference_revision: u64,
        evidence_id: String,
        evidence_revision: u64,
        at_ms: u64,
    },
    /// 指向实际完成并已持久保存的交互；不提升旧正文的可信程度。
    CompletedInteraction {
        evidence_id: String,
        evidence_revision: u64,
        message_id: String,
        session_revision: u64,
        turn_id: u64,
        at_ms: u64,
        field: RecallField,
    },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecallHit {
    /// 实现定义的正整数排序分数，不是概率或事实置信度。
    pub score: u32,
    pub source: MemoryRecallSource,
    pub excerpt: String,
    pub excerpt_truncated: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecallResponse {
    pub scope: MemoryScope,
    /// 本次只读快照的修订；召回不能生成新的持久修订。
    pub revision: u64,
    pub hits: Vec<MemoryRecallHit>,
}

impl MemoryRecallResponse {
    /// 组合层在使用可替换实现的结果前检查身份、来源与传输边界。
    /// 此检查不能代替实现对持久快照及真实来源关联的完整校验。
    pub fn validate_for(
        &self,
        scope: &MemoryScope,
        request: &MemoryRecallRequest,
    ) -> MemoryResult<()> {
        scope.validate()?;
        request.validate()?;
        if &self.scope != scope || self.hits.len() > request.limit {
            return Err(MemoryError::InvalidInput);
        }

        let mut preference_ids = BTreeSet::new();
        let mut interaction_fields = BTreeSet::new();
        let mut evidence_revisions = BTreeMap::new();
        let mut interaction_origins = BTreeMap::new();
        let mut message_origins = BTreeMap::new();
        let mut turn_origins = BTreeMap::new();
        for hit in &self.hits {
            if hit.score == 0 {
                return Err(MemoryError::InvalidInput);
            }
            validate_text(&hit.excerpt, MAX_RECALL_EXCERPT_BYTES)?;
            let (evidence_id, evidence_revision) = match &hit.source {
                MemoryRecallSource::ConfirmedPreference {
                    preference_id,
                    preference_revision,
                    evidence_id,
                    evidence_revision,
                    ..
                } => {
                    validate_id(preference_id)?;
                    validate_revision(*preference_revision, self.revision)?;
                    if !preference_ids.insert(preference_id.as_str()) {
                        return Err(MemoryError::InvalidInput);
                    }
                    (evidence_id, evidence_revision)
                }
                MemoryRecallSource::CompletedInteraction {
                    evidence_id,
                    evidence_revision,
                    message_id,
                    session_revision,
                    turn_id,
                    at_ms,
                    field,
                } => {
                    validate_id(message_id)?;
                    if *turn_id == 0
                        || turn_id
                            .checked_mul(2)
                            .is_none_or(|minimum| *session_revision < minimum)
                        || !interaction_fields.insert((evidence_id.as_str(), *field))
                    {
                        return Err(MemoryError::InvalidInput);
                    }
                    // 同一交互可以分别命中用户与助手字段，但两者必须指向同一来源。
                    let origin = (message_id.as_str(), *session_revision, *turn_id, *at_ms);
                    if interaction_origins
                        .insert(evidence_id.as_str(), origin)
                        .is_some_and(|previous| previous != origin)
                        || message_origins
                            .insert(message_id.as_str(), evidence_id.as_str())
                            .is_some_and(|previous| previous != evidence_id.as_str())
                        || turn_origins
                            .insert(*turn_id, evidence_id.as_str())
                            .is_some_and(|previous| previous != evidence_id.as_str())
                    {
                        return Err(MemoryError::InvalidInput);
                    }
                    (evidence_id, evidence_revision)
                }
            };
            validate_id(evidence_id)?;
            validate_revision(*evidence_revision, self.revision)?;
            if evidence_revisions
                .insert(evidence_id.as_str(), *evidence_revision)
                .is_some_and(|previous| previous != *evidence_revision)
            {
                return Err(MemoryError::InvalidInput);
            }
        }

        // 转义后的 JSON 也受限，避免控制字符或长来源标识放大输出。
        let encoded = serde_json::to_vec(self).map_err(|_| MemoryError::InvalidInput)?;
        if encoded.len() > MAX_RECALL_RESPONSE_BYTES {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}

fn validate_revision(revision: u64, snapshot_revision: u64) -> MemoryResult<()> {
    if revision == 0 || revision > snapshot_revision {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}

/// 已绑定作用域的只读能力；调用方不能在查询时指定其他用户身份。
pub trait MemoryRecallService: Send + Sync {
    fn recall(&self, request: &MemoryRecallRequest) -> MemoryResult<MemoryRecallResponse>;
}

/// 仅由可信宿主持有，不能发布给模型或非可信通道。
pub trait MemoryRecallFactory: Send + Sync {
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryRecallService>>;
}

macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}

redacted_debug!(
    MemoryRecallRequest,
    MemoryRecallSource,
    MemoryRecallHit,
    MemoryRecallResponse
);
