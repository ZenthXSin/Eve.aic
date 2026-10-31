//! 有来源的语义情景摘要：模型推断不等于事实、稳定偏好或执行授权。
use crate::{MemoryResult, MemoryScope, MemorySnapshot};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

pub const SUMMARY_PLUGIN_ID: &str = "eve.memory-summary";
pub const MAX_SUMMARY_JOBS: usize = 128;
pub const MAX_SUMMARY_POINTS: usize = 6;
pub const MAX_SUMMARY_INPUT_BYTES: usize = 65_536;
pub const MAX_SUMMARY_OUTPUT_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug)]
pub struct SummaryOptions {
    pub min_new_evidence: usize,
    pub max_new_evidence: usize,
    pub cooldown_ms: u64,
}
impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            min_new_evidence: 4,
            max_new_evidence: 8,
            cooldown_ms: 300_000,
        }
    }
}
impl SummaryOptions {
    pub fn validate(self) -> MemoryResult<()> {
        if self.min_new_evidence == 0
            || self.min_new_evidence > self.max_new_evidence
            || self.max_new_evidence > 8
            || self.cooldown_ms > 86_400_000
        {
            return Err(crate::MemoryError::InvalidInput);
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarySource {
    pub evidence_id: String,
    pub revision: u64,
    pub at_ms: u64,
    /// 只包含用户原话；旧助手输出不进入语义归纳输入。
    pub user_text: String,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryQuote {
    pub evidence_id: String,
    pub text: String,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryPoint {
    pub text: String,
    pub quotes: Vec<SummaryQuote>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryBatch {
    pub sequence: u64,
    pub scope: MemoryScope,
    pub source_revision: u64,
    pub started_at_ms: u64,
    pub extractor_version: String,
    /// 本批实际消费的新证据；失败与中断后也不自动重放。
    pub consumed_ids: Vec<String>,
    /// 本批新证据与上次摘要引用的原文，均来自本作用域不可变交互。
    pub sources: Vec<SummarySource>,
    pub previous: Vec<SummaryPoint>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SummaryFailure {
    Provider,
    Timeout,
    InvalidOutput,
    Cancelled,
    InputTooLarge,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SummaryStatus {
    Running,
    Completed,
    Failed(SummaryFailure),
    Interrupted,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryJob {
    pub batch: SummaryBatch,
    pub status: SummaryStatus,
    pub finished_at_ms: Option<u64>,
    /// 始终是待核对的模型归纳，不确认或更新 Memory.preferences。
    pub points: Vec<SummaryPoint>,
}
pub type SummaryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SummaryPoint>, SummaryFailure>> + Send + 'a>>;
pub trait ConversationSummarizer: Send + Sync {
    fn version(&self) -> &str;
    fn summarize(&self, batch: SummaryBatch) -> SummaryFuture<'_>;
}
/// 仅可信宿主持有；不会向模型发布跨用户的账本管理接口。
pub trait SummaryAdmin: Send + Sync {
    /// 仅返回全局网络失败的时间元数据，供可信宿主在其他作用域准入前冷却。
    fn last_provider_failure_at(&self) -> MemoryResult<Option<u64>>;
    fn jobs(&self, scope: &MemoryScope) -> MemoryResult<Vec<SummaryJob>>;
    fn reserve(
        &self,
        snapshot: &MemorySnapshot,
        now_ms: u64,
        version: &str,
        options: SummaryOptions,
    ) -> MemoryResult<Option<SummaryBatch>>;
    /// 读取固定版本的完整证据流，内置实现只保留本批来源及仍有效的旧观点来源。
    /// 兼容实现默认读取完整快照；宿主须先绑定目标作用域。
    fn reserve_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn crate::MemoryService,
        now_ms: u64,
        version: &str,
        options: SummaryOptions,
    ) -> MemoryResult<Option<SummaryBatch>> {
        let snapshot = reader.complete_snapshot()?;
        if snapshot.scope != *scope {
            return Err(crate::MemoryError::Conflict);
        }
        self.reserve(&snapshot, now_ms, version, options)
    }
    fn finish(
        &self,
        batch: &SummaryBatch,
        now_ms: u64,
        result: Result<Vec<SummaryPoint>, SummaryFailure>,
    ) -> MemoryResult<()>;
}
