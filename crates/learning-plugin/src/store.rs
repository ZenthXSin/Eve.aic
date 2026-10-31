use crate::{
    archive::{self, Index},
    strict_json,
};
use eve_learning_api::*;
use eve_memory_api::{
    EvidenceSource, InteractionEvidence, MemoryScope, MemorySnapshot, validate_id, validate_text,
};
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt::Write,
    sync::{Arc, Mutex, MutexGuard},
};

#[path = "cold_decisions.rs"]
mod cold_decisions;
#[path = "cold_startup.rs"]
mod cold_startup;

static INSTANCE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const FORMAT_VERSION: u32 = 1;
const DECISIONS_STATE_KEY: &str = "learning.decisions.v1";
const MAX_DECISIONS_STATE_BYTES: usize = MAX_STATE_BYTES;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    jobs: Vec<JobRecord>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobRecord {
    job: LearningJob,
    /// 只保存批次来源的修订，不复制偏好和未消费的原始记忆。
    memory_revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    previous_attempts: Vec<MaintenanceAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reflection: Option<ReflectionDraft>,
}
/// 决策账本单独保存，旧批次文档的格式和大小预算保持不变。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionsDocument {
    format_version: u32,
    records: Vec<ScopedDecisionRecord>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedDecisionRecord {
    scope: MemoryScope,
    record: LearningDecisionRecord,
}
struct Inner {
    change: LearningChangeToken,
    scope_changes: BTreeMap<MemoryScope, u64>,
    document: Document,
    decisions: DecisionsDocument,
    context: Option<PluginContext>,
    archive: Option<Arc<dyn LearningArchiveStore>>,
    job_index: Option<Index>,
    decision_index: Option<Index>,
}
// 兼容公开的全历史操作，但不在两次操作之间保留冷正文。
// 操作期间仍会加载全账本；分页消费属于后续性能改进，不能宣称恒定峰值内存。
fn evict(inner: &mut Inner) {
    if inner.job_index.is_some() {
        inner.document.jobs = Vec::new();
    }
    if inner.decision_index.is_some() {
        inner.decisions.records = Vec::new();
    }
}
struct WorkingGuard<'a>(MutexGuard<'a, Inner>);
impl std::ops::Deref for WorkingGuard<'_> {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
impl std::ops::DerefMut for WorkingGuard<'_> {
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.0
    }
}
impl Drop for WorkingGuard<'_> {
    fn drop(&mut self) {
        evict(&mut self.0);
    }
}
type Decoder<T> = Box<dyn FnMut(serde_json::Value) -> LearningResult<Option<T>> + Send>;
struct ArchiveCursor<T> {
    service: Arc<StoredLearning>,
    scope: MemoryScope,
    store: Option<Arc<dyn LearningArchiveStore>>,
    refs: VecDeque<archive::RecordRef>,
    hot: VecDeque<T>,
    decode: Decoder<T>,
    failed: bool,
}
impl<T> ArchiveCursor<T> {
    fn check(&self) -> LearningResult<()> {
        let inner = self
            .service
            .inner
            .lock()
            .map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        Ok(())
    }
}
impl<T: Send> LearningHistoryCursor<T> for ArchiveCursor<T> {
    fn scope(&self) -> &MemoryScope {
        &self.scope
    }
    fn next_page(&mut self) -> LearningResult<Vec<T>> {
        if self.failed {
            return Err(LearningError::Unavailable);
        }
        let result = (|| {
            self.check()?;
            let mut page = Vec::new();
            while page.len() < MAX_LEARNING_PAGE {
                if let Some(item) = self.hot.pop_front() {
                    page.push(item);
                    continue;
                }
                let Some(reference) = self.refs.pop_front() else {
                    break;
                };
                let value = archive::load_value(
                    self.store.as_deref().ok_or(LearningError::Unavailable)?,
                    &reference,
                )?;
                if let Some(item) = (self.decode)(value)? {
                    page.push(item);
                }
            }
            self.check()?;
            Ok(page)
        })();
        if result.is_err() {
            self.failed = true;
            self.refs.clear();
            self.hot.clear();
            // 不能把损坏页跳过去，或让其他学习操作继续使用已不可信的存储。
            let _ = self.service.close();
        }
        result
    }
}
pub(super) struct StoredLearning {
    inner: Mutex<Inner>,
}
impl StoredLearning {
    pub(super) fn open(
        context: PluginContext,
        archive: Option<Arc<dyn LearningArchiveStore>>,
    ) -> LearningResult<Self> {
        let mut job_index = None;
        let mut decision_index = None;
        let document = match context
            .state_get(LEARNING_STATE_KEY)
            .map_err(|_| LearningError::Storage)?
        {
            None => Document {
                format_version: 1,
                jobs: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(LearningError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| LearningError::CorruptState)?;
                if value
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(2)
                {
                    let index: Index =
                        serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                    if archive.is_none() {
                        return Err(LearningError::UnsupportedVersion);
                    }
                    let jobs = vec![];
                    job_index = Some(index);
                    Document {
                        format_version: 1,
                        jobs,
                    }
                } else {
                    let doc: Document =
                        serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                    if doc.format_version != FORMAT_VERSION {
                        return Err(LearningError::UnsupportedVersion);
                    }
                    doc
                }
            }
        };
        validate_document(&document, job_index.is_some())
            .map_err(|_| LearningError::CorruptState)?;
        let decisions = match context
            .state_get(DECISIONS_STATE_KEY)
            .map_err(|_| LearningError::Storage)?
        {
            None => DecisionsDocument {
                format_version: 1,
                records: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(LearningError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| LearningError::CorruptState)?;
                if value
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(2)
                {
                    let index: Index =
                        serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                    if archive.is_none() {
                        return Err(LearningError::UnsupportedVersion);
                    }
                    let records = vec![];
                    decision_index = Some(index);
                    DecisionsDocument {
                        format_version: 1,
                        records,
                    }
                } else {
                    let doc: DecisionsDocument =
                        serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                    if doc.format_version != FORMAT_VERSION {
                        return Err(LearningError::UnsupportedVersion);
                    }
                    doc
                }
            }
        };
        if decision_index.is_none() && decisions.records.len() > MAX_DECISIONS {
            return Err(LearningError::CorruptState);
        }
        if job_index.is_none() && decision_index.is_none() {
            validate_decisions_document(&decisions, &document, false)
                .map_err(|_| LearningError::CorruptState)?;
        }
        let mut inner = Inner {
            change: LearningChangeToken {
                instance: INSTANCE_SEQUENCE
                    .fetch_update(
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                        |n| n.checked_add(1),
                    )
                    .map_err(|_| LearningError::LimitReached)?,
                revision: 0,
            },
            scope_changes: BTreeMap::new(),
            document,
            decisions,
            context: Some(context),
            archive,
            job_index,
            decision_index,
        };
        if inner.job_index.is_some() || inner.decision_index.is_some() {
            cold_startup::validate_and_recover(&mut inner)?;
            evict(&mut inner);
            return Ok(Self {
                inner: Mutex::new(inner),
            });
        }
        let mut next = inner.document.clone();
        let mut interrupted = false;
        for record in &mut next.jobs {
            if record.job.status == JobStatus::Running {
                record.job.status = JobStatus::Interrupted;
                interrupted = true;
            }
        }
        // 两个账本全部读验通过后，才允许写入重启中断结局。
        if interrupted {
            persist(&mut inner, next)?;
        }
        evict(&mut inner);
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }
    pub(super) fn change_token(
        &self,
        scope: &MemoryScope,
    ) -> LearningResult<Option<LearningChangeToken>> {
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        let inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        Ok(Some(LearningChangeToken {
            instance: inner.change.instance,
            revision: inner.scope_changes.get(scope).copied().unwrap_or(0),
        }))
    }
    fn lock(&self) -> LearningResult<WorkingGuard<'_>> {
        let mut inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        let loaded = (|| {
            if let Some(index) = &inner.job_index {
                inner.document.jobs = archive::thaw(
                    inner.archive.as_deref().ok_or(LearningError::Unavailable)?,
                    index,
                    "jobs",
                )?;
            }
            if let Some(index) = &inner.decision_index {
                inner.decisions.records = archive::thaw(
                    inner.archive.as_deref().ok_or(LearningError::Unavailable)?,
                    index,
                    "decisions",
                )?;
            }
            validate_document(&inner.document, inner.job_index.is_some())?;
            validate_decisions_document(
                &inner.decisions,
                &inner.document,
                inner.decision_index.is_some(),
            )?;
            Ok(())
        })();
        if let Err(error) = loaded {
            inner.context = None;
            evict(&mut inner);
            return Err(error);
        }
        Ok(WorkingGuard(inner))
    }
    pub(super) fn archived_history(&self) -> LearningResult<bool> {
        let inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        Ok(inner.archive.is_some())
    }
    pub(super) fn scan_jobs(
        self: &Arc<Self>,
        scope: &MemoryScope,
    ) -> LearningResult<Box<dyn LearningHistoryCursor<LearningJob>>> {
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        let inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        let target = scope.clone();
        Ok(Box::new(ArchiveCursor {
            service: self.clone(),
            scope: scope.clone(),
            store: inner.archive.clone(),
            refs: inner
                .job_index
                .as_ref()
                .map(|i| i.records.clone().into())
                .unwrap_or_default(),
            hot: if inner.job_index.is_none() {
                inner
                    .document
                    .jobs
                    .iter()
                    .filter(|r| &r.job.batch.scope == scope)
                    .map(|r| r.job.clone())
                    .collect()
            } else {
                VecDeque::new()
            },
            decode: Box::new(move |value| {
                let record: JobRecord =
                    serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                let mut document = Document {
                    format_version: 1,
                    jobs: vec![record],
                };
                validate_document(&document, true)?;
                let job = document.jobs.pop().ok_or(LearningError::CorruptState)?.job;
                Ok(if job.batch.scope == target {
                    Some(job)
                } else {
                    None
                })
            }),
            failed: false,
        }))
    }
    pub(super) fn scan_decisions(
        self: &Arc<Self>,
        scope: &MemoryScope,
    ) -> LearningResult<Box<dyn LearningHistoryCursor<LearningDecisionRecord>>> {
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        let inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        let target = scope.clone();
        let mut last = 0u64;
        Ok(Box::new(ArchiveCursor {
            service: self.clone(),
            scope: scope.clone(),
            store: inner.archive.clone(),
            refs: inner
                .decision_index
                .as_ref()
                .map(|i| i.records.clone().into())
                .unwrap_or_default(),
            hot: if inner.decision_index.is_none() {
                inner
                    .decisions
                    .records
                    .iter()
                    .filter(|r| &r.scope == scope)
                    .map(|r| r.record.clone())
                    .collect()
            } else {
                VecDeque::new()
            },
            decode: Box::new(move |value| {
                let entry: ScopedDecisionRecord =
                    serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                entry
                    .scope
                    .validate()
                    .map_err(|_| LearningError::CorruptState)?;
                entry.record.decision.validate()?;
                if entry.scope != target {
                    return Ok(None);
                }
                last = last.checked_add(1).ok_or(LearningError::CorruptState)?;
                if entry.record.sequence != last {
                    return Err(LearningError::CorruptState);
                }
                Ok(Some(entry.record))
            }),
            failed: false,
        }))
    }
    pub(super) fn record_decision(
        &self,
        scope: &MemoryScope,
        decision: LearningDecision,
        at_ms: u64,
    ) -> LearningResult<LearningDecisionRecord> {
        {
            let mut inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
            if inner.context.is_none() {
                return Err(LearningError::Unavailable);
            }
            if inner.archive.is_some()
                && inner.job_index.is_some()
                && inner.decision_index.is_some()
            {
                return cold_decisions::append(&mut inner, scope, decision, at_ms);
            }
        }
        let mut inner = self.lock()?;
        validate_decision_binding(&inner.document, scope, &decision, at_ms)?;
        // 只与该候选最近一次决定比较；A→B→A 必须保留三个历史结局。
        // Memory 的其他写入提升修订号，不应让轮询重复写入同一决定。
        if let Some(previous) = inner.decisions.records.iter().rev().find(|record| {
            record.scope == *scope && record.record.decision.candidate_id == decision.candidate_id
        }) && previous.record.decision.same_outcome(&decision)
        {
            return Ok(previous.record.clone());
        }
        if inner.archive.is_none() && inner.decisions.records.len() >= MAX_DECISIONS {
            return Err(LearningError::LimitReached);
        }
        let sequence = inner
            .decisions
            .records
            .iter()
            .rev()
            .find(|record| record.scope == *scope)
            .map_or(Some(1), |record| record.record.sequence.checked_add(1))
            .ok_or(LearningError::LimitReached)?;
        let record = LearningDecisionRecord {
            sequence,
            at_ms,
            decision,
        };
        let mut next = inner.decisions.clone();
        next.records.push(ScopedDecisionRecord {
            scope: scope.clone(),
            record: record.clone(),
        });
        persist_decisions(&mut inner, next)?;
        Ok(record)
    }
    pub(super) fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        let mut inner = self.lock()?;
        options.validate()?;
        validate_id(extractor_version).map_err(|_| LearningError::InvalidInput)?;
        validate_memory(memory)?;
        let previous: Vec<_> = inner
            .document
            .jobs
            .iter()
            .filter(|record| record.job.batch.scope == memory.scope)
            .collect();
        let mut consumed = BTreeSet::new();
        for record in &previous {
            if memory.revision < record.memory_revision {
                return Err(LearningError::Conflict);
            }
            for evidence in &record.job.batch.evidence {
                // 已提炼批次的来源可能已移入记忆归档；热层同 ID 不同内容仍是冲突。
                if !memory.evidence.iter().any(|value| value == evidence)
                    && !memory.may_be_archived(evidence)
                {
                    return Err(LearningError::Conflict);
                }
                consumed.insert(evidence.id.as_str());
            }
        }
        if let Some(last) = previous.iter().max_by_key(|r| r.job.batch.started_at_ms) {
            let Some(elapsed) = now_ms.checked_sub(last.job.batch.started_at_ms) else {
                return Ok(None);
            };
            if elapsed < options.cooldown_ms {
                return Ok(None);
            }
        }
        let mut available: Vec<_> = memory
            .evidence
            .iter()
            .filter(|evidence| {
                matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
                    && !consumed.contains(evidence.id.as_str())
            })
            .collect();
        available.sort_by_key(|evidence| evidence.revision);
        let mut batch = LearningBatch {
            attempt: 0,
            // 摘要长度固定；先用等长占位值计算完整输入 JSON 的预算。
            id: format!("learning-batch-{}", "0".repeat(64)),
            scope: memory.scope.clone(),
            extractor_version: extractor_version.into(),
            started_at_ms: now_ms,
            evidence: vec![],
        };
        for next in available {
            let size = serde_json::to_vec(next)
                .map_err(|_| LearningError::InvalidInput)?
                .len();
            if size > MAX_EVIDENCE_BYTES {
                continue;
            }
            batch.evidence.push(next.clone());
            if serde_json::to_vec(&batch)
                .map_err(|_| LearningError::InvalidInput)?
                .len()
                > MAX_INPUT_BYTES
            {
                batch.evidence.pop();
                continue;
            }
            if batch.evidence.len() == options.max_batch_evidence {
                break;
            }
        }
        let aged = batch
            .evidence
            .first()
            .is_some_and(|e| now_ms.saturating_sub(e.at_ms) >= MAINTENANCE_DELAY_MS);
        if batch.evidence.is_empty() || (batch.evidence.len() < options.min_new_evidence && !aged) {
            return Ok(None);
        }
        if inner.archive.is_none() && inner.document.jobs.len() >= MAX_JOBS {
            return Err(LearningError::LimitReached);
        }
        batch.id = batch_id(&memory.scope, extractor_version, &batch.evidence)?;
        let mut next = inner.document.clone();
        next.jobs.push(JobRecord {
            memory_revision: memory.revision,
            previous_attempts: vec![],
            reflection: None,
            job: LearningJob {
                batch: batch.clone(),
                status: JobStatus::Running,
                finished_at_ms: None,
                candidates: vec![],
            },
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }
    pub(super) fn reserve_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn eve_memory_api::MemoryService,
        now_ms: u64,
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        use eve_memory_api::{MemoryEvidenceCursor, ValidatedMemoryCursor};
        options.validate()?;
        validate_id(extractor_version).map_err(|_| LearningError::InvalidInput)?;
        let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
        let revision = cursor.summary().revision;
        let mut inner = self.lock()?;
        let previous: Vec<_> = inner
            .document
            .jobs
            .iter()
            .filter(|r| r.job.batch.scope == *scope)
            .collect();
        let mut expected = BTreeMap::new();
        for record in &previous {
            if revision < record.memory_revision {
                return Err(LearningError::Conflict);
            }
            for evidence in &record.job.batch.evidence {
                if expected
                    .insert(evidence.id.as_str(), evidence)
                    .is_some_and(|old| old != evidence)
                {
                    return Err(LearningError::Conflict);
                }
            }
        }
        let cooling = previous
            .iter()
            .max_by_key(|r| r.job.batch.started_at_ms)
            .is_some_and(|r| {
                now_ms
                    .checked_sub(r.job.batch.started_at_ms)
                    .is_none_or(|elapsed| elapsed < options.cooldown_ms)
            });
        let mut seen = BTreeSet::new();
        let mut batch = LearningBatch {
            attempt: 0,
            id: format!("learning-batch-{}", "0".repeat(64)),
            scope: scope.clone(),
            extractor_version: extractor_version.into(),
            started_at_ms: now_ms,
            evidence: vec![],
        };
        loop {
            let page = cursor.next_page()?;
            if page.is_empty() {
                break;
            }
            for evidence in page {
                if let Some(prior) = expected.get(evidence.id.as_str()) {
                    if **prior != evidence {
                        return Err(LearningError::Conflict);
                    }
                    seen.insert(evidence.id.clone());
                    continue;
                }
                if cooling
                    || batch.evidence.len() == options.max_batch_evidence
                    || !matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
                {
                    continue;
                }
                if serde_json::to_vec(&evidence)
                    .map_err(|_| LearningError::InvalidInput)?
                    .len()
                    > MAX_EVIDENCE_BYTES
                {
                    continue;
                }
                batch.evidence.push(evidence);
                if serde_json::to_vec(&batch)
                    .map_err(|_| LearningError::InvalidInput)?
                    .len()
                    > MAX_INPUT_BYTES
                {
                    batch.evidence.pop();
                }
            }
        }
        if seen.len() != expected.len() {
            return Err(LearningError::Conflict);
        }
        let aged = batch
            .evidence
            .first()
            .is_some_and(|e| now_ms.saturating_sub(e.at_ms) >= MAINTENANCE_DELAY_MS);
        if cooling
            || batch.evidence.is_empty()
            || (batch.evidence.len() < options.min_new_evidence && !aged)
        {
            return Ok(None);
        }
        if inner.archive.is_none() && inner.document.jobs.len() >= MAX_JOBS {
            return Err(LearningError::LimitReached);
        }
        batch.id = batch_id(scope, extractor_version, &batch.evidence)?;
        let mut next = inner.document.clone();
        next.jobs.push(JobRecord {
            memory_revision: revision,
            previous_attempts: vec![],
            reflection: None,
            job: LearningJob {
                batch: batch.clone(),
                status: JobStatus::Running,
                finished_at_ms: None,
                candidates: vec![],
            },
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }

    pub(super) fn reserve_repair_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn eve_memory_api::MemoryService,
        now_ms: u64,
        extractor_version: &str,
    ) -> LearningResult<Option<LearningBatch>> {
        use eve_memory_api::{MemoryEvidenceCursor, ValidatedMemoryCursor};
        validate_id(extractor_version).map_err(|_| LearningError::InvalidInput)?;
        let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
        let revision = cursor.summary().revision;
        let mut inner = self.lock()?;
        let busy = inner
            .document
            .jobs
            .iter()
            .any(|r| r.job.batch.scope == *scope && r.job.status == JobStatus::Running);
        let index = (!busy)
            .then(|| {
                inner.document.jobs.iter().position(|r| {
                    r.job.batch.scope == *scope
                        && r.job.batch.extractor_version == extractor_version
                        && retryable(&r.job.status)
                        && r.previous_attempts.len() < MAX_REPAIRS
                        && now_ms
                            .checked_sub(r.job.finished_at_ms.unwrap_or(r.job.batch.started_at_ms))
                            .is_some_and(|elapsed| elapsed >= REPAIR_COOLDOWN_MS)
                })
            })
            .flatten();
        let mut expected = BTreeMap::new();
        if let Some(index) = index {
            let old = &inner.document.jobs[index];
            if revision < old.memory_revision {
                return Err(LearningError::Conflict);
            }
            for evidence in &old.job.batch.evidence {
                expected.insert(evidence.id.as_str(), evidence);
            }
        }
        let mut seen = BTreeSet::new();
        loop {
            let page = cursor.next_page()?;
            if page.is_empty() {
                break;
            }
            for evidence in page {
                if let Some(prior) = expected.get(evidence.id.as_str()) {
                    if **prior != evidence {
                        return Err(LearningError::Conflict);
                    }
                    seen.insert(evidence.id.clone());
                }
            }
        }
        if seen.len() != expected.len() {
            return Err(LearningError::Conflict);
        }
        let Some(index) = index else {
            return Ok(None);
        };
        let mut next = inner.document.clone();
        let record = &mut next.jobs[index];
        record.previous_attempts.push(MaintenanceAttempt {
            started_at_ms: record.job.batch.started_at_ms,
            finished_at_ms: record.job.finished_at_ms,
            status: record.job.status.clone(),
        });
        record.job.batch.started_at_ms = now_ms;
        record.job.batch.attempt = u8::try_from(record.previous_attempts.len())
            .map_err(|_| LearningError::InvalidInput)?;
        record.job.status = JobStatus::Running;
        record.job.finished_at_ms = None;
        let batch = record.job.batch.clone();
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }

    pub(super) fn finish(
        &self,
        batch: &LearningBatch,
        at_ms: u64,
        outcome: LearningOutcome,
    ) -> LearningResult<()> {
        let mut inner = self.lock()?;
        let (outcome, reflection) = match outcome {
            LearningOutcome::Maintained(value) => (
                LearningOutcome::Completed(value.candidates),
                value.reflection,
            ),
            other => (other, None),
        };
        if let Some(reflection) = &reflection {
            reflection.validate(batch)?;
        }
        let index = inner
            .document
            .jobs
            .iter()
            .position(|record| record.job.batch.id == batch.id)
            .ok_or(LearningError::Conflict)?;
        let old = &inner.document.jobs[index].job;
        if old.batch != *batch {
            return Err(LearningError::Conflict);
        }
        if old.status != JobStatus::Running {
            let replay = match (&old.status, &outcome) {
                (JobStatus::Completed, LearningOutcome::Completed(drafts)) => {
                    old.candidates.len() == drafts.len()
                        && old
                            .candidates
                            .iter()
                            .zip(drafts)
                            .all(|(candidate, draft)| candidate.draft == *draft)
                }
                (JobStatus::Failed(first), LearningOutcome::Failed(second)) => first == second,
                _ => false,
            };
            return if replay && inner.document.jobs[index].reflection == reflection {
                Ok(())
            } else {
                Err(LearningError::Conflict)
            };
        }
        if let LearningOutcome::Completed(drafts) = &outcome {
            validate_drafts(batch, drafts)?;
        }
        if at_ms < batch.started_at_ms {
            return Err(LearningError::InvalidInput);
        }
        let mut next = inner.document.clone();
        next.jobs[index].reflection = reflection;
        let job = &mut next.jobs[index].job;
        job.finished_at_ms = Some(at_ms);
        match outcome {
            LearningOutcome::Maintained(_) => unreachable!("normalized above"),
            LearningOutcome::Failed(reason) => job.status = JobStatus::Failed(reason),
            LearningOutcome::Completed(drafts) => {
                let expires_at_ms = at_ms
                    .checked_add(CANDIDATE_TTL_MS)
                    .ok_or(LearningError::InvalidInput)?;
                job.status = JobStatus::Completed;
                job.candidates = drafts
                    .into_iter()
                    .enumerate()
                    .map(|(index, draft)| PreferenceCandidate {
                        id: candidate_id(&batch.id, index),
                        batch_id: batch.id.clone(),
                        draft,
                        created_at_ms: at_ms,
                        expires_at_ms,
                    })
                    .collect();
            }
        }
        persist(&mut inner, next)
    }
    pub(super) fn maintenance_receipts(
        &self,
        scope: &MemoryScope,
    ) -> LearningResult<Vec<MaintenanceReceipt>> {
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        let inner = self.lock()?;
        Ok(inner
            .document
            .jobs
            .iter()
            .filter(|r| &r.job.batch.scope == scope)
            .map(|r| MaintenanceReceipt {
                batch_id: r.job.batch.id.clone(),
                extractor_version: r.job.batch.extractor_version.clone(),
                source_revision: r.memory_revision,
                evidence_ids: r.job.batch.evidence.iter().map(|e| e.id.clone()).collect(),
                status: r.job.status.clone(),
                started_at_ms: r.job.batch.started_at_ms,
                finished_at_ms: r.job.finished_at_ms,
                previous_attempts: r.previous_attempts.clone(),
                reflection: r.reflection.clone(),
            })
            .collect())
    }
    pub(super) fn reserve_repair(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        extractor_version: &str,
    ) -> LearningResult<Option<LearningBatch>> {
        validate_memory(memory)?;
        let mut inner = self.lock()?;
        if inner
            .document
            .jobs
            .iter()
            .any(|r| r.job.batch.scope == memory.scope && r.job.status == JobStatus::Running)
        {
            return Ok(None);
        }
        let Some(index) = inner.document.jobs.iter().position(|r| {
            r.job.batch.scope == memory.scope
                && r.job.batch.extractor_version == extractor_version
                && retryable(&r.job.status)
                && r.previous_attempts.len() < MAX_REPAIRS
                && now_ms
                    .checked_sub(r.job.finished_at_ms.unwrap_or(r.job.batch.started_at_ms))
                    .is_some_and(|elapsed| elapsed >= REPAIR_COOLDOWN_MS)
        }) else {
            return Ok(None);
        };
        let old = &inner.document.jobs[index];
        if memory.revision < old.memory_revision
            || old
                .job
                .batch
                .evidence
                .iter()
                .any(|e| !memory.evidence.iter().any(|v| v == e))
        {
            return Err(LearningError::Conflict);
        }
        let mut next = inner.document.clone();
        let r = &mut next.jobs[index];
        r.previous_attempts.push(MaintenanceAttempt {
            started_at_ms: r.job.batch.started_at_ms,
            finished_at_ms: r.job.finished_at_ms,
            status: r.job.status.clone(),
        });
        r.job.batch.started_at_ms = now_ms;
        r.job.batch.attempt =
            u8::try_from(r.previous_attempts.len()).map_err(|_| LearningError::InvalidInput)?;
        r.job.status = JobStatus::Running;
        r.job.finished_at_ms = None;
        let batch = r.job.batch.clone();
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }
    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("偏好提炼状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn persist(inner: &mut Inner, next: Document) -> LearningResult<()> {
    let revision = inner
        .change
        .revision
        .checked_add(1)
        .ok_or(LearningError::LimitReached)?;
    let (bytes, index) = if let Some(store) = &inner.archive {
        let (bytes, index) = archive::freeze(store.as_ref(), "jobs", &next.jobs)?;
        (bytes, Some(index))
    } else {
        (encode(&next)?, None)
    };
    let context = inner.context.as_ref().ok_or(LearningError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(LEARNING_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(LearningError::Storage);
    }
    for (position, record) in next.jobs.iter().enumerate() {
        if inner.document.jobs.get(position) != Some(record) {
            if let Some(old) = inner.document.jobs.get(position) {
                inner
                    .scope_changes
                    .insert(old.job.batch.scope.clone(), revision);
            }
            inner
                .scope_changes
                .insert(record.job.batch.scope.clone(), revision);
        }
    }
    for old in inner.document.jobs.iter().skip(next.jobs.len()) {
        inner
            .scope_changes
            .insert(old.job.batch.scope.clone(), revision);
    }
    inner.document = next;
    inner.job_index = index;
    inner.change.revision = revision;
    Ok(())
}
fn persist_decisions(inner: &mut Inner, next: DecisionsDocument) -> LearningResult<()> {
    let revision = inner
        .change
        .revision
        .checked_add(1)
        .ok_or(LearningError::LimitReached)?;
    if inner.archive.is_none() && next.records.len() > MAX_DECISIONS {
        return Err(LearningError::LimitReached);
    }
    let (bytes, index) = if let Some(store) = &inner.archive {
        let (bytes, index) = archive::freeze(store.as_ref(), "decisions", &next.records)?;
        (bytes, Some(index))
    } else {
        (
            serde_json::to_vec(&next).map_err(|_| LearningError::InvalidInput)?,
            None,
        )
    };
    if bytes.len() > MAX_DECISIONS_STATE_BYTES {
        return Err(LearningError::LimitReached);
    }
    let context = inner.context.as_ref().ok_or(LearningError::Unavailable)?;
    // 账本提交和记忆修改分属两个插件；这里仅确认决策意图已持久保存。
    // 即使错误出现在实际写入之后，也关闭两个学习句柄并等待重新打开核对。
    if context.state_set(DECISIONS_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(LearningError::Storage);
    }
    for (position, record) in next.records.iter().enumerate() {
        if inner.decisions.records.get(position) != Some(record) {
            if let Some(old) = inner.decisions.records.get(position) {
                inner.scope_changes.insert(old.scope.clone(), revision);
            }
            inner.scope_changes.insert(record.scope.clone(), revision);
        }
    }
    for old in inner.decisions.records.iter().skip(next.records.len()) {
        inner.scope_changes.insert(old.scope.clone(), revision);
    }
    inner.decisions = next;
    inner.decision_index = index;
    inner.change.revision = revision;
    Ok(())
}

fn validate_decision_binding(
    document: &Document,
    scope: &MemoryScope,
    decision: &LearningDecision,
    at_ms: u64,
) -> LearningResult<()> {
    scope.validate().map_err(|_| LearningError::InvalidInput)?;
    decision.validate()?;
    let job = document
        .jobs
        .iter()
        .find(|record| record.job.batch.scope == *scope && record.job.batch.id == decision.batch_id)
        .ok_or(LearningError::Conflict)?;
    if job.job.status != JobStatus::Completed || decision.memory_revision < job.memory_revision {
        return Err(LearningError::Conflict);
    }
    let candidate = job
        .job
        .candidates
        .iter()
        .find(|candidate| candidate.id == decision.candidate_id)
        .ok_or(LearningError::Conflict)?;
    if candidate.batch_id != decision.batch_id
        || candidate.draft.evidence_ids != decision.evidence_ids
        || at_ms < candidate.created_at_ms
    {
        return Err(LearningError::Conflict);
    }
    Ok(())
}

fn validate_decisions_document(
    decisions: &DecisionsDocument,
    document: &Document,
    archived: bool,
) -> LearningResult<()> {
    if !archived && decisions.records.len() > MAX_DECISIONS {
        return Err(LearningError::CorruptState);
    }
    let mut sequences = BTreeMap::new();
    let mut last_by_candidate: BTreeMap<_, &LearningDecision> = BTreeMap::new();
    for entry in &decisions.records {
        let record = &entry.record;
        validate_decision_binding(document, &entry.scope, &record.decision, record.at_ms)?;
        let expected = sequences
            .get(&entry.scope)
            .copied()
            .unwrap_or(0_u64)
            .checked_add(1)
            .ok_or(LearningError::CorruptState)?;
        if record.sequence != expected {
            return Err(LearningError::CorruptState);
        }
        sequences.insert(&entry.scope, record.sequence);
        let key = (&entry.scope, &record.decision.candidate_id);
        if let Some(previous) = last_by_candidate.insert(key, &record.decision)
            && previous.same_outcome(&record.decision)
        {
            return Err(LearningError::CorruptState);
        }
    }
    Ok(())
}
fn encode(document: &Document) -> LearningResult<Vec<u8>> {
    if document.jobs.len() > MAX_JOBS {
        return Err(LearningError::LimitReached);
    }
    let bytes = serde_json::to_vec(document).map_err(|_| LearningError::InvalidInput)?;
    // Interrupted 比 Running 多四个 ASCII 字节。写入时保留恢复空间，避免状态
    // 刚好填满后，合法的中断标记反而无法在下次启动时原子保存。
    let recovery_bytes = document
        .jobs
        .iter()
        .filter(|record| record.job.status == JobStatus::Running)
        .count()
        * ("Interrupted".len() - "Running".len());
    if bytes.len().saturating_add(recovery_bytes) > MAX_STATE_BYTES {
        return Err(LearningError::LimitReached);
    }
    Ok(bytes)
}

/// 验证模型候选；不执行模型调用，不修改持久状态或确认偏好。
pub fn validate_drafts(batch: &LearningBatch, drafts: &[CandidateDraft]) -> LearningResult<()> {
    if drafts.len() > MAX_CANDIDATES {
        return Err(LearningError::InvalidInput);
    }
    let evidence_ids: BTreeSet<_> = batch.evidence.iter().map(|item| &item.id).collect();
    for draft in drafts {
        validate_text(&draft.text, MAX_CANDIDATE_BYTES).map_err(|_| LearningError::InvalidInput)?;
        if draft.confidence > 100 || draft.evidence_ids.is_empty() {
            return Err(LearningError::InvalidInput);
        }
        let mut references = BTreeSet::new();
        for id in &draft.evidence_ids {
            if !evidence_ids.contains(id) || !references.insert(id) {
                return Err(LearningError::InvalidInput);
            }
        }
    }
    Ok(())
}
fn batch_id(
    scope: &MemoryScope,
    version: &str,
    evidence: &[InteractionEvidence],
) -> LearningResult<String> {
    let stable_evidence: Vec<_> = evidence
        .iter()
        .map(|value| {
            let mut stable = value.clone();
            stable.at_ms = 0;
            stable
        })
        .collect();
    let bytes = serde_json::to_vec(&(scope, version, stable_evidence))
        .map_err(|_| LearningError::InvalidInput)?;
    Ok(hash_id("learning-batch-", &bytes))
}
fn candidate_id(batch_id: &str, index: usize) -> String {
    // 批次标识为固定十六进制摘要；分隔符和整数索引没有连接歧义。
    hash_id(
        "learning-candidate-",
        format!("{batch_id}:{index}").as_bytes(),
    )
}
fn hash_id(prefix: &str, bytes: &[u8]) -> String {
    let mut id = String::from(prefix);
    for byte in digest(&SHA256, bytes).as_ref() {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

fn validate_evidence(evidence: &InteractionEvidence) -> LearningResult<()> {
    validate_id(&evidence.id).map_err(|_| LearningError::InvalidInput)?;
    if evidence.revision == 0 {
        return Err(LearningError::InvalidInput);
    }
    let text_limit = eve_memory_api::MAX_TEXT_BYTES;
    match &evidence.source {
        EvidenceSource::UserStatement { message_id, text } => {
            validate_id(message_id).map_err(|_| LearningError::InvalidInput)?;
            validate_text(text, text_limit).map_err(|_| LearningError::InvalidInput)?;
        }
        EvidenceSource::CompletedInteraction {
            message_id,
            session_revision,
            turn_id,
            user_text,
            assistant_text,
        } => {
            validate_id(message_id).map_err(|_| LearningError::InvalidInput)?;
            validate_text(user_text, text_limit).map_err(|_| LearningError::InvalidInput)?;
            validate_text(assistant_text, text_limit).map_err(|_| LearningError::InvalidInput)?;
            if *turn_id == 0
                || turn_id
                    .checked_mul(2)
                    .is_none_or(|minimum| *session_revision < minimum)
            {
                return Err(LearningError::InvalidInput);
            }
        }
    }
    Ok(())
}
fn source_message(evidence: &InteractionEvidence) -> &str {
    match &evidence.source {
        EvidenceSource::UserStatement { message_id, .. }
        | EvidenceSource::CompletedInteraction { message_id, .. } => message_id,
    }
}
fn source_turn(evidence: &InteractionEvidence) -> Option<u64> {
    match evidence.source {
        EvidenceSource::CompletedInteraction { turn_id, .. } => Some(turn_id),
        _ => None,
    }
}
fn validate_memory(memory: &MemorySnapshot) -> LearningResult<()> {
    memory
        .scope
        .validate()
        .map_err(|_| LearningError::InvalidInput)?;
    // 完整记忆可以超过热层容量；只核对修订不少于证据条数且有界。
    if memory.evidence.len() as u64 > memory.revision
        || memory.revision
            > (memory.evidence.len() + eve_memory_api::MAX_HISTORY) as u64
                + memory.archived_evidence
    {
        return Err(LearningError::InvalidInput);
    }
    let mut ids = BTreeSet::new();
    let mut messages = BTreeSet::new();
    let mut turns = BTreeSet::new();
    let mut revisions = BTreeSet::new();
    for evidence in &memory.evidence {
        validate_evidence(evidence)?;
        if evidence.revision > memory.revision
            || !ids.insert(&evidence.id)
            || !messages.insert(source_message(evidence))
            || !revisions.insert(evidence.revision)
            || source_turn(evidence).is_some_and(|turn| !turns.insert(turn))
        {
            return Err(LearningError::InvalidInput);
        }
    }
    Ok(())
}
fn validate_batch(batch: &LearningBatch, memory_revision: u64) -> LearningResult<()> {
    batch
        .scope
        .validate()
        .map_err(|_| LearningError::InvalidInput)?;
    validate_id(&batch.extractor_version).map_err(|_| LearningError::InvalidInput)?;
    if memory_revision == 0
        || batch.evidence.is_empty()
        || batch.evidence.len() > MAX_BATCH_EVIDENCE
        || batch.id != batch_id(&batch.scope, &batch.extractor_version, &batch.evidence)?
    {
        return Err(LearningError::InvalidInput);
    }
    let mut previous_revision = 0;
    for evidence in &batch.evidence {
        validate_evidence(evidence)?;
        if !matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
            || evidence.revision <= previous_revision
            || evidence.revision > memory_revision
            || serde_json::to_vec(evidence)
                .map_err(|_| LearningError::InvalidInput)?
                .len()
                > MAX_EVIDENCE_BYTES
        {
            return Err(LearningError::InvalidInput);
        }
        previous_revision = evidence.revision;
    }
    if serde_json::to_vec(batch)
        .map_err(|_| LearningError::InvalidInput)?
        .len()
        > MAX_INPUT_BYTES
    {
        return Err(LearningError::InvalidInput);
    }
    Ok(())
}
fn validate_document(document: &Document, archived: bool) -> LearningResult<()> {
    if !archived && document.jobs.len() > MAX_JOBS {
        return Err(LearningError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    let mut consumed_ids = BTreeSet::new();
    let mut consumed_messages = BTreeSet::new();
    let mut consumed_turns = BTreeSet::new();
    let mut consumed_revisions = BTreeSet::new();
    let mut previous_by_scope = BTreeMap::new();
    for record in &document.jobs {
        let job = &record.job;
        let batch = &job.batch;
        validate_batch(batch, record.memory_revision)?;
        if record.previous_attempts.len() > MAX_REPAIRS
            || usize::from(batch.attempt) != record.previous_attempts.len()
        {
            return Err(LearningError::CorruptState);
        }
        let mut previous_end = 0;
        for attempt in &record.previous_attempts {
            if !retryable(&attempt.status)
                || attempt.started_at_ms < previous_end
                || attempt
                    .finished_at_ms
                    .is_some_and(|end| end < attempt.started_at_ms)
                || (attempt.status == JobStatus::Interrupted) != attempt.finished_at_ms.is_none()
            {
                return Err(LearningError::CorruptState);
            }
            previous_end = attempt
                .finished_at_ms
                .unwrap_or(attempt.started_at_ms)
                .checked_add(REPAIR_COOLDOWN_MS)
                .ok_or(LearningError::CorruptState)?;
        }
        if batch.started_at_ms < previous_end {
            return Err(LearningError::CorruptState);
        }
        if let Some(reflection) = &record.reflection {
            if job.status != JobStatus::Completed {
                return Err(LearningError::CorruptState);
            }
            reflection.validate(batch)?;
        }
        if !ids.insert(&batch.id) {
            return Err(LearningError::CorruptState);
        }
        if let Some((started_at_ms, memory_revision)) = previous_by_scope.insert(
            &batch.scope,
            (
                record
                    .previous_attempts
                    .first()
                    .map_or(batch.started_at_ms, |a| a.started_at_ms),
                record.memory_revision,
            ),
        ) && (record
            .previous_attempts
            .first()
            .map_or(batch.started_at_ms, |a| a.started_at_ms)
            < started_at_ms
            || record.memory_revision < memory_revision)
        {
            return Err(LearningError::CorruptState);
        }
        for evidence in &batch.evidence {
            if !consumed_ids.insert((&batch.scope, &evidence.id))
                || !consumed_messages.insert((&batch.scope, source_message(evidence)))
                || !consumed_turns.insert((&batch.scope, source_turn(evidence)))
                || !consumed_revisions.insert((&batch.scope, evidence.revision))
            {
                return Err(LearningError::CorruptState);
            }
        }
        let drafts: Vec<_> = job
            .candidates
            .iter()
            .map(|item| item.draft.clone())
            .collect();
        validate_drafts(batch, &drafts)?;
        match &job.status {
            JobStatus::Running | JobStatus::Interrupted => {
                if job.finished_at_ms.is_some() || !job.candidates.is_empty() {
                    return Err(LearningError::CorruptState);
                }
            }
            JobStatus::Failed(_) | JobStatus::Completed => {
                let at_ms = job.finished_at_ms.ok_or(LearningError::CorruptState)?;
                if at_ms < batch.started_at_ms {
                    return Err(LearningError::CorruptState);
                }
                if job.status == JobStatus::Completed
                    && at_ms.checked_add(CANDIDATE_TTL_MS).is_none()
                {
                    return Err(LearningError::CorruptState);
                }
                if matches!(job.status, JobStatus::Failed(_)) && !job.candidates.is_empty() {
                    return Err(LearningError::CorruptState);
                }
                for (index, candidate) in job.candidates.iter().enumerate() {
                    if candidate.id != candidate_id(&batch.id, index)
                        || candidate.batch_id != batch.id
                        || candidate.created_at_ms != at_ms
                        || at_ms.checked_add(CANDIDATE_TTL_MS) != Some(candidate.expires_at_ms)
                    {
                        return Err(LearningError::CorruptState);
                    }
                }
            }
        }
    }
    Ok(())
}

fn retryable(status: &JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Interrupted
            | JobStatus::Failed(LearningFailure::TransientProvider | LearningFailure::Timeout)
    )
}
