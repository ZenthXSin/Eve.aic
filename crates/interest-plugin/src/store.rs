use crate::{cold, strict_json};
use eve_interest_api::*;
use eve_memory_api::{
    EvidenceSource, InteractionEvidence, MemoryScope, MemorySnapshot, validate_id, validate_text,
};
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    sync::{Arc, Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;
// 既有校验游标保存全部来源唯一性元数据；本操作先给该分配设置明确边界。
const MAX_SOURCE_EVIDENCE: u64 = 262_144;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Document {
    format_version: u32,
    jobs: Vec<JobRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archive: Vec<cold::JobRef>,
    interests: Vec<InterestRecord>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobRecord {
    pub(super) job: ObservationJob,
    /// 只保存批次来源的记忆修订，不复制未消费的原始记忆。
    pub(super) memory_revision: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(super) sequence: u64,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
struct Inner {
    document: Document,
    context: Option<PluginContext>,
    archive: Option<Arc<dyn InterestJobArchiveStore>>,
}
pub(super) struct StoredInterests {
    inner: Mutex<Inner>,
}

fn known_interests(document: &Document, scope: &MemoryScope) -> Vec<KnownInterest> {
    let mut active: Vec<_> = document
        .interests
        .iter()
        .filter(|interest| interest.scope == *scope && interest.status == InterestStatus::Active)
        .collect();
    active.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    active.truncate(MAX_KNOWN_INTERESTS);
    let mut known: Vec<_> = active
        .into_iter()
        .map(|interest| KnownInterest {
            id: interest.id.clone(),
            topic: interest.topic.clone(),
        })
        .collect();
    known.sort_by(|left, right| left.id.cmp(&right.id));
    known
}

impl StoredInterests {
    pub(super) fn open(
        context: PluginContext,
        archive: Option<Arc<dyn InterestJobArchiveStore>>,
    ) -> InterestResult<Self> {
        let mut document = match context
            .state_get(INTEREST_STATE_KEY)
            .map_err(|_| InterestError::Storage)?
        {
            None => {
                // 已有冷文件可能属于丢失根或未确认提交；不得当作空库重新消费来源。
                if let Some(archive) = &archive
                    && !archive.is_empty()?
                {
                    return Err(InterestError::CorruptState);
                }
                Document {
                    format_version: FORMAT_VERSION,
                    jobs: vec![],
                    archive: vec![],
                    interests: vec![],
                }
            }
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(InterestError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| InterestError::CorruptState)?;
                match value.get("format_version").and_then(|value| value.as_u64()) {
                    Some(1 | 2) => {}
                    Some(_) => return Err(InterestError::UnsupportedVersion),
                    None => return Err(InterestError::CorruptState),
                }
                let document: Document =
                    serde_json::from_value(value).map_err(|_| InterestError::CorruptState)?;
                validate_document(&document, archive.as_deref())?;
                if document.format_version == 2
                    && document
                        .jobs
                        .iter()
                        .any(|record| record.job.status != JobStatus::Running)
                {
                    return Err(InterestError::CorruptState);
                }
                document
            }
        };
        let mut changed = false;
        for record in &mut document.jobs {
            if record.job.status == JobStatus::Running {
                record.job.status = JobStatus::Interrupted;
                changed = true;
            }
        }
        if archive.is_some() && document.format_version == 1 {
            for (index, record) in document.jobs.iter_mut().enumerate() {
                record.sequence = index as u64 + 1;
            }
            document.format_version = 2;
            changed = true;
        }
        let mut inner = Inner {
            document: document.clone(),
            context: Some(context),
            archive,
        };
        // 先保存中断与迁移；公开实例前完成持久化，不请求模型或重放输入。
        if changed {
            persist(&mut inner, document)?;
        }
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> InterestResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| InterestError::Unavailable)?;
        if inner.context.is_none() {
            return Err(InterestError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn scopes(&self) -> InterestResult<Vec<MemoryScope>> {
        let inner = self.lock()?;
        let mut scopes = BTreeSet::new();
        for record in &inner.document.jobs {
            scopes.insert(record.job.batch.scope.clone());
        }
        for record in &inner.document.archive {
            scopes.insert(record.scope.clone());
        }
        for interest in &inner.document.interests {
            scopes.insert(interest.scope.clone());
        }
        Ok(scopes.into_iter().collect())
    }

    pub(super) fn snapshot(&self, scope: &MemoryScope) -> InterestResult<InterestSnapshot> {
        scope.validate().map_err(|_| InterestError::InvalidInput)?;
        let inner = self.lock()?;
        Ok(InterestSnapshot {
            scope: scope.clone(),
            interests: inner
                .document
                .interests
                .iter()
                .filter(|interest| interest.scope == *scope)
                .cloned()
                .collect(),
            jobs: inner
                .document
                .jobs
                .iter()
                .filter(|record| record.job.batch.scope == *scope)
                .map(|record| record.job.clone())
                .collect(),
            archived_jobs: inner
                .document
                .archive
                .iter()
                .filter(|record| record.scope == *scope)
                .count() as u64,
        })
    }

    pub(super) fn archive_status(&self) -> InterestResult<InterestArchiveStatus> {
        let inner = self.lock()?;
        Ok(InterestArchiveStatus {
            enabled: inner.archive.is_some(),
            hot_jobs: inner.document.jobs.len(),
            archived_jobs: inner.document.archive.len(),
            archived_bytes: inner.document.archive.iter().map(|r| r.bytes as u64).sum(),
            index_bytes: encode(&inner.document)?.len(),
        })
    }

    pub(super) fn jobs_page(
        &self,
        scope: &MemoryScope,
        after_sequence: u64,
        limit: usize,
    ) -> InterestResult<InterestJobPage> {
        scope.validate().map_err(|_| InterestError::InvalidInput)?;
        if limit == 0 || limit > MAX_JOB_PAGE {
            return Err(InterestError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut entries: Vec<(u64, bool, usize)> = inner
            .document
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, record)| record.job.batch.scope == *scope)
            .map(|(index, record)| {
                (
                    if inner.document.format_version == 1 {
                        index as u64 + 1
                    } else {
                        record.sequence
                    },
                    false,
                    index,
                )
            })
            .chain(
                inner
                    .document
                    .archive
                    .iter()
                    .enumerate()
                    .filter(|(_, record)| record.scope == *scope)
                    .map(|(index, record)| (record.sequence, true, index)),
            )
            .filter(|(sequence, _, _)| *sequence > after_sequence)
            .collect();
        entries.sort_unstable();
        let more = entries.len() > limit;
        entries.truncate(limit);
        let next_after = more.then(|| entries.last().expect("nonzero page").0);
        let mut jobs = Vec::with_capacity(entries.len());
        for (_, archived, index) in entries {
            let job = if archived {
                match load_ref(&inner, &inner.document.archive[index]) {
                    Ok(record) => record.job,
                    Err(error) => {
                        inner.context = None;
                        return Err(error);
                    }
                }
            } else {
                inner.document.jobs[index].job.clone()
            };
            if job.batch.scope != *scope {
                inner.context = None;
                return Err(InterestError::CorruptState);
            }
            jobs.push(job);
        }
        Ok(InterestJobPage { jobs, next_after })
    }

    pub(super) fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        observer_version: &str,
        options: &ObservationOptions,
    ) -> InterestResult<Option<ObservationBatch>> {
        let mut inner = self.lock()?;
        options.validate()?;
        validate_id(observer_version).map_err(|_| InterestError::InvalidInput)?;
        memory
            .scope
            .validate()
            .map_err(|_| InterestError::InvalidInput)?;
        if now_ms == 0 {
            return Err(InterestError::InvalidInput);
        }
        if memory.evidence.len() as u64 > MAX_SOURCE_EVIDENCE {
            return Err(InterestError::LimitReached);
        }
        let (consumed, last_start) = consumption(&inner, &memory.scope, memory.revision)?;
        for (id, expected) in &consumed {
            let evidence = memory
                .evidence
                .iter()
                .find(|evidence| &evidence.id == id)
                .ok_or(InterestError::Conflict)?;
            if cold::consumed(evidence)? != *expected {
                return Err(InterestError::Conflict);
            }
        }
        if last_start.is_some_and(|at| {
            now_ms
                .checked_sub(at)
                .is_none_or(|elapsed| elapsed < options.cooldown_ms)
        }) {
            return Ok(None);
        }
        let mut available: Vec<_> = memory
            .evidence
            .iter()
            .filter(|evidence| {
                matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
                    && !consumed.contains_key(evidence.id.as_str())
            })
            .collect();
        available.sort_by_key(|evidence| evidence.revision);
        let known_interests = known_interests(&inner.document, &memory.scope);
        let mut batch = ObservationBatch {
            // 摘要长度固定；先用等长占位值计算完整输入 JSON 的预算。
            id: format!("interest-batch-{}", "0".repeat(64)),
            scope: memory.scope.clone(),
            observer_version: observer_version.into(),
            started_at_ms: now_ms,
            evidence: vec![],
            known_interests,
        };
        for next in available {
            let size = serde_json::to_vec(next)
                .map_err(|_| InterestError::InvalidInput)?
                .len();
            if size > MAX_EVIDENCE_BYTES {
                continue;
            }
            batch.evidence.push(next.clone());
            if serde_json::to_vec(&batch)
                .map_err(|_| InterestError::InvalidInput)?
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
        if batch.evidence.len() < options.min_new_evidence {
            return Ok(None);
        }
        if inner.document.jobs.len() >= MAX_JOBS
            || (inner.document.format_version == 2
                && inner.document.jobs.len() + inner.document.archive.len() >= MAX_JOB_REFERENCES)
        {
            return Err(InterestError::LimitReached);
        }
        batch.id = batch_id(&batch)?;
        batch.validate()?;
        let mut next = inner.document.clone();
        next.jobs.push(JobRecord {
            sequence: next_sequence(&next),
            memory_revision: memory.revision,
            job: ObservationJob {
                batch: batch.clone(),
                status: JobStatus::Running,
                finished_at_ms: None,
                updates: vec![],
                results: vec![],
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
        observer_version: &str,
        options: &ObservationOptions,
    ) -> InterestResult<Option<ObservationBatch>> {
        use eve_memory_api::{MemoryEvidenceCursor, ValidatedMemoryCursor};
        options.validate()?;
        validate_id(observer_version).map_err(|_| InterestError::InvalidInput)?;
        scope.validate().map_err(|_| InterestError::InvalidInput)?;
        if now_ms == 0 {
            return Err(InterestError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut cursor = ValidatedMemoryCursor::new(scope.clone(), reader.scan()?)?;
        if cursor.summary().evidence_count > MAX_SOURCE_EVIDENCE {
            return Err(InterestError::LimitReached);
        }
        let revision = cursor.summary().revision;
        let (expected, last_start) = consumption(&inner, scope, revision)?;
        let cooling = last_start.is_some_and(|at| {
            now_ms
                .checked_sub(at)
                .is_none_or(|elapsed| elapsed < options.cooldown_ms)
        });
        let mut seen = BTreeSet::new();
        let known_interests = known_interests(&inner.document, scope);
        let mut batch = ObservationBatch {
            // 摘要长度固定；先用等长占位值计算完整输入 JSON 的预算。
            id: format!("interest-batch-{}", "0".repeat(64)),
            scope: scope.clone(),
            observer_version: observer_version.into(),
            started_at_ms: now_ms,
            evidence: vec![],
            known_interests,
        };

        loop {
            let page = cursor.next_page()?;
            if page.is_empty() {
                break;
            }
            for evidence in page {
                if let Some(prior) = expected.get(evidence.id.as_str()) {
                    if *prior != cold::consumed(&evidence)? {
                        return Err(InterestError::Conflict);
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
                    .map_err(|_| InterestError::InvalidInput)?
                    .len()
                    > MAX_EVIDENCE_BYTES
                {
                    continue;
                }
                batch.evidence.push(evidence);
                if serde_json::to_vec(&batch)
                    .map_err(|_| InterestError::InvalidInput)?
                    .len()
                    > MAX_INPUT_BYTES
                {
                    batch.evidence.pop();
                }
            }
        }
        // 即使批次已满或仍在冷却，也必须核完后续页和所有已消费来源，才允许提交。
        if seen.len() != expected.len() {
            return Err(InterestError::Conflict);
        }
        if cooling || batch.evidence.len() < options.min_new_evidence {
            return Ok(None);
        }
        if inner.document.jobs.len() >= MAX_JOBS
            || (inner.document.format_version == 2
                && inner.document.jobs.len() + inner.document.archive.len() >= MAX_JOB_REFERENCES)
        {
            return Err(InterestError::LimitReached);
        }
        batch.id = batch_id(&batch)?;
        batch.validate()?;
        let mut next = inner.document.clone();
        next.jobs.push(JobRecord {
            sequence: next_sequence(&next),
            memory_revision: revision,
            job: ObservationJob {
                batch: batch.clone(),
                status: JobStatus::Running,
                finished_at_ms: None,
                updates: vec![],
                results: vec![],
            },
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }

    pub(super) fn finish(
        &self,
        batch: &ObservationBatch,
        at_ms: u64,
        outcome: ObservationOutcome,
    ) -> InterestResult<Vec<UpdateResult>> {
        let mut inner = self.lock()?;
        if let Some(reference) = inner
            .document
            .archive
            .iter()
            .find(|record| record.id == batch.id)
        {
            if reference.scope != batch.scope {
                return Err(InterestError::Conflict);
            }
            let record = match load_ref(&inner, reference) {
                Ok(record) => record,
                Err(error) => {
                    inner.context = None;
                    return Err(error);
                }
            };
            if record.job.batch != *batch {
                return Err(InterestError::Conflict);
            }
            return replay(&record.job, &outcome);
        }
        let index = inner
            .document
            .jobs
            .iter()
            .position(|record| record.job.batch.id == batch.id)
            .ok_or(InterestError::Conflict)?;
        let old = &inner.document.jobs[index].job;
        if old.batch != *batch {
            return Err(InterestError::Conflict);
        }
        if old.status != JobStatus::Running {
            // 同一结局重放返回原结果且零写入；其他结局不能覆盖已保存的批次。
            return replay(old, &outcome);
        }
        if at_ms < batch.started_at_ms {
            return Err(InterestError::InvalidInput);
        }
        let mut next = inner.document.clone();
        let results = match outcome {
            ObservationOutcome::Failed(reason) => {
                let job = &mut next.jobs[index].job;
                job.status = JobStatus::Failed(reason);
                job.finished_at_ms = Some(at_ms);
                vec![]
            }
            ObservationOutcome::Completed(updates) => {
                validate_updates(batch, &updates)?;
                let results = updates
                    .iter()
                    .enumerate()
                    .map(|(position, update)| apply(&mut next, batch, position, update, at_ms))
                    .collect::<InterestResult<Vec<_>>>()?;
                let job = &mut next.jobs[index].job;
                job.status = JobStatus::Completed;
                job.finished_at_ms = Some(at_ms);
                job.updates = updates;
                job.results = results.clone();
                results
            }
        };
        persist(&mut inner, next)?;
        Ok(results)
    }

    pub(super) fn withdraw(
        &self,
        scope: &MemoryScope,
        interest_id: &str,
        request: WithdrawalRequest,
    ) -> InterestResult<InterestRecord> {
        let mut inner = self.lock()?;
        scope.validate().map_err(|_| InterestError::InvalidInput)?;
        validate_id(interest_id).map_err(|_| InterestError::InvalidInput)?;
        validate_id(&request.evidence_id).map_err(|_| InterestError::InvalidInput)?;
        validate_id(&request.message_id).map_err(|_| InterestError::InvalidInput)?;
        validate_text(&request.text, MAX_QUOTE_BYTES).map_err(|_| InterestError::InvalidInput)?;
        if request.at_ms == 0 {
            return Err(InterestError::InvalidInput);
        }
        let index = inner
            .document
            .interests
            .iter()
            .position(|interest| interest.id == interest_id && interest.scope == *scope)
            .ok_or(InterestError::NotFound)?;
        let current = &inner.document.interests[index];
        // 同一命令重放或已撤回的兴趣都不改写记录；撤回不会因容量失败。
        if current.status == InterestStatus::Withdrawn
            || current
                .statements
                .iter()
                .any(|statement| statement.evidence_id == request.evidence_id)
        {
            return Ok(current.clone());
        }
        let mut next = inner.document.clone();
        let interest = &mut next.interests[index];
        interest.statements.push(InterestStatement {
            kind: StatementKind::Withdrawal,
            quote: request.text,
            evidence_id: request.evidence_id,
            message_id: request.message_id,
            observed_at_ms: request.at_ms,
            origin: StatementOrigin::UserCommand,
        });
        interest.status = InterestStatus::Withdrawn;
        interest.revision = interest
            .revision
            .checked_add(1)
            .ok_or(InterestError::LimitReached)?;
        interest.updated_at_ms = interest.updated_at_ms.max(request.at_ms);
        interest.validate()?;
        let saved = interest.clone();
        persist(&mut inner, next)?;
        Ok(saved)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("兴趣观察状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

/// 应用一条已校验的更新；只读当前账本状态，不访问记忆或模型。
fn apply(
    document: &mut Document,
    batch: &ObservationBatch,
    position: usize,
    update: &InterestUpdateDraft,
    at_ms: u64,
) -> InterestResult<UpdateResult> {
    let target = match &update.target {
        InterestTarget::Existing { id } => document
            .interests
            .iter()
            .position(|interest| interest.id == *id && interest.scope == batch.scope),
        // 同名新主题并入当前活动兴趣，避免同一兴趣派生多个目标。
        InterestTarget::New { topic } => document.interests.iter().position(|interest| {
            interest.scope == batch.scope
                && interest.status == InterestStatus::Active
                && topic_key(&interest.topic) == topic_key(topic)
        }),
    };
    let statements = statements(batch, update)?;
    let withdrawal = statements
        .iter()
        .any(|statement| statement.kind == StatementKind::Withdrawal);
    let Some(index) = target else {
        let InterestTarget::New { topic } = &update.target else {
            return Ok(UpdateResult::Rejected {
                reason: RejectReason::NotActive,
            });
        };
        if document.interests.len() >= MAX_INTERESTS {
            return Ok(UpdateResult::Rejected {
                reason: RejectReason::InterestLimit,
            });
        }
        let id = hash_id("interest-", format!("{}:{position}", batch.id).as_bytes());
        let record = InterestRecord {
            id: id.clone(),
            scope: batch.scope.clone(),
            topic: topic.clone(),
            status: InterestStatus::Active,
            revision: 1,
            statements,
            inferences: update
                .inferred_need
                .iter()
                .map(|text| InferredNeed {
                    text: text.clone(),
                    batch_id: batch.id.clone(),
                    interest_revision: 1,
                })
                .collect(),
            created_at_ms: at_ms,
            updated_at_ms: at_ms,
        };
        record.validate()?;
        document.interests.push(record);
        return Ok(UpdateResult::Created { interest_id: id });
    };
    let interest = &mut document.interests[index];
    if interest.status != InterestStatus::Active {
        return Ok(UpdateResult::Rejected {
            reason: RejectReason::NotActive,
        });
    }
    // 普通陈述最多占用 MAX_STATEMENTS - 1 个位置，保证撤回总能保存。
    let fresh: Vec<_> = statements
        .into_iter()
        .filter(|statement| {
            !interest.statements.iter().any(|saved| {
                saved.evidence_id == statement.evidence_id
                    && topic_key(&saved.quote) == topic_key(&statement.quote)
            })
        })
        .collect();
    let limit = if withdrawal {
        MAX_STATEMENTS
    } else {
        MAX_STATEMENTS - 1
    };
    let room = limit.saturating_sub(interest.statements.len());
    if fresh.is_empty() || (!withdrawal && fresh.len() > room) || room == 0 {
        return Ok(UpdateResult::Rejected {
            reason: RejectReason::StatementLimit,
        });
    }
    let revision = interest
        .revision
        .checked_add(1)
        .ok_or(InterestError::LimitReached)?;
    interest.statements.extend(fresh.into_iter().take(room));
    interest.revision = revision;
    interest.updated_at_ms = interest.updated_at_ms.max(at_ms);
    if withdrawal {
        interest.status = InterestStatus::Withdrawn;
    } else if let Some(text) = &update.inferred_need
        && interest.inferences.len() < MAX_INFERENCES
    {
        interest.inferences.push(InferredNeed {
            text: text.clone(),
            batch_id: batch.id.clone(),
            interest_revision: revision,
        });
    }
    interest.validate()?;
    let interest_id = interest.id.clone();
    Ok(if withdrawal {
        UpdateResult::Withdrawn {
            interest_id,
            revision,
        }
    } else {
        UpdateResult::Updated {
            interest_id,
            revision,
        }
    })
}

fn statements(
    batch: &ObservationBatch,
    update: &InterestUpdateDraft,
) -> InterestResult<Vec<InterestStatement>> {
    update
        .statements
        .iter()
        .map(|draft| {
            let evidence = batch
                .evidence
                .iter()
                .find(|evidence| evidence.id == draft.evidence_id)
                .ok_or(InterestError::InvalidInput)?;
            Ok(InterestStatement {
                kind: draft.kind,
                quote: draft.quote.clone(),
                evidence_id: evidence.id.clone(),
                message_id: source_message(evidence).ok_or(InterestError::InvalidInput)?,
                observed_at_ms: evidence.at_ms.max(1),
                origin: StatementOrigin::Observation {
                    batch_id: batch.id.clone(),
                },
            })
        })
        .collect()
}

fn source_message(evidence: &InteractionEvidence) -> Option<String> {
    match &evidence.source {
        EvidenceSource::CompletedInteraction { message_id, .. } => Some(message_id.clone()),
        EvidenceSource::UserStatement { .. } => None,
    }
}

fn replay(old: &ObservationJob, outcome: &ObservationOutcome) -> InterestResult<Vec<UpdateResult>> {
    match (&old.status, outcome) {
        (JobStatus::Completed, ObservationOutcome::Completed(updates))
            if old.updates == *updates =>
        {
            Ok(old.results.clone())
        }
        (JobStatus::Failed(first), ObservationOutcome::Failed(second)) if first == second => {
            Ok(vec![])
        }
        _ => Err(InterestError::Conflict),
    }
}

fn next_sequence(document: &Document) -> u64 {
    if document.format_version == 1 {
        0
    } else {
        (document.jobs.len() + document.archive.len()) as u64 + 1
    }
}

fn load_ref(inner: &Inner, reference: &cold::JobRef) -> InterestResult<JobRecord> {
    cold::load(
        inner.archive.as_deref().ok_or(InterestError::Unavailable)?,
        reference,
    )
}

fn consumption(
    inner: &Inner,
    scope: &MemoryScope,
    revision: u64,
) -> InterestResult<(BTreeMap<String, cold::Consumed>, Option<u64>)> {
    // 使用打开/提交时核实的消费目录；空闲轮和冷却不重新读取所有冷正文。
    // 真正新增 Running 必须经过 persist 的完整正文校验，成功返回后才允许请求模型。
    let mut expected = BTreeMap::new();
    let mut latest = None;
    for reference in inner
        .document
        .archive
        .iter()
        .filter(|record| record.scope == *scope)
    {
        if revision < reference.memory_revision {
            return Err(InterestError::Conflict);
        }
        latest = Some(latest.map_or(reference.started_at_ms, |value: u64| {
            value.max(reference.started_at_ms)
        }));
        for item in &reference.consumed {
            if expected.insert(item.id.clone(), item.clone()).is_some() {
                return Err(InterestError::Conflict);
            }
        }
    }
    for record in inner
        .document
        .jobs
        .iter()
        .filter(|record| record.job.batch.scope == *scope)
    {
        if revision < record.memory_revision {
            return Err(InterestError::Conflict);
        }
        latest = Some(latest.map_or(record.job.batch.started_at_ms, |value| {
            value.max(record.job.batch.started_at_ms)
        }));
        for evidence in &record.job.batch.evidence {
            if expected
                .insert(evidence.id.clone(), cold::consumed(evidence)?)
                .is_some()
            {
                return Err(InterestError::Conflict);
            }
        }
    }
    Ok((expected, latest))
}

fn persist(inner: &mut Inner, mut next: Document) -> InterestResult<()> {
    if let Err(error) = validate_document(&next, inner.archive.as_deref()) {
        if matches!(
            error,
            InterestError::Storage | InterestError::CorruptState | InterestError::Unavailable
        ) {
            inner.context = None;
        }
        return Err(error);
    }
    let mut terminal = Vec::new();
    if next.format_version == 2 {
        let mut running = Vec::new();
        for record in next.jobs.drain(..) {
            if record.job.status == JobStatus::Running {
                running.push(record);
            } else {
                terminal.push(record);
            }
        }
        next.jobs = running;
        for record in &terminal {
            next.archive.push(cold::encode(record)?.0);
        }
        next.archive.sort_by_key(|reference| reference.sequence);
    }
    // 候选根容量先核对；每次只编码一份冷正文，保留旧根和原消费证据直到提交成功。
    let bytes = encode(&next)?;
    for record in &terminal {
        let (reference, body) = cold::encode(record)?;
        let archive = inner.archive.as_deref().ok_or(InterestError::Unavailable)?;
        if let Err(error) = archive.put(&reference.sha256, &body) {
            inner.context = None;
            return Err(error);
        }
    }
    let context = inner.context.as_ref().ok_or(InterestError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(INTEREST_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(InterestError::Storage);
    }
    inner.document = next;
    Ok(())
}

fn encode(document: &Document) -> InterestResult<Vec<u8>> {
    if document.jobs.len() > MAX_JOBS
        || document.interests.len() > MAX_INTERESTS
        || document.jobs.len() + document.archive.len() > MAX_JOB_REFERENCES
    {
        return Err(InterestError::LimitReached);
    }
    let bytes = serde_json::to_vec(document).map_err(|_| InterestError::InvalidInput)?;
    // 新格式的短 Running 正文可能小于终态目录；预留每条替换差额及首次 archive 键。
    // 这是根索引空间，不承诺磁盘还能写入冷正文，也不预先接受任意兴趣更新。
    let mut recovery_bytes = if document.format_version == 2 && document.archive.is_empty() {
        32
    } else {
        0
    };
    for record in document
        .jobs
        .iter()
        .filter(|record| record.job.status == JobStatus::Running)
    {
        if document.format_version == 1 {
            recovery_bytes += "Interrupted".len() - "Running".len();
        } else {
            let mut terminal = record.clone();
            terminal.job.status = JobStatus::Failed(ObservationFailure::InvalidOutput);
            terminal.job.finished_at_ms = Some(u64::MAX);
            let (mut reference, _) = cold::encode(&terminal)?;
            reference.bytes = MAX_JOB_ARCHIVE_RECORD_BYTES;
            let reference_bytes = serde_json::to_vec(&reference)
                .map_err(|_| InterestError::InvalidInput)?
                .len();
            let hot_bytes = serde_json::to_vec(record)
                .map_err(|_| InterestError::InvalidInput)?
                .len();
            recovery_bytes += reference_bytes.saturating_sub(hot_bytes) + 2;
        }
    }
    if bytes.len().saturating_add(recovery_bytes) > MAX_STATE_BYTES {
        return Err(InterestError::LimitReached);
    }
    Ok(bytes)
}

fn validate_job(record: &JobRecord, interest_ids: &BTreeSet<&str>) -> InterestResult<()> {
    let invalid = || InterestError::CorruptState;
    let job = &record.job;
    job.batch.validate().map_err(|_| invalid())?;
    if job.batch.id != batch_id(&job.batch).map_err(|_| invalid())?
        || record.memory_revision == 0
        || job
            .batch
            .evidence
            .iter()
            .any(|evidence| evidence.revision > record.memory_revision)
    {
        return Err(invalid());
    }
    match &job.status {
        JobStatus::Running | JobStatus::Interrupted => {
            if job.finished_at_ms.is_some() || !job.updates.is_empty() || !job.results.is_empty() {
                return Err(invalid());
            }
        }
        JobStatus::Failed(_) => {
            if job
                .finished_at_ms
                .is_none_or(|at| at < job.batch.started_at_ms)
                || !job.updates.is_empty()
                || !job.results.is_empty()
            {
                return Err(invalid());
            }
        }
        JobStatus::Completed => {
            if job
                .finished_at_ms
                .is_none_or(|at| at < job.batch.started_at_ms)
                || job.updates.len() != job.results.len()
            {
                return Err(invalid());
            }
            validate_updates(&job.batch, &job.updates).map_err(|_| invalid())?;
        }
    }
    for result in &job.results {
        let id = match result {
            UpdateResult::Created { interest_id }
            | UpdateResult::Updated { interest_id, .. }
            | UpdateResult::Withdrawn { interest_id, .. } => interest_id,
            UpdateResult::Rejected { .. } => continue,
        };
        if !interest_ids.contains(id.as_str()) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_document(
    document: &Document,
    archive: Option<&dyn InterestJobArchiveStore>,
) -> InterestResult<()> {
    let invalid = || InterestError::CorruptState;
    if document.jobs.len() > MAX_JOBS
        || document.interests.len() > MAX_INTERESTS
        || document.jobs.len() + document.archive.len() > MAX_JOB_REFERENCES
        || (document.format_version == 1
            && (!document.archive.is_empty()
                || document.jobs.iter().any(|record| record.sequence != 0)))
        || !matches!(document.format_version, 1 | 2)
    {
        return Err(invalid());
    }
    if document.format_version == 2 && archive.is_none() {
        return Err(InterestError::Unavailable);
    }
    let mut ids = BTreeSet::new();
    for interest in &document.interests {
        interest.validate().map_err(|_| invalid())?;
        if !ids.insert(interest.id.as_str()) {
            return Err(invalid());
        }
    }
    let mut batches = BTreeMap::new();
    let mut chronological = Vec::new();
    let mut consumed = BTreeSet::new();
    for (index, record) in document.jobs.iter().enumerate() {
        validate_job(record, &ids)?;
        if batches
            .insert(record.job.batch.id.as_str(), (false, index))
            .is_some()
        {
            return Err(invalid());
        }
        let sequence = if document.format_version == 1 {
            index as u64 + 1
        } else {
            record.sequence
        };
        chronological.push((
            sequence,
            &record.job.batch.scope,
            record.job.batch.started_at_ms,
        ));
        for evidence in &record.job.batch.evidence {
            if !consumed.insert((&record.job.batch.scope, evidence.id.as_str())) {
                return Err(invalid());
            }
        }
    }
    for (index, reference) in document.archive.iter().enumerate() {
        let record = cold::load(archive.ok_or_else(invalid)?, reference)?;
        validate_job(&record, &ids)?;
        if batches
            .insert(reference.id.as_str(), (true, index))
            .is_some()
        {
            return Err(invalid());
        }
        chronological.push((
            reference.sequence,
            &reference.scope,
            reference.started_at_ms,
        ));
        for evidence in &reference.consumed {
            if !consumed.insert((&reference.scope, evidence.id.as_str())) {
                return Err(invalid());
            }
        }
    }
    chronological.sort_by_key(|(sequence, _, _)| *sequence);
    let mut last_start = BTreeMap::new();
    for (index, (sequence, scope, started)) in chronological.into_iter().enumerate() {
        if sequence != index as u64 + 1 {
            return Err(invalid());
        }
        let previous = last_start.entry(scope).or_insert(0);
        if started < *previous {
            return Err(invalid());
        }
        *previous = started;
    }
    let get_job = |id: &str| -> InterestResult<ObservationJob> {
        let (archived, index) = *batches.get(id).ok_or_else(invalid)?;
        if archived {
            Ok(cold::load(archive.ok_or_else(invalid)?, &document.archive[index])?.job)
        } else {
            Ok(document.jobs[index].job.clone())
        }
    };
    for interest in &document.interests {
        for statement in &interest.statements {
            let StatementOrigin::Observation { batch_id } = &statement.origin else {
                if statement.kind != StatementKind::Withdrawal {
                    return Err(invalid());
                }
                continue;
            };
            let job = get_job(batch_id)?;
            if job.status != JobStatus::Completed || job.batch.scope != interest.scope {
                return Err(invalid());
            }
            let evidence = job
                .batch
                .evidence
                .iter()
                .find(|evidence| evidence.id == statement.evidence_id)
                .ok_or_else(invalid)?;
            let EvidenceSource::CompletedInteraction {
                message_id,
                user_text,
                ..
            } = &evidence.source
            else {
                return Err(invalid());
            };
            if *message_id != statement.message_id || !quote_matches(user_text, &statement.quote) {
                return Err(invalid());
            }
        }
        for inference in &interest.inferences {
            let job = get_job(&inference.batch_id)?;
            if job.status != JobStatus::Completed || job.batch.scope != interest.scope {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn batch_id(batch: &ObservationBatch) -> InterestResult<String> {
    let stable_evidence: Vec<_> = batch
        .evidence
        .iter()
        .map(|value| {
            let mut stable = value.clone();
            stable.at_ms = 0;
            stable
        })
        .collect();
    let bytes = serde_json::to_vec(&(
        &batch.scope,
        &batch.observer_version,
        stable_evidence,
        &batch.known_interests,
    ))
    .map_err(|_| InterestError::InvalidInput)?;
    Ok(hash_id("interest-batch-", &bytes))
}

pub(crate) fn hash_id(prefix: &str, bytes: &[u8]) -> String {
    let mut id = String::from(prefix);
    for byte in digest(&SHA256, bytes).as_ref() {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    fn running() -> JobRecord {
        let mut batch = ObservationBatch {
            id: "placeholder".into(),
            scope: MemoryScope {
                channel: "q".into(),
                session_id: "s".into(),
                user_id: "u".into(),
            },
            observer_version: "v".into(),
            started_at_ms: 1,
            evidence: vec![InteractionEvidence {
                id: "e".into(),
                revision: 1,
                at_ms: 1,
                source: EvidenceSource::CompletedInteraction {
                    message_id: "m".into(),
                    session_revision: 2,
                    turn_id: 1,
                    user_text: "a".into(),
                    assistant_text: "b".into(),
                },
            }],
            known_interests: vec![],
        };
        batch.id = batch_id(&batch).unwrap();
        JobRecord {
            sequence: 1,
            memory_revision: 1,
            job: ObservationJob {
                batch,
                status: JobStatus::Running,
                finished_at_ms: None,
                updates: vec![],
                results: vec![],
            },
        }
    }

    fn reference() -> cold::JobRef {
        let mut record = running();
        record.job.status = JobStatus::Interrupted;
        cold::encode(&record).unwrap().0
    }

    fn document() -> Document {
        Document {
            format_version: 2,
            jobs: vec![],
            archive: vec![],
            interests: vec![],
        }
    }

    #[test]
    fn reference_count_cap_is_independent_of_small_serialized_root() {
        // 仅测试编码容量，不访问冷存储；语义/唯一性校验由集成测试覆盖。
        let mut root = document();
        root.archive = vec![reference(); MAX_JOB_REFERENCES];
        assert!(encode(&root).is_ok());
        root.archive.push(reference());
        assert_eq!(encode(&root).err(), Some(InterestError::LimitReached));
        root.archive.pop();
        root.jobs.push(running());
        assert_eq!(encode(&root).err(), Some(InterestError::LimitReached));
    }

    fn pad_root(root: &mut Document, target_bytes: usize) {
        // 人工放大目录元数据隔离编码器的字节边界；不把此根当合法业务记录提交。
        let base = serde_json::to_vec(root).unwrap().len();
        root.archive[0]
            .id
            .push_str(&"x".repeat(target_bytes - base));
        assert_eq!(serde_json::to_vec(root).unwrap().len(), target_bytes);
    }

    #[test]
    fn root_byte_limit_does_not_depend_on_reference_count() {
        let mut root = document();
        root.archive.push(reference());
        pad_root(&mut root, MAX_STATE_BYTES);
        assert_eq!(encode(&root).unwrap().len(), MAX_STATE_BYTES);
        root.archive[0].id.push('x');
        assert_eq!(encode(&root).err(), Some(InterestError::LimitReached));
    }

    #[test]
    fn short_running_reserves_growth_for_empty_terminal_or_interrupted_reference() {
        let mut root = document();
        let mut original = running();
        // 编码器边界输入：空正文不是合法观察批次，此处仅使ref替换正增长可被测到。
        original.job.batch.evidence.clear();
        root.jobs.push(original.clone());
        let mut filler = reference();
        filler.sequence = 2;
        root.archive.push(filler);
        let mut worst = original.clone();
        worst.job.status = JobStatus::Failed(ObservationFailure::InvalidOutput);
        worst.job.finished_at_ms = Some(u64::MAX);
        let (mut worst_reference, _) = cold::encode(&worst).unwrap();
        worst_reference.bytes = MAX_JOB_ARCHIVE_RECORD_BYTES;
        let growth = serde_json::to_vec(&worst_reference)
            .unwrap()
            .len()
            .saturating_sub(serde_json::to_vec(&original).unwrap().len())
            + 2;
        assert!(growth > 2, "短输入必须实际覆盖目录变大情形");
        pad_root(&mut root, MAX_STATE_BYTES - growth);
        assert!(encode(&root).is_ok());
        let mut unreserved = root.clone();
        unreserved.archive[0].id.push('x');
        assert!(serde_json::to_vec(&unreserved).unwrap().len() <= MAX_STATE_BYTES);
        assert_eq!(encode(&unreserved).err(), Some(InterestError::LimitReached));
        for (status, finished) in [
            (JobStatus::Interrupted, None),
            (JobStatus::Completed, Some(u64::MAX)),
            (
                JobStatus::Failed(ObservationFailure::InvalidOutput),
                Some(u64::MAX),
            ),
        ] {
            let mut settled = root.clone();
            let mut terminal = settled.jobs.pop().unwrap();
            terminal.job.status = status;
            terminal.job.finished_at_ms = finished;
            settled.archive.push(cold::encode(&terminal).unwrap().0);
            assert!(encode(&settled).is_ok());
        }
    }
}
