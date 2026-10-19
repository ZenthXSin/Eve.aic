use eve_memory_api::*;
use std::collections::BTreeSet;

pub(crate) fn validate_batch(batch: &SummaryBatch) -> MemoryResult<()> {
    batch.scope.validate()?;
    validate_id(&batch.extractor_version)?;
    if batch.sequence == 0
        || batch.source_revision == 0
        || batch.sources.is_empty()
        || batch.sources.len() > 26
        || batch.consumed_ids.is_empty()
        || batch.consumed_ids.len() > 8
    {
        return Err(MemoryError::InvalidInput);
    }
    let mut ids = BTreeSet::new();
    let mut revision = 0;
    for source in &batch.sources {
        validate_id(&source.evidence_id)?;
        validate_text(&source.user_text, MAX_TEXT_BYTES)?;
        if source.revision <= revision
            || source.revision > batch.source_revision
            || !ids.insert(&source.evidence_id)
        {
            return Err(MemoryError::InvalidInput);
        }
        revision = source.revision;
    }
    let consumed: BTreeSet<_> = batch.consumed_ids.iter().collect();
    if consumed.len() != batch.consumed_ids.len() || consumed.iter().any(|id| !ids.contains(*id)) {
        return Err(MemoryError::InvalidInput);
    }
    validate_summary_points(batch, &batch.previous)?;
    if serde_json::to_vec(batch)
        .map_err(|_| MemoryError::InvalidInput)?
        .len()
        > MAX_SUMMARY_INPUT_BYTES
    {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}

/// 校验逐字用户引文；引文存在不证明模型陈述已被语义核实。
pub fn validate_summary_points(batch: &SummaryBatch, points: &[SummaryPoint]) -> MemoryResult<()> {
    if points.len() > MAX_SUMMARY_POINTS
        || serde_json::to_vec(points)
            .map_err(|_| MemoryError::InvalidInput)?
            .len()
            > MAX_SUMMARY_OUTPUT_BYTES
    {
        return Err(MemoryError::InvalidInput);
    }
    let mut texts = BTreeSet::new();
    for point in points {
        validate_text(&point.text, 1024)?;
        if point.quotes.is_empty() || point.quotes.len() > 3 || !texts.insert(&point.text) {
            return Err(MemoryError::InvalidInput);
        }
        let mut quotes = BTreeSet::new();
        for quote in &point.quotes {
            validate_text(&quote.text, 512)?;
            if quote.text.chars().filter(|c| !c.is_whitespace()).count() < 3
                || !quotes.insert((&quote.evidence_id, &quote.text))
                || !batch.sources.iter().any(|source| {
                    source.evidence_id == quote.evidence_id
                        && source.user_text.contains(&quote.text)
                })
            {
                return Err(MemoryError::InvalidInput);
            }
        }
    }
    Ok(())
}

pub(crate) fn user_source(evidence: &InteractionEvidence) -> Option<SummarySource> {
    if let EvidenceSource::CompletedInteraction { user_text, .. } = &evidence.source {
        Some(SummarySource {
            evidence_id: evidence.id.clone(),
            revision: evidence.revision,
            at_ms: evidence.at_ms,
            user_text: user_text.clone(),
        })
    } else {
        None
    }
}

/// 读取当前来源后再提供派生观点；修正、撤销的旧偏好来源不通过摘要复活。
pub fn effective_points(
    job: &SummaryJob,
    snapshot: &MemorySnapshot,
) -> MemoryResult<Vec<SummaryPoint>> {
    if job.batch.scope != snapshot.scope || job.batch.source_revision > snapshot.revision {
        return Err(MemoryError::StaleRevision);
    }
    if job.status != SummaryStatus::Completed {
        return Ok(vec![]);
    }
    validate_batch(&job.batch)?;
    validate_summary_points(&job.batch, &job.points)?;
    for source in &job.batch.sources {
        let hot = snapshot
            .evidence
            .iter()
            .any(|e| user_source(e).as_ref() == Some(source));
        if !hot && !archived_source(snapshot, source) {
            return Err(MemoryError::Conflict);
        }
    }
    let mut blocked = BTreeSet::new();
    for preference in &snapshot.preferences {
        let current = preference.history.last();
        for version in &preference.history {
            if preference.status == PreferenceStatus::Revoked || Some(version) != current {
                blocked.insert(&version.evidence_id);
            }
        }
    }
    Ok(job
        .points
        .iter()
        .filter(|point| {
            point
                .quotes
                .iter()
                .all(|q| !blocked.contains(&q.evidence_id))
        })
        .cloned()
        .collect())
}

/// 不在热层的摘要来源是否可能已移入记忆归档；热层同 ID 或同修订的不同内容仍是冲突。
pub(crate) fn archived_source(snapshot: &MemorySnapshot, source: &SummarySource) -> bool {
    snapshot.archived_evidence > 0
        && source.revision > 0
        && source.revision <= snapshot.revision
        && !snapshot
            .evidence
            .iter()
            .any(|e| e.id == source.evidence_id || e.revision == source.revision)
}
