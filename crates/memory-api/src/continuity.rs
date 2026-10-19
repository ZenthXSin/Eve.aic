//! 跨会话只读个人记忆：授权由可信宿主决定，每个片段保留原始作用域。
use crate::{
    MAX_PREFERENCE_BYTES, MemoryError, MemoryRecallHit, MemoryRecallRequest, MemoryRecallResponse,
    MemoryResult, MemoryScope, validate_id, validate_text,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_CONTINUITY_SOURCES: usize = 8;
pub const MAX_CONTINUITY_PREFERENCES: usize = 8;
pub const MAX_CONTINUITY_HITS: usize = 3;
pub const MAX_CONTINUITY_BYTES: usize = 8192;

/// 不授予跨会话写入、身份合并或工具权限。私有经历与偏好可分别授权。
#[derive(Clone)]
pub struct MemoryScopeGrant {
    pub scope: MemoryScope,
    pub interactions: bool,
}
/// 仅宿主持有；候选来自实际记忆账本，不从模型参数或昵称选择身份。
pub trait MemoryContinuityPolicy: Send + Sync {
    fn grants(
        &self,
        target: &MemoryScope,
        candidates: &[MemoryScope],
    ) -> MemoryResult<Vec<MemoryScopeGrant>>;
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuityPreference {
    pub id: String,
    pub revision: u64,
    pub text: String,
    pub evidence_id: String,
    pub at_ms: u64,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuitySource {
    pub scope: MemoryScope,
    pub revision: u64,
    pub preferences: Vec<ContinuityPreference>,
    pub hits: Vec<MemoryRecallHit>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryContinuityView {
    pub target: MemoryScope,
    pub sources: Vec<ContinuitySource>,
    pub truncated: bool,
}
impl MemoryContinuityView {
    pub fn validate_for(&self, target: &MemoryScope, query: &str) -> MemoryResult<()> {
        target.validate()?;
        if query.len() > crate::MAX_RECALL_QUERY_BYTES || query.chars().any(char::is_control) {
            return Err(MemoryError::InvalidInput);
        }
        if &self.target != target || self.sources.len() > MAX_CONTINUITY_SOURCES {
            return Err(MemoryError::InvalidInput);
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut preferences = 0;
        let mut hits = 0;
        for source in &self.sources {
            source.scope.validate()?;
            if &source.scope == target || source.revision == 0 || !seen.insert(source.scope.clone())
            {
                return Err(MemoryError::InvalidInput);
            }
            let mut ids = std::collections::BTreeSet::new();
            for preference in &source.preferences {
                validate_id(&preference.id)?;
                validate_id(&preference.evidence_id)?;
                validate_text(&preference.text, MAX_PREFERENCE_BYTES)?;
                if preference.revision == 0
                    || preference.revision > source.revision
                    || preference.at_ms == 0
                    || !ids.insert(&preference.id)
                {
                    return Err(MemoryError::InvalidInput);
                }
            }
            if query.trim().is_empty() && !source.hits.is_empty() {
                return Err(MemoryError::InvalidInput);
            }
            if !source.hits.is_empty() {
                MemoryRecallResponse {
                    scope: source.scope.clone(),
                    revision: source.revision,
                    hits: source.hits.clone(),
                }
                .validate_for(
                    &source.scope,
                    &MemoryRecallRequest {
                        query: query.into(),
                        limit: MAX_CONTINUITY_HITS,
                    },
                )?;
            }
            preferences += source.preferences.len();
            hits += source.hits.len();
        }
        if preferences > MAX_CONTINUITY_PREFERENCES
            || hits > MAX_CONTINUITY_HITS
            || serde_json::to_vec(self)
                .map_err(|_| MemoryError::InvalidInput)?
                .len()
                > MAX_CONTINUITY_BYTES
        {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}
/// 只绑定目标作用域；每次读取重新检查宿主授权、更正与撤销。
pub trait MemoryContinuityReader: Send + Sync {
    /// 空查询只读取有效偏好；非空查询另查有来源的经历。只做本地只读操作。
    fn read(&self, query: &str) -> MemoryResult<MemoryContinuityView>;
}
pub trait MemoryContinuityFactory: Send + Sync {
    fn reader(&self, target: MemoryScope) -> MemoryResult<Arc<dyn MemoryContinuityReader>>;
}
