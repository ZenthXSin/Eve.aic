use super::{SUMMARY_STATE_KEY, validation::*};
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
}
struct Inner {
    document: Document,
    context: Option<PluginContext>,
}
pub(super) struct StoredSummaries {
    inner: Mutex<Inner>,
}
impl StoredSummaries {
    pub(super) fn last_provider_failure_at(&self) -> MemoryResult<Option<u64>> {
        let inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        Ok(inner
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
            .max())
    }
    pub(super) fn open(context: PluginContext) -> MemoryResult<Self> {
        let mut document = match context
            .state_get(SUMMARY_STATE_KEY)
            .map_err(|_| MemoryError::Storage)?
        {
            None => Document {
                format_version: 1,
                jobs: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(MemoryError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| MemoryError::CorruptState)?;
                if value.get("format_version").and_then(|v| v.as_u64()) != Some(1) {
                    return Err(MemoryError::UnsupportedVersion);
                }
                let document: Document =
                    serde_json::from_value(value).map_err(|_| MemoryError::CorruptState)?;
                validate_document(&document).map_err(|_| MemoryError::CorruptState)?;
                document
            }
        };
        let mut changed = false;
        for job in &mut document.jobs {
            if job.status == SummaryStatus::Running {
                job.status = SummaryStatus::Interrupted;
                changed = true;
            }
        }
        if changed {
            context
                .state_set(
                    SUMMARY_STATE_KEY,
                    serde_json::to_vec(&document).map_err(|_| MemoryError::Storage)?,
                )
                .map_err(|_| MemoryError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                document,
                context: Some(context),
            }),
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
        let inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        Ok(inner
            .document
            .jobs
            .iter()
            .filter(|j| &j.batch.scope == scope)
            .cloned()
            .collect())
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
        let jobs: Vec<_> = inner
            .document
            .jobs
            .iter()
            .filter(|j| j.batch.scope == snapshot.scope)
            .collect();
        if jobs.iter().any(|j| j.status == SummaryStatus::Running)
            || jobs.last().is_some_and(|j| {
                now.saturating_sub(j.batch.started_at_ms) < options.cooldown_ms
                    || now < j.batch.started_at_ms
            })
        {
            return Ok(None);
        }
        let consumed: BTreeSet<_> = jobs
            .iter()
            .flat_map(|j| j.batch.consumed_ids.iter())
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
        if inner.document.jobs.len() >= MAX_SUMMARY_JOBS {
            return Err(MemoryError::LimitReached);
        }
        let mut previous = jobs
            .iter()
            .rev()
            .find(|j| j.status == SummaryStatus::Completed)
            .map(|j| effective_points(j, snapshot))
            .transpose()?
            .unwrap_or_default();
        let mut new: Vec<_> = pending.into_iter().take(options.max_new_evidence).collect();
        let batch = loop {
            let referenced: BTreeSet<_> = previous
                .iter()
                .flat_map(|p| p.quotes.iter().map(|q| &q.evidence_id))
                .collect();
            let new_ids: Vec<_> = new.iter().map(|s| s.evidence_id.clone()).collect();
            let sources = snapshot
                .evidence
                .iter()
                .filter(|e| referenced.contains(&e.id) || new_ids.contains(&e.id))
                .filter_map(user_source)
                .collect();
            let batch = SummaryBatch {
                sequence: inner.document.jobs.len() as u64 + 1,
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
fn persist(inner: &mut Inner, next: Document) -> MemoryResult<()> {
    validate_document(&next)?;
    let bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::InvalidInput)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(MemoryError::LimitReached);
    }
    if inner
        .context
        .as_ref()
        .ok_or(MemoryError::Unavailable)?
        .state_set(SUMMARY_STATE_KEY, bytes)
        .is_err()
    {
        inner.context = None;
        return Err(MemoryError::Storage);
    }
    inner.document = next;
    Ok(())
}
fn validate_document(document: &Document) -> MemoryResult<()> {
    if document.format_version != 1 || document.jobs.len() > MAX_SUMMARY_JOBS {
        return Err(MemoryError::InvalidInput);
    }
    let mut consumed = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut latest: BTreeMap<&MemoryScope, &SummaryJob> = BTreeMap::new();
    let mut running = BTreeSet::new();
    for (index, job) in document.jobs.iter().enumerate() {
        validate_batch(&job.batch)?;
        validate_summary_points(&job.batch, &job.points)?;
        if job.batch.sequence != index as u64 + 1
            || job
                .batch
                .consumed_ids
                .iter()
                .any(|id| !consumed.insert((&job.batch.scope, id)))
        {
            return Err(MemoryError::InvalidInput);
        }
        for point in &job.batch.previous {
            if !latest
                .get(&job.batch.scope)
                .is_some_and(|j| j.points.contains(point))
            {
                return Err(MemoryError::InvalidInput);
            }
        }
        for source in &job.batch.sources {
            if let Some(old) = sources.insert((&job.batch.scope, &source.evidence_id), source)
                && old != source
            {
                return Err(MemoryError::InvalidInput);
            }
        }
        match job.status {
            SummaryStatus::Running | SummaryStatus::Interrupted => {
                if !job.points.is_empty() || job.finished_at_ms.is_some() {
                    return Err(MemoryError::InvalidInput);
                }
                if job.status == SummaryStatus::Running && !running.insert(&job.batch.scope) {
                    return Err(MemoryError::InvalidInput);
                }
            }
            SummaryStatus::Completed | SummaryStatus::Failed(_) => {
                if job
                    .finished_at_ms
                    .is_none_or(|t| t < job.batch.started_at_ms)
                    || (job.status != SummaryStatus::Completed && !job.points.is_empty())
                {
                    return Err(MemoryError::InvalidInput);
                }
            }
        }
        if job.status == SummaryStatus::Completed {
            latest.insert(&job.batch.scope, job);
        }
    }
    Ok(())
}
