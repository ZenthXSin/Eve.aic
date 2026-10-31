//! 启动与收尾逐条核验两本账；不恢复整本冷正文。
use super::cold_decisions::{Check, index_total};
use super::*;

pub(super) fn scan(
    inner: &Inner,
    mut visit: impl FnMut(usize, &serde_json::Value) -> LearningResult<()>,
) -> LearningResult<()> {
    let archive = inner.archive.as_ref().ok_or(LearningError::Unavailable)?;
    let mut check = Check::default();
    if let Some(index) = &inner.job_index {
        index_total(index, "jobs")?;
        for (position, reference) in index.records.iter().enumerate() {
            let value = archive::load_value(archive.as_ref(), reference)?;
            visit(position, &value)?;
            check.job(value).map_err(|_| LearningError::CorruptState)?;
        }
    } else {
        for (position, record) in inner.document.jobs.iter().enumerate() {
            let value = serde_json::to_value(record).map_err(|_| LearningError::CorruptState)?;
            visit(position, &value)?;
            check.job(value).map_err(|_| LearningError::CorruptState)?;
        }
    }
    if let Some(index) = &inner.decision_index {
        index_total(index, "decisions")?;
        for reference in &index.records {
            check
                .decision(archive::load_value(archive.as_ref(), reference)?, None)
                .map_err(|_| LearningError::CorruptState)?;
        }
    } else {
        for record in &inner.decisions.records {
            check
                .decision(
                    serde_json::to_value(record).map_err(|_| LearningError::CorruptState)?,
                    None,
                )
                .map_err(|_| LearningError::CorruptState)?;
        }
    }
    Ok(())
}

pub(super) fn validate_and_recover(inner: &mut Inner) -> LearningResult<()> {
    let archive = inner
        .archive
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .clone();
    let mut running = Vec::new();
    scan(inner, |position, value| {
        if value
            .get("job")
            .and_then(|job| job.get("status"))
            .and_then(serde_json::Value::as_str)
            == Some("Running")
        {
            running.push(position);
        }
        Ok(())
    })?;
    if let Some(index) = &inner.job_index {
        if running.is_empty() {
            return Ok(());
        }
        let mut next = index.clone();
        let mut total = index_total(&next, "jobs")?;
        let revision = inner
            .change
            .revision
            .checked_add(1)
            .ok_or(LearningError::LimitReached)?;
        let mut scopes = BTreeSet::new();
        for position in running {
            let old = &next.records[position];
            let mut record: JobRecord =
                serde_json::from_value(archive::load_value(archive.as_ref(), old)?)
                    .map_err(|_| LearningError::CorruptState)?;
            if record.job.status != JobStatus::Running {
                return Err(LearningError::CorruptState);
            }
            record.job.status = JobStatus::Interrupted;
            let body = serde_json::to_vec(&record).map_err(|_| LearningError::InvalidInput)?;
            total = total
                .checked_sub(old.bytes)
                .and_then(|n| n.checked_add(body.len()))
                .ok_or(LearningError::LimitReached)?;
            if body.len() > MAX_ARCHIVE_RECORD_BYTES || total > MAX_ARCHIVE_RESTORE_BYTES {
                return Err(LearningError::LimitReached);
            }
            let hash = archive::key(&body);
            archive.put(&hash, &body)?;
            scopes.insert(record.job.batch.scope);
            next.records[position] = archive::RecordRef {
                hash,
                bytes: body.len(),
            };
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| LearningError::InvalidInput)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(LearningError::LimitReached);
        }
        if inner
            .context
            .as_ref()
            .ok_or(LearningError::Unavailable)?
            .state_set(LEARNING_STATE_KEY, bytes)
            .is_err()
        {
            inner.context = None;
            return Err(LearningError::Storage);
        }
        inner.job_index = Some(next);
        inner.change.revision = revision;
        for scope in scopes {
            inner.scope_changes.insert(scope, revision);
        }
    } else {
        let mut next = inner.document.clone();
        let mut changed = false;
        for record in &mut next.jobs {
            if record.job.status == JobStatus::Running {
                record.job.status = JobStatus::Interrupted;
                changed = true;
            }
        }
        if changed {
            persist(inner, next)?;
        }
    }
    Ok(())
}
