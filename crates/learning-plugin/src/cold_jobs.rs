//! 批次终态只更新目标冷记录；完整校验仍先于任何写入。
use super::*;

pub(super) fn finish(
    inner: &mut Inner,
    batch: &LearningBatch,
    at_ms: u64,
    outcome: LearningOutcome,
) -> LearningResult<()> {
    let mut selected = None;
    if let Err(error) = cold_startup::scan(inner, |position, value| {
        if value
            .get("job")
            .and_then(|job| job.get("batch"))
            .and_then(|batch| batch.get("id"))
            .and_then(serde_json::Value::as_str)
            == Some(&batch.id)
        {
            let record: JobRecord =
                serde_json::from_value(value.clone()).map_err(|_| LearningError::CorruptState)?;
            selected = Some((position, record));
        }
        Ok(())
    }) {
        inner.context = None;
        evict(inner);
        return Err(error);
    }
    let (position, old) = selected.ok_or(LearningError::Conflict)?;
    let Some(record) = finish_record(&old, batch, at_ms, outcome)? else {
        return Ok(());
    };
    put_job(inner, Some(position), record)
}

fn put_job(inner: &mut Inner, position: Option<usize>, record: JobRecord) -> LearningResult<()> {
    let mut next = inner
        .job_index
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .clone();
    let total = cold_decisions::index_total(&next, "jobs")?;
    let body = serde_json::to_vec(&record).map_err(|_| LearningError::InvalidInput)?;
    if body.len() > MAX_ARCHIVE_RECORD_BYTES
        || total
            .checked_sub(position.map_or(0, |position| next.records[position].bytes))
            .and_then(|n| n.checked_add(body.len()))
            .is_none_or(|n| n > MAX_ARCHIVE_RESTORE_BYTES)
    {
        return Err(LearningError::LimitReached);
    }
    let hash = archive::key(&body);
    let reference = archive::RecordRef {
        hash: hash.clone(),
        bytes: body.len(),
    };
    if let Some(position) = position {
        next.records[position] = reference;
    } else {
        next.records.push(reference);
    }
    let bytes = serde_json::to_vec(&next).map_err(|_| LearningError::InvalidInput)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(LearningError::LimitReached);
    }
    let revision = inner
        .change
        .revision
        .checked_add(1)
        .ok_or(LearningError::LimitReached)?;
    inner
        .archive
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .put(&hash, &body)?;
    if inner
        .context
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .state_set(LEARNING_STATE_KEY, bytes)
        .is_err()
    {
        inner.context = None;
        evict(inner);
        return Err(LearningError::Storage);
    }
    inner.job_index = Some(next);
    inner.change.revision = revision;
    inner.scope_changes.insert(record.job.batch.scope, revision);
    Ok(())
}

fn source_hash(evidence: &eve_memory_api::InteractionEvidence) -> LearningResult<[u8; 32]> {
    let bytes = serde_json::to_vec(evidence).map_err(|_| LearningError::InvalidInput)?;
    let mut hash = [0; 32];
    hash.copy_from_slice(digest(&SHA256, &bytes).as_ref());
    Ok(hash)
}
fn checked_scan(
    inner: &mut Inner,
    visit: impl FnMut(usize, &serde_json::Value) -> LearningResult<()>,
) -> LearningResult<()> {
    if let Err(error) = cold_startup::scan(inner, visit) {
        inner.context = None;
        evict(inner);
        return Err(error);
    }
    Ok(())
}

