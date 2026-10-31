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
    let mut next = inner
        .job_index
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .clone();
    let total = cold_decisions::index_total(&next, "jobs")?;
    let body = serde_json::to_vec(&record).map_err(|_| LearningError::InvalidInput)?;
    if body.len() > MAX_ARCHIVE_RECORD_BYTES
        || total
            .checked_sub(next.records[position].bytes)
            .and_then(|n| n.checked_add(body.len()))
            .is_none_or(|n| n > MAX_ARCHIVE_RESTORE_BYTES)
    {
        return Err(LearningError::LimitReached);
    }
    let hash = archive::key(&body);
    next.records[position] = archive::RecordRef {
        hash: hash.clone(),
        bytes: body.len(),
    };
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
    inner.scope_changes.insert(batch.scope.clone(), revision);
    Ok(())
}
