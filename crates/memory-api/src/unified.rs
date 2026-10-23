//! 可信宿主专用的跨通道定向授权与独立召回保留集；不向模型开放管理能力。
use crate::*;
use serde::{Deserialize, Serialize};

pub const UNIFIED_MEMORY_PLUGIN_ID: &str = "eve.memory-unified";
pub const MAX_UNIFIED_RECORDS: usize = 256;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnifiedMemoryBinding {
    pub id: String,
    pub revision: u64,
    /// 由宿主认证系统提供，不能来自昵称或模型推断。
    pub principal: String,
    pub source: MemoryScope,
    pub target: MemoryScope,
    /// 仅授权偏好时为 false；不隐含反向或传递授权。
    pub interactions: bool,
    pub revoked: bool,
    pub authorization_reference: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallHoldoutSource {
    pub scope: MemoryScope,
    pub revision: u64,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallExpectedEvidence {
    pub scope: MemoryScope,
    pub evidence_id: String,
}

/// 必须先固定保留集，再评估版本；管理员拥有正文，学习器不得读取这些题目。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallHoldout {
    pub id: String,
    pub query: String,
    pub target: MemoryScope,
    /// 固定全部候选来源和语料修订，不只固定正确答案所在来源。
    pub sources: Vec<RecallHoldoutSource>,
    pub expected: Vec<RecallExpectedEvidence>,
    pub binding_revision: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum RecallEvaluationStatus {
    Measured,
    Inconclusive,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallEvaluation {
    pub holdout_id: String,
    pub implementation_version: String,
    pub status: RecallEvaluationStatus,
    pub relevant: usize,
    pub retrieved: usize,
    pub expected: usize,
    /// 整数万分率；无相关答案时精确空结果为 10000。
    pub precision_bps: u32,
    pub recall_bps: u32,
    pub reason: String,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnifiedMemorySnapshot {
    pub revision: u64,
    pub binding_revision: u64,
    pub bindings: Vec<UnifiedMemoryBinding>,
    pub holdouts: Vec<RecallHoldout>,
    pub evaluations: Vec<RecallEvaluation>,
}

/// 宿主必须先完成身份核验和来源用户对目标场景的授权；本能力不接受模型调用。
pub trait UnifiedMemoryAdmin: MemoryContinuityPolicy {
    fn snapshot(&self) -> MemoryResult<UnifiedMemorySnapshot>;
    fn bind(&self, expected_revision: u64, binding: UnifiedMemoryBinding) -> MemoryResult<()>;
    fn revoke(&self, expected_revision: u64, binding_id: &str) -> MemoryResult<()>;
    fn register_holdout(&self, expected_revision: u64, holdout: RecallHoldout) -> MemoryResult<()>;
    /// version 必须包含模型/提示词/索引/排序器的实际指纹。学习输入用于精确污染排除，
    /// 不能证明语义无重叠；该接口不向学习器返回保留集题目。
    fn evaluate(
        &self,
        holdout_id: &str,
        implementation_version: &str,
        training_queries: &[String],
        memory: &dyn MemoryAdmin,
        recall: &dyn MemoryContinuityFactory,
    ) -> MemoryResult<RecallEvaluation>;
}