pub(super) fn reserve_reader(
    inner: &mut Inner,
    scope: &MemoryScope,
    reader: &dyn eve_memory_api::MemoryService,
    now_ms: u64,
    extractor_version: &str,
    options: &LearningOptions,
) -> LearningResult<Option<LearningBatch>> {
    use eve_memory_api::{MemoryEvidenceCursor, ValidatedMemoryCursor};
    let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
    let revision = cursor.summary().revision;
    let mut expected = BTreeMap::new();
    let mut last_start = None;
    let mut last_revision = 0;
    checked_scan(inner, |_, value| {
        let record: JobRecord =
            serde_json::from_value(value.clone()).map_err(|_| LearningError::CorruptState)?;
        if record.job.batch.scope == *scope {
            last_start = Some(last_start.unwrap_or(0).max(record.job.batch.started_at_ms));
            last_revision = last_revision.max(record.memory_revision);
            for evidence in &record.job.batch.evidence {
                expected.insert(evidence.id.clone(), source_hash(evidence)?);
            }
        }
        Ok(())
    })?;
    if revision < last_revision {
        return Err(LearningError::Conflict);
    }
    let cooling = last_start.is_some_and(|start| {
        now_ms
            .checked_sub(start)
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
            if let Some(prior) = expected.get(&evidence.id) {
                if *prior != source_hash(&evidence)? {
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
    batch.id = batch_id(scope, extractor_version, &batch.evidence)?;
    let record = JobRecord {
        memory_revision: revision,
        previous_attempts: vec![],
        reflection: None,
        job: LearningJob {
            batch: batch.clone(),
            status: JobStatus::Running,
            finished_at_ms: None,
            candidates: vec![],
        },
    };
    put_job(inner, None, record)?;
    Ok(Some(batch))
}

pub(super) fn reserve_repair_reader(
    inner: &mut Inner,
    scope: &MemoryScope,
    reader: &dyn eve_memory_api::MemoryService,
    now_ms: u64,
    extractor_version: &str,
) -> LearningResult<Option<LearningBatch>> {
    use eve_memory_api::{MemoryEvidenceCursor, ValidatedMemoryCursor};
    let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
    let revision = cursor.summary().revision;
    let mut selected = None;
    let mut busy = false;
    checked_scan(inner, |position, value| {
        let record: JobRecord =
            serde_json::from_value(value.clone()).map_err(|_| LearningError::CorruptState)?;
        if record.job.batch.scope == *scope {
            busy |= record.job.status == JobStatus::Running;
            if selected.is_none()
                && record.job.batch.extractor_version == extractor_version
                && retryable(&record.job.status)
                && record.previous_attempts.len() < MAX_REPAIRS
                && now_ms
                    .checked_sub(
                        record
                            .job
                            .finished_at_ms
                            .unwrap_or(record.job.batch.started_at_ms),
                    )
                    .is_some_and(|elapsed| elapsed >= REPAIR_COOLDOWN_MS)
            {
                selected = Some((position, record));
            }
        }
        Ok(())
    })?;
    if busy {
        selected = None;
    }
    let mut expected = BTreeMap::new();
    if let Some((_, record)) = &selected {
        if revision < record.memory_revision {
            return Err(LearningError::Conflict);
        }
        for evidence in &record.job.batch.evidence {
            expected.insert(evidence.id.clone(), source_hash(evidence)?);
        }
    }
    let mut seen = BTreeSet::new();
    loop {
        let page = cursor.next_page()?;
        if page.is_empty() {
            break;
        }
        for evidence in page {
            if let Some(prior) = expected.get(&evidence.id) {
                if *prior != source_hash(&evidence)? {
                    return Err(LearningError::Conflict);
                }
                seen.insert(evidence.id.clone());
            }
        }
    }
    if seen.len() != expected.len() {
        return Err(LearningError::Conflict);
    }
    let Some((position, mut record)) = selected else {
        return Ok(None);
    };
    record.previous_attempts.push(MaintenanceAttempt {
        started_at_ms: record.job.batch.started_at_ms,
        finished_at_ms: record.job.finished_at_ms,
        status: record.job.status.clone(),
    });
    record.job.batch.started_at_ms = now_ms;
    record.job.batch.attempt =
        u8::try_from(record.previous_attempts.len()).map_err(|_| LearningError::InvalidInput)?;
    record.job.status = JobStatus::Running;
    record.job.finished_at_ms = None;
    let batch = record.job.batch.clone();
    put_job(inner, Some(position), record)?;
    Ok(Some(batch))
}
