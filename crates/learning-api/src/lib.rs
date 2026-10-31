//! 有来源的偏好候选；候选不等于用户确认；宿主显式启用自主模式后，可由受限策略确认。
use eve_memory_api::{InteractionEvidence, MemoryError, MemoryScope, MemorySnapshot};
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, pin::Pin};

mod archive;
mod decisions;
mod maintenance;
mod scan;
pub use archive::{LearningArchiveStore, MAX_ARCHIVE_RECORD_BYTES, MAX_ARCHIVE_RESTORE_BYTES};
pub use decisions::{
    DecisionReason, LearningDecision, LearningDecisionAction, LearningDecisionRecord,
};
pub use maintenance::*;
pub use scan::{LearningHistoryCursor, MAX_LEARNING_PAGE, SnapshotHistoryCursor};

pub const LEARNING_PLUGIN_ID: &str = "eve.learning";
pub const LEARNING_STATE_KEY: &str = "learning.v1";
pub const MAX_JOBS: usize = 1024;
pub const MAX_BATCH_EVIDENCE: usize = 8;
pub const MAX_EVIDENCE_BYTES: usize = 8192;
pub const MAX_INPUT_BYTES: usize = 32768;
pub const MAX_OUTPUT_BYTES: usize = 8192;
pub const MAX_CANDIDATES: usize = 3;
pub const MAX_CANDIDATE_BYTES: usize = 1024;
/// 决策历史不自动淘汰；达到容量后须保留已有记录并拒绝新增。
pub const MAX_DECISIONS: usize = 8192;
/// 学习批次账本和决策账本各自的持久化字节上限，不是进程 RSS 上限。
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
/// 历史账本读取兼容；新写入遵守 MAX_STATE_BYTES，不能把旧大账本误报为损坏。
pub const MAX_LEGACY_STATE_BYTES: usize = 32 * 1024 * 1024;
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
            max_executions: 15,
        }
    }
}
impl LearningOptions {
    pub fn validate(&self) -> LearningResult<()> {
        if self.min_new_evidence == 0
            || self.min_new_evidence > self.max_batch_evidence
            || self.max_batch_evidence > MAX_BATCH_EVIDENCE
            || self.cooldown_ms > 86_400_000
            || !(1..=15).contains(&self.max_executions)
        {
            return Err(LearningError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningBatch {
    /// 只读模型提炼的尝试代数；旧记录为0，用于拒绝迟到的旧尝试回执。
    #[serde(default)]
    pub attempt: u8,
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
    TransientProvider,
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
    Maintained(MaintenanceExtraction),
    Failed(LearningFailure),
}

/// 仅当前进程中的服务变更标识；不能用于授权、CAS 或跨进程恢复。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LearningChangeToken {
    pub instance: u64,
    pub revision: u64,
}

/// 仅可信宿主持有。不发布跨作用域管理能力给模型或不受信插件。
pub trait LearningAdmin: Send + Sync {
    /// 当前范围的无正文变更通知。None 表示旧实现不支持，宿主不能据此跳过完整扫描。
    /// 标识相同不证明冷文件未损坏，缓存仍须定期完整复核。
    fn change_token(&self, _scope: &MemoryScope) -> LearningResult<Option<LearningChangeToken>> {
        Ok(None)
    }
    fn scan_jobs(
        &self,
        scope: &MemoryScope,
    ) -> LearningResult<Box<dyn LearningHistoryCursor<LearningJob>>> {
        let snapshot = self.snapshot(scope)?;
        if &snapshot.scope != scope {
            return Err(LearningError::Conflict);
        }
        scan::jobs_scope(scope, &snapshot.jobs)?;
        Ok(Box::new(SnapshotHistoryCursor::new(
            scope.clone(),
            snapshot.jobs,
        )?))
    }
    fn scan_decisions(
        &self,
        scope: &MemoryScope,
    ) -> LearningResult<Box<dyn LearningHistoryCursor<LearningDecisionRecord>>> {
        Ok(Box::new(SnapshotHistoryCursor::new(
            scope.clone(),
            self.decisions(scope)?,
        )?))
    }
    /// 是否装配冷历史；宿主不能再用旧热层条数推断其只读视图已损坏或已满。
    fn archived_history(&self) -> LearningResult<bool> {
        Ok(false)
    }
    fn maintenance_receipts(
        &self,
        _scope: &MemoryScope,
    ) -> LearningResult<Vec<MaintenanceReceipt>> {
        Err(LearningError::Unavailable)
    }
    /// 只补做无工具模型提炼；原尝试保留，至多两次，取消/无效输出不补做。
    fn reserve_repair(
        &self,
        _memory: &MemorySnapshot,
        _now_ms: u64,
        _extractor_version: &str,
    ) -> LearningResult<Option<LearningBatch>> {
        Ok(None)
    }
    /// 从固定版本的完整证据流准入只读补做；内置实现不恢复整份原始记忆。
    fn reserve_repair_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn eve_memory_api::MemoryService,
        now_ms: u64,
        extractor_version: &str,
    ) -> LearningResult<Option<LearningBatch>> {
        let snapshot = reader.complete_snapshot()?;
        if snapshot.scope != *scope {
            return Err(LearningError::Conflict);
        }
        self.reserve_repair(&snapshot, now_ms, extractor_version)
    }
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
    /// 仅新增 CompletedInteraction 触发；此入口不重新消费旧证据。只读失败补做使用reserve_repair。
    fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>>;
    /// 从绑定作用域的证据流准入新批次，内置实现保留批次正文及校验元数据。
    fn reserve_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn eve_memory_api::MemoryService,
        now_ms: u64,
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        let snapshot = reader.complete_snapshot()?;
        if snapshot.scope != *scope {
            return Err(LearningError::Conflict);
        }
        self.reserve(&snapshot, now_ms, extractor_version, options)
    }
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
    fn extract_maintenance(
        &self,
        batch: LearningBatch,
    ) -> LearningFuture<'_, MaintenanceExtraction> {
        Box::pin(async move {
            self.extract(batch)
                .await
                .map(|candidates| MaintenanceExtraction {
                    candidates,
                    reflection: None,
                })
        })
    }
}
/// 用户已在宿主启用自主学习时使用的可替换自动确认策略。
/// 纯判断，不持有写能力；候选、批次和当前记忆均由可信宿主绑定到同一范围。
/// 返回允许不代表提交成功，也不授予工具、身份合并或自改代码能力。
pub trait AutoConfirmationPolicy: Send + Sync {
    /// 持久记录决策来源；自定义策略修改判断规则时应使用新的稳定版本。
    fn version(&self) -> &str {
        "legacy-auto-confirm-v1"
    }
    /// 只读判断入口。旧策略保持完整快照语义；原生策略可逐页核验后只保留
    /// 候选批次和偏好历史所需原文。宿主仍独立复核提议并以原修订 CAS 提交。
    fn decide_reader(
        &self,
        candidate: &PreferenceCandidate,
        batch: &LearningBatch,
        reader: &dyn eve_memory_api::MemoryService,
        now_ms: u64,
    ) -> LearningResult<LearningDecision> {
        let memory = reader.complete_snapshot()?;
        self.decide(candidate, batch, &memory, now_ms)
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
