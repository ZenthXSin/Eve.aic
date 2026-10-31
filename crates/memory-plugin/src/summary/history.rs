//! 冷历史只保留哈希索引；验证顺序连续，不能拿旧批次副本冒充当前原始证据。
use super::{archive, validation::*};
use eve_memory_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {
    pub hash: String,
    pub bytes: usize,
    pub first: u64,
    pub last: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Chunk {
    pub format_version: u32,
    pub previous: Option<Reference>,
    pub jobs: Vec<SummaryJob>,
}
#[derive(Clone, Default)]
pub(super) struct ScopeIndex {
    pub last_started: Option<u64>,
    pub latest: Option<Completed>,
    pub last_was_repair: bool,
}
#[derive(Clone)]
pub(super) struct Completed {
    pub sequence: u64,
    pub origin_sequence: u64,
    pub revision: u64,
    pub reference: Option<Reference>,
    pub points: Vec<String>,
}
#[derive(Clone)]
pub(super) struct RepairRoot {
    pub sequence: u64,
    pub scope: MemoryScope,
    pub reference: Option<Reference>,
    pub consumed_ids: Vec<String>,
    pub revision: u64,
    pub attempt: u8,
    pub last_started: u64,
    pub last_finished: Option<u64>,
    pub status: SummaryStatus,
}
#[derive(Clone, Default)]
pub(super) struct Metadata {
    pub sequence: u64,
    metadata_bytes: usize,
    pub scopes: BTreeMap<MemoryScope, ScopeIndex>,
    pub consumed: BTreeMap<(MemoryScope, String), u64>,
    sources: BTreeMap<(MemoryScope, String), String>,
    pub repairs: BTreeMap<u64, RepairRoot>,
    running: BTreeSet<MemoryScope>,
    pub provider_failure: Option<u64>,
}
pub(super) fn repairable(status: &SummaryStatus) -> bool {
    matches!(
        status,
        SummaryStatus::Failed(
            SummaryFailure::Provider
                | SummaryFailure::Timeout
                | SummaryFailure::InvalidOutput
                | SummaryFailure::InvalidOutputJson
                | SummaryFailure::InvalidOutputShape
                | SummaryFailure::InvalidOutputSource
                | SummaryFailure::InvalidOutputTooLarge
                | SummaryFailure::UnexpectedTool
        )
    )
}
fn digest<T: Serialize>(value: &T) -> MemoryResult<String> {
    serde_json::to_vec(value)
        .map(|bytes| archive::key(&bytes))
        .map_err(|_| MemoryError::InvalidInput)
}
impl Metadata {
    fn charge(&mut self, bytes: usize) -> MemoryResult<()> {
        self.metadata_bytes = self
            .metadata_bytes
            .checked_add(bytes)
            .ok_or(MemoryError::LimitReached)?;
        if self.metadata_bytes > 16 * 1024 * 1024 {
            return Err(MemoryError::LimitReached);
        }
        Ok(())
    }
    pub fn extend(
        &mut self,
        jobs: &[SummaryJob],
        reference: Option<&Reference>,
    ) -> MemoryResult<()> {
        for job in jobs {
            validate_batch(&job.batch)?;
            validate_summary_points(&job.batch, &job.points)?;
            if self.sequence.checked_add(1) != Some(job.batch.sequence) {
                return Err(MemoryError::CorruptState);
            }
            self.sequence = job.batch.sequence;
            let scope = job.batch.scope.clone();
            let scope_bytes = scope.channel.len() + scope.session_id.len() + scope.user_id.len();
            if !self.scopes.contains_key(&scope) {
                self.charge(scope_bytes + 1024)?;
            }
            for id in &job.batch.consumed_ids {
                if !self.consumed.contains_key(&(scope.clone(), id.clone())) {
                    self.charge(scope_bytes + id.len() + 256)?;
                }
            }
            for source in &job.batch.sources {
                if !self
                    .sources
                    .contains_key(&(scope.clone(), source.evidence_id.clone()))
                {
                    self.charge(scope_bytes + source.evidence_id.len() + 256)?;
                }
            }
            if job.repair.is_none() && repairable(&job.status) {
                self.charge(8192)?;
            }
            let prior = self.scopes.entry(scope.clone()).or_default();
            if prior
                .last_started
                .is_some_and(|at| at > job.batch.started_at_ms)
            {
                return Err(MemoryError::CorruptState);
            }
            for point in &job.batch.previous {
                if !prior.latest.as_ref().is_some_and(|latest| {
                    digest(point).is_ok_and(|hash| latest.points.contains(&hash))
                }) {
                    return Err(MemoryError::CorruptState);
                }
            }
            let root = job
                .repair
                .as_ref()
                .map_or(job.batch.sequence, |repair| repair.original_sequence);
            if let Some(repair) = &job.repair {
                let original = self
                    .repairs
                    .get_mut(&root)
                    .ok_or(MemoryError::CorruptState)?;
                if original.scope != scope
                    || !repairable(&original.status)
                    || repair.attempt == 0
                    || repair.attempt > 2
                    || repair.attempt != original.attempt + 1
                    || job.batch.source_revision != original.revision
                    || job.batch.consumed_ids != original.consumed_ids
                    || !job.batch.previous.is_empty()
                {
                    return Err(MemoryError::CorruptState);
                }
                original.attempt = repair.attempt;
                original.last_started = job.batch.started_at_ms;
                original.last_finished = job.finished_at_ms;
                original.status = job.status.clone();
            }
            for id in &job.batch.consumed_ids {
                let key = (scope.clone(), id.clone());
                match self.consumed.get(&key) {
                    Some(previous) if job.repair.is_some() && *previous == root => {}
                    Some(_) => return Err(MemoryError::CorruptState),
                    None if job.repair.is_none() => {
                        self.consumed.insert(key, root);
                    }
                    None => return Err(MemoryError::CorruptState),
                }
            }
            for source in &job.batch.sources {
                let hash = digest(source)?;
                let key = (scope.clone(), source.evidence_id.clone());
                if self.sources.get(&key).is_some_and(|old| *old != hash) {
                    return Err(MemoryError::CorruptState);
                }
                self.sources.insert(key, hash);
            }
            match &job.status {
                SummaryStatus::Running | SummaryStatus::Interrupted => {
                    if !job.points.is_empty()
                        || job.finished_at_ms.is_some()
                        || (job.status == SummaryStatus::Running
                            && !self.running.insert(scope.clone()))
                    {
                        return Err(MemoryError::CorruptState);
                    }
                }
                SummaryStatus::Completed | SummaryStatus::Failed(_) => {
                    if job
                        .finished_at_ms
                        .is_none_or(|at| at < job.batch.started_at_ms)
                        || (job.status != SummaryStatus::Completed && !job.points.is_empty())
                    {
                        return Err(MemoryError::CorruptState);
                    }
                }
            }
            if job.repair.is_none() && repairable(&job.status) {
                self.repairs.insert(
                    root,
                    RepairRoot {
                        sequence: root,
                        scope: scope.clone(),
                        reference: reference.cloned(),
                        consumed_ids: job.batch.consumed_ids.clone(),
                        revision: job.batch.source_revision,
                        attempt: 0,
                        last_started: job.batch.started_at_ms,
                        last_finished: job.finished_at_ms,
                        status: job.status.clone(),
                    },
                );
            }
            if matches!(
                job.status,
                SummaryStatus::Failed(SummaryFailure::Provider | SummaryFailure::Timeout)
            ) {
                self.provider_failure = self.provider_failure.max(job.finished_at_ms);
            }
            if job.status == SummaryStatus::Completed
                && prior.latest.as_ref().is_none_or(|latest| {
                    (job.batch.source_revision, root) >= (latest.revision, latest.origin_sequence)
                })
            {
                prior.latest = Some(Completed {
                    sequence: job.batch.sequence,
                    origin_sequence: root,
                    revision: job.batch.source_revision,
                    reference: reference.cloned(),
                    points: job.points.iter().map(digest).collect::<MemoryResult<_>>()?,
                });
            }
            prior.last_started = Some(job.batch.started_at_ms);
            prior.last_was_repair = job.repair.is_some();
        }
        Ok(())
    }
}
pub(super) struct Cold {
    pub archive: Arc<dyn SummaryArchiveStore>,
    pub base: Metadata,
}
pub(super) fn read(store: &dyn SummaryArchiveStore, reference: &Reference) -> MemoryResult<Chunk> {
    if !archive::valid(&reference.hash)
        || reference.bytes > MAX_SUMMARY_ARCHIVE_BYTES
        || reference.first == 0
        || reference.last < reference.first
    {
        return Err(MemoryError::CorruptState);
    }
    let bytes = store.get(&reference.hash)?;
    if bytes.len() != reference.bytes || archive::key(&bytes) != reference.hash {
        return Err(MemoryError::CorruptState);
    }
    let chunk: Chunk = serde_json::from_value(
        crate::strict_json::from_slice(&bytes).map_err(|_| MemoryError::CorruptState)?,
    )
    .map_err(|_| MemoryError::CorruptState)?;
    if chunk.format_version != 1
        || chunk.jobs.is_empty()
        || chunk.jobs.len() > 64
        || chunk.jobs.first().map(|j| j.batch.sequence) != Some(reference.first)
        || chunk.jobs.last().map(|j| j.batch.sequence) != Some(reference.last)
        || chunk
            .jobs
            .iter()
            .any(|job| job.status == SummaryStatus::Running)
    {
        return Err(MemoryError::CorruptState);
    }
    Ok(chunk)
}
pub(super) fn references(
    store: &dyn SummaryArchiveStore,
    tail: Option<&Reference>,
) -> MemoryResult<Vec<Reference>> {
    let mut next = tail.cloned();
    let mut result = Vec::new();
    while let Some(reference) = next {
        if result.len() >= 65536 {
            return Err(MemoryError::LimitReached);
        }
        let chunk = read(store, &reference)?;
        if chunk
            .previous
            .as_ref()
            .map_or(reference.first != 1, |previous| {
                previous.last.checked_add(1) != Some(reference.first)
            })
        {
            return Err(MemoryError::CorruptState);
        }
        next = chunk.previous;
        result.push(reference);
    }
    result.reverse();
    Ok(result)
}
pub(super) fn open(
    archive: Arc<dyn SummaryArchiveStore>,
    tail: Option<&Reference>,
) -> MemoryResult<Cold> {
    let mut base = Metadata::default();
    for reference in references(archive.as_ref(), tail)? {
        base.extend(&read(archive.as_ref(), &reference)?.jobs, Some(&reference))?;
    }
    Ok(Cold { archive, base })
}
pub(super) fn freeze(
    cold: &Cold,
    previous: Option<Reference>,
    jobs: Vec<SummaryJob>,
) -> MemoryResult<Reference> {
    let first = jobs
        .first()
        .ok_or(MemoryError::InvalidInput)?
        .batch
        .sequence;
    let last = jobs.last().ok_or(MemoryError::InvalidInput)?.batch.sequence;
    let bytes = serde_json::to_vec(&Chunk {
        format_version: 1,
        previous,
        jobs,
    })
    .map_err(|_| MemoryError::InvalidInput)?;
    if bytes.len() > MAX_SUMMARY_ARCHIVE_BYTES {
        return Err(MemoryError::LimitReached);
    }
    let reference = Reference {
        hash: archive::key(&bytes),
        bytes: bytes.len(),
        first,
        last,
    };
    cold.archive.put(&reference.hash, &bytes)?;
    read(cold.archive.as_ref(), &reference)?;
    Ok(reference)
}
