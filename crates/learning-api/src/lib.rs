//! 有来源的偏好候选；候选不等于用户确认；宿主显式启用自主模式后，可由受限策略确认。
use eve_memory_api::{InteractionEvidence, MemoryError, MemoryScope, MemorySnapshot};
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, pin::Pin};

mod decisions;
pub use decisions::{
    DecisionReason, LearningDecision, LearningDecisionAction, LearningDecisionRecord,
};

pub const LEARNING_PLUGIN_ID: &str = "eve.learning";
pub const LEARNING_STATE_KEY: &str = "learning.v1";
pub const MAX_JOBS: usize = 128;
pub const MAX_BATCH_EVIDENCE: usize = 8;
pub const MAX_EVIDENCE_BYTES: usize = 8192;
pub const MAX_INPUT_BYTES: usize = 32768;
pub const MAX_OUTPUT_BYTES: usize = 8192;
pub const MAX_CANDIDATES: usize = 3;
pub const MAX_CANDIDATE_BYTES: usize = 1024;
/// 决策历史不自动淘汰；达到容量后须保留已有记录并拒绝新增。
pub const MAX_DECISIONS: usize = 1024;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
pub const CANDIDATE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub type LearningResult<T> = Result<T, LearningError>;
pub type LearningFuture<'a, T> = Pin<Box<dyn Future<Output = LearningResult<T>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct LearningOptions {
    pub min_new_evidence: usize,
    pub max_batch_evidence: usize,
    pub cooldown_ms: u64,
    pub max_executions: u16,
}
impl Default for LearningOptions {
    fn default() -> Self {
        Self {
            min_new_evidence: 3,
            max_batch_evidence: 8,
            cooldown_ms: 300_000,
            max_executions: 4,
        }
    }
}
impl LearningOptions {
    pub fn validate(&self) -> LearningResult<()> {
        if self.min_new_evidence == 0
            || self.min_new_evidence > self.max_batch_evidence
            || self.max_batch_evidence > MAX_BATCH_EVIDENCE
            || self.cooldown_ms > 86_400_000
            || !(1..=8).contains(&self.max_executions)
        {
            return Err(LearningError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningBatch {
    pub id: String,
    pub scope: MemoryScope,
    pub extractor_version: String,
    pub started_at_ms: u64,
    /// 只允许真实 Memory 快照中已确认送达的 CompletedInteraction；保存确切输入。
    pub evidence: Vec<InteractionEvidence>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDraft {
    pub text: String,
    /// 模型自评，0..=100；不是经过校准的事实概率。
    pub confidence: u8,
    pub evidence_ids: Vec<String>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceCandidate {
    pub id: String,
    pub batch_id: String,
    pub draft: CandidateDraft,
    pub created_at_ms: u64,
    /// 只限制首次确认；已确认偏好继续由用户修正或撤销。
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LearningFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum JobStatus {
    Running,
    Completed,
    Failed(LearningFailure),
    Interrupted,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningJob {
    pub batch: LearningBatch,
    pub status: JobStatus,
    pub finished_at_ms: Option<u64>,
    pub candidates: Vec<PreferenceCandidate>,
}
#[derive(Clone, Eq, PartialEq)]
pub struct LearningSnapshot {
    pub scope: MemoryScope,
    pub jobs: Vec<LearningJob>,
}
pub enum LearningOutcome {
    Completed(Vec<CandidateDraft>),
    Failed(LearningFailure),
}

/// 仅可信宿主持有。不发布跨作用域管理能力给模型或不受信插件。
pub trait LearningAdmin: Send + Sync {
    fn snapshot(&self, scope: &MemoryScope) -> LearningResult<LearningSnapshot>;
    /// 只返回当前可信宿主绑定范围的决策，按范围内连续 sequence 排序。
    /// 决策记录是提交前的意图，不证明偏好已经写入记忆。
    fn decisions(&self, _scope: &MemoryScope) -> LearningResult<Vec<LearningDecisionRecord>> {
        Err(LearningError::Unavailable)
    }
    /// 原子追加决策；宿主之后仍须重新校验候选、来源和当前记忆，并用记忆 CAS 提交。
    /// 同一范围内该候选的最新记录与本次 same_outcome 时复用原记录，零写入；重放时间和记忆修订
    /// 变化不产生新记录。同一候选的有效策略、证据或动作变化可以追加新决策。
    /// 存储失败不能静默丢弃历史，也不能把意图标记成记忆提交成功。
    fn record_decision(
        &self,
        _scope: &MemoryScope,
        _decision: LearningDecision,
        _at_ms: u64,
    ) -> LearningResult<LearningDecisionRecord> {
        Err(LearningError::Unavailable)
    }
    /// 原子保存 Running 与确切消费证据，成功返回后才可进行一次模型调用。
    /// 仅新增 CompletedInteraction 触发；失败、空候选、重启都不再消费同一证据。
    fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>>;
    fn finish(
        &self,
        batch: &LearningBatch,
        at_ms: u64,
        outcome: LearningOutcome,
    ) -> LearningResult<()>;
}
/// 可替换提炼器；实现至多一次模型请求、零工具，不读取批次之外的记忆。
pub trait PreferenceExtractor: Send + Sync {
    fn version(&self) -> &str;
    fn extract(&self, batch: LearningBatch) -> LearningFuture<'_, Vec<CandidateDraft>>;
}
/// 用户已在宿主启用自主学习时使用的可替换自动确认策略。
/// 纯判断，不持有写能力；候选、批次和当前记忆均由可信宿主绑定到同一范围。
/// 返回允许不代表提交成功，也不授予工具、身份合并或自改代码能力。
pub trait AutoConfirmationPolicy: Send + Sync {
    /// 持久记录决策来源；自定义策略修改判断规则时应使用新的稳定版本。
    fn version(&self) -> &str {
        "legacy-auto-confirm-v1"
    }
    fn allows(
        &self,
        candidate: &PreferenceCandidate,
        batch: &LearningBatch,
        memory: &MemorySnapshot,
        now_ms: u64,
    ) -> LearningResult<bool>;
    /// 兼容原有布尔策略，默认只产生确认或暂缓；宿主负责再次约束和提交。
    fn decide(
        &self,
        candidate: &PreferenceCandidate,
        batch: &LearningBatch,
        memory: &MemorySnapshot,
        now_ms: u64,
    ) -> LearningResult<LearningDecision> {
        batch
            .scope
            .validate()
            .map_err(|_| LearningError::InvalidInput)?;
        if candidate.batch_id != batch.id || memory.scope != batch.scope {
            return Err(LearningError::InvalidInput);
        }
        let allowed = self.allows(candidate, batch, memory, now_ms)?;
        let decision = LearningDecision {
            candidate_id: candidate.id.clone(),
            batch_id: batch.id.clone(),
            policy_version: self.version().to_owned(),
            memory_revision: memory.revision,
            evidence_ids: candidate.draft.evidence_ids.clone(),
            action: if allowed {
                LearningDecisionAction::Confirm
            } else {
                LearningDecisionAction::Defer
            },
            reason: if allowed {
                DecisionReason::Eligible
            } else {
                DecisionReason::PolicyDenied
            },
        };
        decision.validate()?;
        Ok(decision)
    }
}
/// 固定跨插件关联键；实际确认来源保存真实用户命令，或自主模式下所引用的完成交互。
pub fn preference_id(candidate_id: &str) -> String {
    format!("learned-{candidate_id}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LearningError {
    InvalidInput,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Memory(MemoryError),
    Extraction(LearningFailure),
}
impl From<MemoryError> for LearningError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}
impl fmt::Display for LearningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "偏好提炼输入无效",
            Self::Unavailable => "偏好提炼服务不可用",
            Self::CorruptState => "偏好提炼状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该偏好提炼状态版本",
            Self::Storage => "偏好提炼提交无法确认；须重新打开核对",
            Self::LimitReached => "偏好提炼容量已满；保留原记录",
            Self::Conflict => "偏好提炼批次冲突",
            Self::Memory(_) => "偏好提炼来源不可用",
            Self::Extraction(_) => "偏好提炼请求失败",
        })
    }
}
impl std::error::Error for LearningError {}
macro_rules! redacted { ($($ty:ty),+) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(
    LearningBatch,
    CandidateDraft,
    PreferenceCandidate,
    LearningJob,
    LearningSnapshot,
    LearningOutcome
);
