use super::{
    SUMMARY_STATE_KEY,
    history::{self, Cold, Metadata, Reference},
    validation::*,
};
use crate::{ExtractiveConversationDistiller, strict_json};
use eve_memory_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    jobs: Vec<SummaryJob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tail: Option<Reference>,
}
const INITIALIZED_KEY: &str = "summaries.initialized.v2";
const INITIALIZED: &[u8] = br#"{"format_version":2,"initialized":true}"#;

struct Inner {
    document: Document,
    context: Option<PluginContext>,
    cold: Option<Cold>,
    sealed: bool,
    repair_scans: BTreeMap<MemoryScope, (u64, u64, u64)>,
}
pub(super) struct StoredSummaries {
    inner: Mutex<Inner>,
}
impl StoredSummaries {
    pub(super) fn latest_completed(&self, scope: &MemoryScope) -> MemoryResult<Option<SummaryJob>> {
        scope.validate()?;
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        latest(&mut inner, scope)
    }
    pub(super) fn last_provider_failure_at(&self) -> MemoryResult<Option<u64>> {
        let inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        let cold = inner
            .cold
            .as_ref()
            .and_then(|cold| cold.base.provider_failure);
        Ok(cold.max(
            inner
                .document
                .jobs
                .iter()
                .filter(|job| {
                    matches!(
                        job.status,
                        SummaryStatus::Failed(SummaryFailure::Provider | SummaryFailure::Timeout)
                    )
                })
                .filter_map(|job| job.finished_at_ms)
                .max(),
        ))
    }
    pub(super) fn open(
        context: PluginContext,
        archive: Option<std::sync::Arc<dyn SummaryArchiveStore>>,
    ) -> MemoryResult<Self> {
        let marker = context
            .state_get(INITIALIZED_KEY)
            .map_err(|_| MemoryError::Storage)?;
        if marker
            .as_deref()
            .is_some_and(|marker| marker != INITIALIZED)
        {
            return Err(MemoryError::CorruptState);
        }
        let mut document = match context
            .state_get(SUMMARY_STATE_KEY)
            .map_err(|_| MemoryError::Storage)?
        {
            None => {
                if marker.is_some() {
                    return Err(MemoryError::CorruptState);
                }
                Document {
                    format_version: 1,
                    jobs: vec![],
                    tail: None,
                }
            }
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(MemoryError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| MemoryError::CorruptState)?;
                let version = value.get("format_version").and_then(|v| v.as_u64());
                if !matches!(version, Some(1 | 2)) || (version == Some(2) && archive.is_none()) {
                    return Err(MemoryError::UnsupportedVersion);
                }
                let document: Document =
                    serde_json::from_value(value).map_err(|_| MemoryError::CorruptState)?;
                if document.jobs.len() > MAX_SUMMARY_JOBS
                    || (version == Some(1) && document.tail.is_some())
                {
                    return Err(MemoryError::CorruptState);
                }
                document
            }
        };
        let cold = archive
            .map(|archive| history::open(archive, document.tail.as_ref()))
            .transpose()?;
        let legacy = document.format_version == 1 && cold.is_some();
        if cold.is_some() {
            document.format_version = 2;
        }
        let mut inner = Inner {
            document,
            context: Some(context),
            cold,
            sealed: marker.is_some(),
            repair_scans: BTreeMap::new(),
        };
        metadata(&inner).map_err(|_| MemoryError::CorruptState)?;
        let mut next = inner.document.clone();
        let mut changed = legacy;
        for job in &mut next.jobs {
            if job.status == SummaryStatus::Running {
                job.status = SummaryStatus::Interrupted;
                changed = true;
            }
        }
        // 所有冷块及跨记录关系验证完成后，才允许恢复或发布迁移根索引。
        if changed {
            persist(&mut inner, next)?;
        }
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }
    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("语义摘要状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
    pub(super) fn jobs(&self, scope: &MemoryScope) -> MemoryResult<Vec<SummaryJob>> {
        scope.validate()?;
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        let result = (|| {
            let mut result = Vec::new();
            if let Some(cold) = &inner.cold {
                for reference in
                    history::references(cold.archive.as_ref(), inner.document.tail.as_ref())?
                {
                    result.extend(
                        history::read(cold.archive.as_ref(), &reference)?
                            .jobs
                            .into_iter()
                            .filter(|j| j.batch.scope == *scope),
                    );
                }
            }
            result.extend(
                inner
                    .document
                    .jobs
                    .iter()
                    .filter(|j| j.batch.scope == *scope)
                    .cloned(),
            );
            Ok(result)
        })();
        if matches!(
            result,
            Err(MemoryError::Storage | MemoryError::CorruptState)
        ) {
            inner.context = None;
        }
        result
    }

    pub(super) fn reserve(
        &self,
        snapshot: &MemorySnapshot,
        now: u64,
        version: &str,
        options: SummaryOptions,
    ) -> MemoryResult<Option<SummaryBatch>> {
        options.validate()?;
        validate_id(version)?;
        ExtractiveConversationDistiller.distill(
            snapshot,
            DistillationOptions {
                max_episodes: 0,
                max_expression_examples: 0,
                max_bytes: 512,
            },
            &[],
        )?;
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        let meta = metadata(&inner)?;
        if inner
            .document
            .jobs
            .iter()
            .any(|j| j.batch.scope == snapshot.scope && j.status == SummaryStatus::Running)
            || meta
                .scopes
                .get(&snapshot.scope)
                .and_then(|entry| entry.last_started)
                .is_some_and(|at| {
                    now.checked_sub(at)
                        .is_none_or(|elapsed| elapsed < options.cooldown_ms)
                })
        {
            return Ok(None);
        }
        let consumed: BTreeSet<_> = meta
            .consumed
            .keys()
            .filter(|(scope, _)| scope == &snapshot.scope)
            .map(|(_, id)| id.clone())
            .collect();
        let pending: Vec<_> = snapshot
            .evidence
            .iter()
            .filter(|e| !consumed.contains(&e.id))
            .filter_map(user_source)
            .collect();
        if pending.len() < options.min_new_evidence {
            return Ok(None);
        }
        let mut previous = latest(&mut inner, &snapshot.scope)?
            .as_ref()
            .map(|job| effective_points(job, snapshot))
            .transpose()?
            .unwrap_or_default();
        let mut new: Vec<_> = pending.into_iter().take(options.max_new_evidence).collect();
        let batch = loop {
            let referenced: BTreeSet<_> = previous
                .iter()
                .flat_map(|p| p.quotes.iter().map(|q| &q.evidence_id))
                .collect();
            let new_ids: Vec<_> = new.iter().map(|s| s.evidence_id.clone()).collect();
            let mut sources: Vec<SummarySource> = snapshot
                .evidence
                .iter()
                .filter(|e| referenced.contains(&e.id) || new_ids.contains(&e.id))
                .filter_map(user_source)
                .collect();
            // 不完整热快照不能证明归档来源；调用 reserve_reader 才能逐条核对。
            if referenced
                .iter()
                .any(|id| !sources.iter().any(|source| &&source.evidence_id == id))
            {
                return Err(MemoryError::Conflict);
            }
            sources.sort_by_key(|s| s.revision);
            let batch = SummaryBatch {
                sequence: meta
                    .sequence
                    .checked_add(1)
                    .ok_or(MemoryError::LimitReached)?,
                scope: snapshot.scope.clone(),
                source_revision: snapshot.revision,
                started_at_ms: now,
                extractor_version: version.into(),
                consumed_ids: new_ids,
                sources,
                previous: previous.clone(),
            };
            if serde_json::to_vec(&batch)
                .map_err(|_| MemoryError::InvalidInput)?
                .len()
                <= MAX_SUMMARY_INPUT_BYTES
            {
                break batch;
            }
            if !previous.is_empty() {
                previous.remove(0);
            } else if new.len() > 1 {
                new.pop();
            } else {
                return Err(MemoryError::LimitReached);
            }
        };
        validate_batch(&batch)?;
        let mut next = inner.document.clone();
        next.jobs.push(SummaryJob {
            repair: None,
            batch: batch.clone(),
            status: SummaryStatus::Running,
            finished_at_ms: None,
            points: vec![],
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }
    pub(super) fn reserve_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn MemoryService,
        now: u64,
        version: &str,
        options: SummaryOptions,
    ) -> MemoryResult<Option<SummaryBatch>> {
        options.validate()?;
        validate_id(version)?;
        let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
        let summary = cursor.summary().clone();
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        let meta = metadata(&inner)?;
        let cooling = inner
            .document
            .jobs
            .iter()
            .any(|j| j.batch.scope == *scope && j.status == SummaryStatus::Running)
            || meta
                .scopes
                .get(scope)
                .and_then(|entry| entry.last_started)
                .is_some_and(|at| {
                    now.checked_sub(at)
                        .is_none_or(|elapsed| elapsed < options.cooldown_ms)
                });
        let consumed: BTreeSet<_> = meta
            .consumed
            .keys()
            .filter(|(actual, _)| actual == scope)
            .map(|(_, id)| id.clone())
            .collect();
        let previous_job = latest(&mut inner, scope)?;
        let previous_job = previous_job.as_ref();
        let mut expected = BTreeMap::new();
        if let Some(job) = previous_job {
            if job.batch.source_revision > summary.revision {
                return Err(MemoryError::StaleRevision);
            }
            validate_batch(&job.batch)?;
            validate_summary_points(&job.batch, &job.points)?;
            for source in &job.batch.sources {
                expected.insert(source.evidence_id.as_str(), source);
            }
        }
        let mut verified_sources = Vec::new();
        let mut seen = BTreeSet::new();
        let mut fresh = Vec::new();
        let mut fresh_count = 0usize;
        loop {
            let page = cursor.next_page()?;
            if page.is_empty() {
                break;
            }
            for evidence in page {
                let source = user_source(&evidence);
                if let Some(prior) = expected.get(evidence.id.as_str()) {
                    if source.as_ref() != Some(*prior) {
                        return Err(MemoryError::Conflict);
                    }
                    seen.insert(evidence.id.clone());
                    verified_sources.push(source.clone().ok_or(MemoryError::Conflict)?);
                }
                if !cooling
                    && !consumed.contains(evidence.id.as_str())
                    && let Some(source) = source
                {
                    fresh_count = fresh_count.saturating_add(1);
                    if fresh.len() < options.max_new_evidence {
                        fresh.push(source);
                    }
                }
            }
        }
        if seen.len() != expected.len() {
            return Err(MemoryError::Conflict);
        }
        if cooling || fresh_count < options.min_new_evidence {
            return Ok(None);
        }
        let mut previous = previous_job
            .map(|job| filter_points(job, &summary.preferences))
            .unwrap_or_default();
        verified_sources.extend(fresh.iter().cloned());
        let batch = loop {
            let referenced: BTreeSet<_> = previous
                .iter()
                .flat_map(|p| p.quotes.iter().map(|q| q.evidence_id.as_str()))
                .collect();
            let new_ids: Vec<_> = fresh.iter().map(|s| s.evidence_id.clone()).collect();
            let mut sources: Vec<_> = verified_sources
                .iter()
                .filter(|s| {
                    referenced.contains(s.evidence_id.as_str()) || new_ids.contains(&s.evidence_id)
                })
                .cloned()
                .collect();
            sources.sort_by_key(|s| s.revision);
            sources.dedup_by(|a, b| a.evidence_id == b.evidence_id);
            let batch = SummaryBatch {
                sequence: meta
                    .sequence
                    .checked_add(1)
                    .ok_or(MemoryError::LimitReached)?,
                scope: scope.clone(),
                source_revision: summary.revision,
                started_at_ms: now,
                extractor_version: version.into(),
                consumed_ids: new_ids,
                sources,
                previous: previous.clone(),
            };
            if serde_json::to_vec(&batch)
                .map_err(|_| MemoryError::InvalidInput)?
                .len()
                <= MAX_SUMMARY_INPUT_BYTES
            {
                break batch;
            }
            if !previous.is_empty() {
                previous.remove(0);
            } else if fresh.len() > 1 {
                fresh.pop();
            } else {
                return Err(MemoryError::LimitReached);
            }
        };
        validate_batch(&batch)?;
        let mut next = inner.document.clone();
        next.jobs.push(SummaryJob {
            repair: None,
            batch: batch.clone(),
            status: SummaryStatus::Running,
            finished_at_ms: None,
            points: vec![],
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }

    pub(super) fn prefer_repair(&self, scope: &MemoryScope) -> MemoryResult<bool> {
        let inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        Ok(metadata(&inner)?
            .scopes
            .get(scope)
            .is_some_and(|scope| !scope.last_was_repair))
    }
    pub(super) fn reserve_repair_reader(
        &self,
        scope: &MemoryScope,
        reader: &dyn MemoryService,
        now: u64,
        version: &str,
        options: SummaryOptions,
    ) -> MemoryResult<Option<SummaryBatch>> {
        scope.validate()?;
        options.validate()?;
        validate_id(version)?;
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        let meta = metadata(&inner)?;
        if inner
            .document
            .jobs
            .iter()
            .any(|job| job.batch.scope == *scope && job.status == SummaryStatus::Running)
            || meta
                .scopes
                .get(scope)
                .and_then(|scope| scope.last_started)
                .is_some_and(|at| {
                    now.checked_sub(at)
                        .is_none_or(|elapsed| elapsed < options.cooldown_ms)
                })
        {
            return Ok(None);
        }
        let mut roots: Vec<_> = meta
            .repairs
            .values()
            .filter(|root| {
                root.scope == *scope
                    && root.attempt < 2
                    && history::repairable(&root.status)
                    && root.last_finished.is_some_and(|at| {
                        now.checked_sub(at).is_some_and(|elapsed| {
                            elapsed
                                >= options
                                    .cooldown_ms
                                    .max(60_000)
                                    .saturating_mul(1 << root.attempt)
                        })
                    })
            })
            .cloned()
            .collect();
        if roots.is_empty() {
            return Ok(None);
        }
        // 固定原始流完整读完之后才选中补做；坏末页不能让前面的批次漏检。
        let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
        let summary = cursor.summary().clone();
        let last_sequence = roots.last().map(|root| root.sequence).unwrap_or(0);
        let mut after = 0;
        if let Some((sequence, revision, next_at)) = inner.repair_scans.get(scope)
            && *revision == summary.revision
        {
            if now < *next_at {
                return Ok(None);
            }
            if *sequence < last_sequence {
                after = *sequence;
            }
        }
        roots.retain(|root| root.sequence > after);
        let mut blocked = BTreeSet::new();
        for preference in &summary.preferences {
            for prior in &preference.history {
                if preference.status == PreferenceStatus::Revoked
                    || preference.history.last() != Some(prior)
                {
                    blocked.insert(prior.evidence_id.clone());
                }
            }
        }
        // 只读失败候选正文；未选中的旧输入不进入常驻索引。
        let mut candidates = Vec::new();
        for root in roots.into_iter().take(8) {
            after = root.sequence;
            let original = load(&mut inner, root.sequence, root.reference.as_ref())?;
            let mut sources: Vec<_> = original
                .batch
                .sources
                .into_iter()
                .filter(|source| root.consumed_ids.contains(&source.evidence_id))
                .collect();
            sources.sort_by_key(|source| source.revision);
            if root.revision > summary.revision
                || sources
                    .iter()
                    .any(|source| blocked.contains(&source.evidence_id))
            {
                continue;
            }
            candidates.push((root, sources, BTreeSet::new(), false));
            // 单次准入只核对前八个到期候选，下一次仍可公平检查新鲜工作。
        }
        let next_at = if after >= last_sequence {
            now.saturating_add(options.cooldown_ms.max(60_000))
        } else {
            now
        };
        inner
            .repair_scans
            .insert(scope.clone(), (after, summary.revision, next_at));
        loop {
            let page = cursor.next_page()?;
            if page.is_empty() {
                break;
            }
            for evidence in page {
                for (_, sources, seen, invalid) in &mut candidates {
                    if let Some(expected) = sources
                        .iter()
                        .find(|source| source.evidence_id == evidence.id)
                    {
                        if user_source(&evidence).as_ref() != Some(expected) {
                            *invalid = true;
                        }
                        seen.insert(evidence.id.clone());
                    }
                }
            }
        }
        let Some((root, sources, _, _)) = candidates
            .into_iter()
            .find(|(_, sources, seen, invalid)| !invalid && sources.len() == seen.len())
        else {
            return Ok(None);
        };
        let batch = SummaryBatch {
            sequence: meta
                .sequence
                .checked_add(1)
                .ok_or(MemoryError::LimitReached)?,
            scope: scope.clone(),
            source_revision: root.revision,
            started_at_ms: now,
            extractor_version: version.into(),
            consumed_ids: root.consumed_ids,
            sources,
            previous: vec![],
        };
        validate_batch(&batch)?;
        let mut next = inner.document.clone();
        next.jobs.push(SummaryJob {
            repair: Some(SummaryRepair {
                original_sequence: root.sequence,
                attempt: root.attempt + 1,
            }),
            batch: batch.clone(),
            status: SummaryStatus::Running,
            finished_at_ms: None,
            points: vec![],
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }

    pub(super) fn finish(
        &self,
        batch: &SummaryBatch,
        now: u64,
        result: Result<Vec<SummaryPoint>, SummaryFailure>,
    ) -> MemoryResult<()> {
        if now < batch.started_at_ms {
            return Err(MemoryError::InvalidInput);
        }
        if let Ok(points) = &result {
            validate_summary_points(batch, points)?;
        }
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        let mut next = inner.document.clone();
        let job = next
            .jobs
            .iter_mut()
            .find(|j| j.batch == *batch)
            .ok_or(MemoryError::NotFound)?;
        if job.status != SummaryStatus::Running {
            return Err(MemoryError::Conflict);
        }
        match result {
            Ok(points) => {
                job.status = SummaryStatus::Completed;
                job.points = points;
            }
            Err(reason) => job.status = SummaryStatus::Failed(reason),
        }
        job.finished_at_ms = Some(now);
        persist(&mut inner, next)
    }
}
fn metadata(inner: &Inner) -> MemoryResult<Metadata> {
    if inner.context.is_none() {
        return Err(MemoryError::Unavailable);
    }
    if inner.document.jobs.len() > MAX_SUMMARY_JOBS {
        return Err(MemoryError::LimitReached);
    }
    let mut meta = inner
        .cold
        .as_ref()
        .map(|cold| cold.base.clone())
        .unwrap_or_default();
    meta.extend(&inner.document.jobs, None)?;
    Ok(meta)
}
fn load(
    inner: &mut Inner,
    sequence: u64,
    reference: Option<&Reference>,
) -> MemoryResult<SummaryJob> {
    if let Some(job) = inner
        .document
        .jobs
        .iter()
        .find(|job| job.batch.sequence == sequence)
    {
        return Ok(job.clone());
    }
    let result = (|| {
        let cold = inner.cold.as_ref().ok_or(MemoryError::CorruptState)?;
        let reference = reference.ok_or(MemoryError::CorruptState)?;
        history::read(cold.archive.as_ref(), reference)?
            .jobs
            .into_iter()
            .find(|job| job.batch.sequence == sequence)
            .ok_or(MemoryError::CorruptState)
    })();
    if result.is_err() {
        inner.context = None;
    }
    result
}
fn latest(inner: &mut Inner, scope: &MemoryScope) -> MemoryResult<Option<SummaryJob>> {
    if inner.context.is_none() {
        return Err(MemoryError::Unavailable);
    }
    let cold = inner
        .cold
        .as_ref()
        .and_then(|cold| cold.base.scopes.get(scope))
        .and_then(|scope| scope.latest.clone());
    let hot = inner
        .document
        .jobs
        .iter()
        .filter(|job| job.batch.scope == *scope && job.status == SummaryStatus::Completed)
        .max_by_key(|job| {
            (
                job.batch.source_revision,
                job.repair
                    .as_ref()
                    .map_or(job.batch.sequence, |repair| repair.original_sequence),
            )
        });
    if let Some(hot) = hot
        && cold.as_ref().is_none_or(|cold| {
            (
                hot.batch.source_revision,
                hot.repair
                    .as_ref()
                    .map_or(hot.batch.sequence, |repair| repair.original_sequence),
            ) >= (cold.revision, cold.origin_sequence)
        })
    {
        return Ok(Some(hot.clone()));
    }
    cold.map(|cold| load(inner, cold.sequence, cold.reference.as_ref()))
        .transpose()
}
fn persist(inner: &mut Inner, mut next: Document) -> MemoryResult<()> {
    if inner.context.is_none() {
        return Err(MemoryError::Unavailable);
    }
    let mut verified = inner
        .cold
        .as_ref()
        .map(|cold| cold.base.clone())
        .unwrap_or_default();
    verified.extend(&next.jobs, None)?;
    let mut next_base = None;
    let result = (|| {
        if inner.cold.is_some() && !inner.sealed {
            inner
                .context
                .as_ref()
                .ok_or(MemoryError::Unavailable)?
                .state_set(INITIALIZED_KEY, INITIALIZED.to_vec())
                .map_err(|_| MemoryError::Storage)?;
            inner.sealed = true;
        }
        if let Some(cold) = &inner.cold
            && (next.jobs.len() > 96
                || serde_json::to_vec(&next)
                    .map_err(|_| MemoryError::InvalidInput)?
                    .len()
                    > MAX_STATE_BYTES * 3 / 4)
        {
            let count = next
                .jobs
                .iter()
                .take(64)
                .take_while(|job| job.status != SummaryStatus::Running)
                .count();
            if count != 0 {
                let reference =
                    history::freeze(cold, next.tail.clone(), next.jobs[..count].to_vec())?;
                let mut base = cold.base.clone();
                base.extend(&next.jobs[..count], Some(&reference))?;
                next.jobs.drain(..count);
                next.tail = Some(reference);
                next_base = Some(base);
            }
        }
        if next.jobs.len() > MAX_SUMMARY_JOBS {
            return Err(MemoryError::LimitReached);
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::InvalidInput)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(MemoryError::LimitReached);
        }
        inner
            .context
            .as_ref()
            .ok_or(MemoryError::Unavailable)?
            .state_set(SUMMARY_STATE_KEY, bytes)
            .map_err(|_| MemoryError::Storage)
    })();
    if matches!(
        result,
        Err(MemoryError::Storage | MemoryError::CorruptState)
    ) {
        inner.context = None;
    }
    result?;
    if let Some(base) = next_base {
        inner.cold.as_mut().ok_or(MemoryError::Unavailable)?.base = base;
    }
    inner.document = next;
    Ok(())
}
