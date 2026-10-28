//! 上下文装载的候选契约。候选必须先由宿主按scope授权，选择器只建议相关性。
use crate::{MemoryError, MemoryResult};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, future::Future, pin::Pin};

pub const MAX_CONTEXT_CANDIDATES: usize = 24;
pub const MAX_CONTEXT_CANDIDATE_BYTES: usize = 2048;
pub const MAX_CONTEXT_QUERY_BYTES: usize = 4096;
pub const MAX_CONTEXT_SELECTED: usize = 8;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCandidate {
    /// 宿主生成的本批不透明索引，不含用户或会话标识。
    pub id: String,
    /// 有界材料，只作相关性判断；完整原文通过宿主引用读取。
    pub text: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSelectionInput {
    pub query: String,
    pub candidates: Vec<ContextCandidate>,
}
impl ContextSelectionInput {
    pub fn validate(&self) -> MemoryResult<()> {
        let mut ids = BTreeSet::new();
        if self.query.trim().is_empty()
            || self.query.len() > MAX_CONTEXT_QUERY_BYTES
            || self.candidates.is_empty()
            || self.candidates.len() > MAX_CONTEXT_CANDIDATES
            || self.candidates.iter().any(|c| {
                c.id.is_empty()
                    || c.id.len() > 64
                    || !c.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || !ids.insert(&c.id)
                    || c.text.trim().is_empty()
                    || c.text.len() > MAX_CONTEXT_CANDIDATE_BYTES
            })
        {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
    pub fn validate_selection(&self, selected: &[String]) -> MemoryResult<()> {
        self.validate()?;
        let mut ids = BTreeSet::new();
        if selected.len() > MAX_CONTEXT_SELECTED
            || selected
                .iter()
                .any(|id| !ids.insert(id) || !self.candidates.iter().any(|c| &c.id == id))
        {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}
/// 无工具、无授权能力；失败时宿主使用确定性scope安全选择，禁止扩大候选范围。
pub trait ContextRelevanceSelector: Send + Sync {
    fn select<'a>(
        &'a self,
        input: ContextSelectionInput,
    ) -> Pin<Box<dyn Future<Output = MemoryResult<Vec<String>>> + Send + 'a>>;
}
