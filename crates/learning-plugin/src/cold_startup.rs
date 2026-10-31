//! 启动逐条验证两本账；全部通过后只重写实际中断的批次，不恢复整本冷正文。
use super::cold_decisions::{Check, index_total};
use super::*;

pub(super) fn validate_and_recover(inner: &mut Inner) -> LearningResult<()> {
    let archive = inner
        .archive
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .clone();
    let mut check = Check::default();
    let mut running = Vec::new();
    if let Some(index) = &inner.job_index {
        index_total(index, "jobs")?;
        for (position, reference) in index.records.iter().enumerate() {
            let value = archive::load_value(archive.as_ref(), reference)?;
            let is_running = value
                .get("job")
                .and_then(|job| job.get("status"))
                .and_then(serde_json::Value::as_str)
                == Some("Running");
            check.job(value).map_err(|_| LearningError::CorruptState)?;
            if is_running {
                running.push(position);
            }
        }
    } else {
        for record in &inner.document.jobs {
            check
                .job(serde_json::to_value(record).map_err(|_| LearningError::CorruptState)?)
                .map_err(|_| LearningError::CorruptState)?;
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
    // 跨条目身份/候选绑定元数据不再参与恢复，先释放再写冷文件。
    drop(check);
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
