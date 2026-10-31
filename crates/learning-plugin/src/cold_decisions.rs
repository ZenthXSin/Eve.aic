//! 已是双冷索引的决策追加：逐条核验全部账本，不恢复全部学习正文或重写旧冷记录。
use super::*;

struct CandidateBinding {
    memory_revision: u64,
    created_at_ms: u64,
    evidence_ids: Vec<String>,
}
#[derive(Default)]
pub(super) struct Check {
    job_ids: BTreeSet<String>,
    ids: BTreeSet<(MemoryScope, String)>,
    messages: BTreeSet<(MemoryScope, String)>,
    turns: BTreeSet<(MemoryScope, Option<u64>)>,
    revisions: BTreeSet<(MemoryScope, u64)>,
    previous: BTreeMap<MemoryScope, (u64, u64)>,
    candidates: BTreeMap<(MemoryScope, String, String), CandidateBinding>,
    sequences: BTreeMap<MemoryScope, u64>,
    outcomes: BTreeMap<(MemoryScope, String), [u8; 32]>,
    selected: Option<LearningDecisionRecord>,
}
pub(super) fn index_total(index: &Index, kind: &str) -> LearningResult<usize> {
    if index.format_version != 2 || index.kind != kind {
        return Err(LearningError::CorruptState);
    }
    let count = index
        .records
        .iter()
        .try_fold(0usize, |n, r| n.checked_add(r.bytes))
        .ok_or(LearningError::LimitReached)?;
    if count > MAX_ARCHIVE_RESTORE_BYTES {
        return Err(LearningError::LimitReached);
    }
    Ok(count)
}
fn outcome(decision: &LearningDecision) -> LearningResult<[u8; 32]> {
    // 与 same_outcome 完全相同的字段；不把读取修订变化当作新的决定。
    let bytes = serde_json::to_vec(&(
        &decision.candidate_id,
        &decision.batch_id,
        &decision.policy_version,
        &decision.evidence_ids,
        &decision.action,
        &decision.reason,
    ))
    .map_err(|_| LearningError::InvalidInput)?;
    let mut result = [0; 32];
    result.copy_from_slice(digest(&SHA256, &bytes).as_ref());
    Ok(result)
}
impl Check {
    pub(super) fn job(&mut self, value: serde_json::Value) -> LearningResult<()> {
        let record: JobRecord =
            serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
        let one = Document {
            format_version: 1,
            jobs: vec![record],
        };
        validate_document(&one, true)?;
        let record = &one.jobs[0];
        let job = &record.job;
        let batch = &job.batch;
        if !self.job_ids.insert(batch.id.clone()) {
            return Err(LearningError::CorruptState);
        }
        let first_start = record
            .previous_attempts
            .first()
            .map_or(batch.started_at_ms, |a| a.started_at_ms);
        if let Some((started, revision)) = self
            .previous
            .insert(batch.scope.clone(), (first_start, record.memory_revision))
            && (first_start < started || record.memory_revision < revision)
        {
            return Err(LearningError::CorruptState);
        }
        for source in &batch.evidence {
            if !self.ids.insert((batch.scope.clone(), source.id.clone()))
                || !self
                    .messages
                    .insert((batch.scope.clone(), source_message(source).to_owned()))
                || !self
                    .turns
                    .insert((batch.scope.clone(), source_turn(source)))
                || !self
                    .revisions
                    .insert((batch.scope.clone(), source.revision))
            {
                return Err(LearningError::CorruptState);
            }
        }
        for candidate in &job.candidates {
            self.candidates.insert(
                (batch.scope.clone(), batch.id.clone(), candidate.id.clone()),
                CandidateBinding {
                    memory_revision: record.memory_revision,
                    created_at_ms: candidate.created_at_ms,
                    evidence_ids: candidate.draft.evidence_ids.clone(),
                },
            );
        }
        Ok(())
    }
    fn binding(
        &self,
        scope: &MemoryScope,
        decision: &LearningDecision,
        at_ms: u64,
    ) -> LearningResult<()> {
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        decision.validate()?;
        let candidate = self
            .candidates
            .get(&(
                scope.clone(),
                decision.batch_id.clone(),
                decision.candidate_id.clone(),
            ))
            .ok_or(LearningError::Conflict)?;
        if decision.memory_revision < candidate.memory_revision
            || decision.evidence_ids != candidate.evidence_ids
            || at_ms < candidate.created_at_ms
        {
            return Err(LearningError::Conflict);
        }
        Ok(())
    }
    pub(super) fn decision(
        &mut self,
        value: serde_json::Value,
        target: Option<(&MemoryScope, &str)>,
    ) -> LearningResult<()> {
        let entry: ScopedDecisionRecord =
            serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
        let record = entry.record;
        self.binding(&entry.scope, &record.decision, record.at_ms)?;
        let previous = self.sequences.get(&entry.scope).copied().unwrap_or(0);
        if record.sequence != previous.checked_add(1).ok_or(LearningError::CorruptState)? {
            return Err(LearningError::CorruptState);
        }
        self.sequences.insert(entry.scope.clone(), record.sequence);
        let current = outcome(&record.decision)?;
        if self.outcomes.insert(
            (entry.scope.clone(), record.decision.candidate_id.clone()),
            current,
        ) == Some(current)
        {
            return Err(LearningError::CorruptState);
        }
        if target.is_some_and(|(scope, candidate)| {
            entry.scope == *scope && record.decision.candidate_id == candidate
        }) {
            self.selected = Some(record);
        }
        Ok(())
    }
}

pub(super) fn append(
    inner: &mut Inner,
    scope: &MemoryScope,
    decision: LearningDecision,
    at_ms: u64,
) -> LearningResult<LearningDecisionRecord> {
    let archive = inner
        .archive
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .clone();
    let checked = (|| {
        let jobs = inner.job_index.as_ref().ok_or(LearningError::Unavailable)?;
        let decisions = inner
            .decision_index
            .as_ref()
            .ok_or(LearningError::Unavailable)?;
        index_total(jobs, "jobs")?;
        let total = index_total(decisions, "decisions")?;
        let mut check = Check::default();
        for reference in &jobs.records {
            check.job(archive::load_value(archive.as_ref(), reference)?)?;
        }
        for reference in &decisions.records {
            check.decision(
                archive::load_value(archive.as_ref(), reference)?,
                Some((scope, &decision.candidate_id)),
            )?;
        }
        Ok((check, total))
    })();
    let (check, total) = match checked {
        Ok(value) => value,
        Err(error) => {
            inner.context = None;
            evict(inner);
            return Err(error);
        }
    };
    check.binding(scope, &decision, at_ms)?;
    if let Some(previous) = check.selected
        && previous.decision.same_outcome(&decision)
    {
        return Ok(previous);
    }
    let sequence = check
        .sequences
        .get(scope)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(LearningError::LimitReached)?;
    let record = LearningDecisionRecord {
        sequence,
        at_ms,
        decision,
    };
    let entry = ScopedDecisionRecord {
        scope: scope.clone(),
        record: record.clone(),
    };
    let body = serde_json::to_vec(&entry).map_err(|_| LearningError::InvalidInput)?;
    if body.len() > MAX_ARCHIVE_RECORD_BYTES
        || total
            .checked_add(body.len())
            .is_none_or(|n| n > MAX_ARCHIVE_RESTORE_BYTES)
    {
        return Err(LearningError::LimitReached);
    }
    let hash = archive::key(&body);
    let mut next = inner
        .decision_index
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .clone();
    next.records.push(archive::RecordRef {
        hash: hash.clone(),
        bytes: body.len(),
    });
    let bytes = serde_json::to_vec(&next).map_err(|_| LearningError::InvalidInput)?;
    if bytes.len() > MAX_DECISIONS_STATE_BYTES {
        return Err(LearningError::LimitReached);
    }
    let revision = inner
        .change
        .revision
        .checked_add(1)
        .ok_or(LearningError::LimitReached)?;
    archive.put(&hash, &body)?;
    if inner
        .context
        .as_ref()
        .ok_or(LearningError::Unavailable)?
        .state_set(DECISIONS_STATE_KEY, bytes)
        .is_err()
    {
        inner.context = None;
        evict(inner);
        return Err(LearningError::Storage);
    }
    inner.decision_index = Some(next);
    inner.change.revision = revision;
    inner.scope_changes.insert(scope.clone(), revision);
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job(version: &str, evidence_revision: u64, started_at_ms: u64) -> JobRecord {
        let scope = MemoryScope {
            channel: "qq".into(),
            session_id: "test".into(),
            user_id: "u".into(),
        };
        let evidence = InteractionEvidence {
            id: format!("e{evidence_revision}"),
            revision: evidence_revision,
            at_ms: 1,
            source: EvidenceSource::CompletedInteraction {
                message_id: format!("m{evidence_revision}"),
                session_revision: evidence_revision * 2,
                turn_id: evidence_revision,
                user_text: "原始证据".into(),
                assistant_text: "回复".into(),
            },
        };
        let id = batch_id(&scope, version, std::slice::from_ref(&evidence)).unwrap();
        let candidate = PreferenceCandidate {
            id: candidate_id(&id, 0),
            batch_id: id.clone(),
            draft: CandidateDraft {
                text: "偏好".into(),
                confidence: 90,
                evidence_ids: vec![evidence.id.clone()],
            },
            created_at_ms: started_at_ms + 1,
            expires_at_ms: started_at_ms + 1 + CANDIDATE_TTL_MS,
        };
        JobRecord {
            memory_revision: evidence_revision,
            previous_attempts: vec![],
            reflection: None,
            job: LearningJob {
                batch: LearningBatch {
                    attempt: 0,
                    id,
                    scope,
                    extractor_version: version.into(),
                    started_at_ms,
                    evidence: vec![evidence],
                },
                status: JobStatus::Completed,
                finished_at_ms: Some(started_at_ms + 1),
                candidates: vec![candidate],
            },
        }
    }
    #[test]
    fn streaming_global_checks_reject_cross_record_aliases_and_regressing_scope_order() {
        let mut check = Check::default();
        check
            .job(serde_json::to_value(job("v1", 1, 10)).unwrap())
            .unwrap();
        // 单条分别合法；组合不能重复消费同一范围的真实来源。
        assert!(
            check
                .job(serde_json::to_value(job("v2", 1, 11)).unwrap())
                .is_err()
        );
        let mut check = Check::default();
        check
            .job(serde_json::to_value(job("v1", 1, 10)).unwrap())
            .unwrap();
        assert!(
            check
                .job(serde_json::to_value(job("v2", 2, 5)).unwrap())
                .is_err()
        );
    }
    #[test]
    fn streaming_decisions_reject_consecutive_same_outcome_despite_revision_changes() {
        let source = job("v1", 1, 10);
        let scope = source.job.batch.scope.clone();
        let candidate = source.job.candidates[0].clone();
        let mut check = Check::default();
        check.job(serde_json::to_value(source).unwrap()).unwrap();
        let mut entry = ScopedDecisionRecord {
            scope: scope.clone(),
            record: LearningDecisionRecord {
                sequence: 1,
                at_ms: 12,
                decision: LearningDecision {
                    candidate_id: candidate.id.clone(),
                    batch_id: candidate.batch_id,
                    policy_version: "policy".into(),
                    memory_revision: 1,
                    evidence_ids: candidate.draft.evidence_ids,
                    action: LearningDecisionAction::Confirm,
                    reason: DecisionReason::Eligible,
                },
            },
        };
        check
            .decision(
                serde_json::to_value(&entry).unwrap(),
                Some((&scope, &candidate.id)),
            )
            .unwrap();
        entry.record.sequence = 2;
        entry.record.decision.memory_revision = 2;
        assert!(
            check
                .decision(
                    serde_json::to_value(entry).unwrap(),
                    Some((&scope, &candidate.id))
                )
                .is_err()
        );
    }
}
