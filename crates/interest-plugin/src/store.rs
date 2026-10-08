use crate::strict_json;
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
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    jobs: Vec<JobRecord>,
    interests: Vec<InterestRecord>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobRecord {
    job: ObservationJob,
    /// 只保存批次来源的记忆修订，不复制未消费的原始记忆。
    memory_revision: u64,
}
struct Inner {
    document: Document,
    context: Option<PluginContext>,
}
pub(super) struct StoredInterests {
    inner: Mutex<Inner>,
}

impl StoredInterests {
    pub(super) fn open(context: PluginContext) -> InterestResult<Self> {
        let mut document = match context
            .state_get(INTEREST_STATE_KEY)
            .map_err(|_| InterestError::Storage)?
        {
            None => Document {
                format_version: FORMAT_VERSION,
                jobs: vec![],
                interests: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(InterestError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| InterestError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        InterestError::UnsupportedVersion
                    } else {
                        InterestError::CorruptState
                    });
                }
                let document: Document =
                    serde_json::from_value(value).map_err(|_| InterestError::CorruptState)?;
                validate_document(&document).map_err(|_| InterestError::CorruptState)?;
                document
            }
        };
        let mut interrupted = false;
        for record in &mut document.jobs {
            if record.job.status == JobStatus::Running {
                record.job.status = JobStatus::Interrupted;
                interrupted = true;
            }
        }
        // 先保存中断结局再公开实例；不重试已消费的输入，也不伪造完成时间。
        if interrupted {
            let bytes = encode(&document)?;
            context
                .state_set(INTEREST_STATE_KEY, bytes)
                .map_err(|_| InterestError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                document,
                context: Some(context),
            }),
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
        })
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
        let previous: Vec<_> = inner
            .document
            .jobs
            .iter()
            .filter(|record| record.job.batch.scope == memory.scope)
            .collect();
        let mut consumed = BTreeSet::new();
        for record in &previous {
            // 记忆回退或已消费证据被改写时不能继续推进，避免把同一交互当成新输入。
            if memory.revision < record.memory_revision {
                return Err(InterestError::Conflict);
            }
            for evidence in &record.job.batch.evidence {
                if !memory.evidence.iter().any(|value| value == evidence) {
                    return Err(InterestError::Conflict);
                }
                consumed.insert(evidence.id.as_str());
            }
        }
        if let Some(last) = previous.last() {
            let Some(elapsed) = now_ms.checked_sub(last.job.batch.started_at_ms) else {
                return Ok(None);
            };
            if elapsed < options.cooldown_ms {
                return Ok(None);
            }
        }
        let mut available: Vec<_> = memory
            .evidence
            .iter()
            .filter(|evidence| {
                matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
                    && !consumed.contains(evidence.id.as_str())
            })
            .collect();
        available.sort_by_key(|evidence| evidence.revision);
        let mut active: Vec<_> = inner
            .document
            .interests
            .iter()
            .filter(|interest| {
                interest.scope == memory.scope && interest.status == InterestStatus::Active
            })
            .collect();
        // 只列出最近更新的活动兴趣；更旧的兴趣仍保存，但本批次不能关联或撤回。
        active.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        active.truncate(MAX_KNOWN_INTERESTS);
        let mut known_interests: Vec<_> = active
            .into_iter()
            .map(|interest| KnownInterest {
                id: interest.id.clone(),
                topic: interest.topic.clone(),
            })
            .collect();
        known_interests.sort_by(|left, right| left.id.cmp(&right.id));
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
        if inner.document.jobs.len() >= MAX_JOBS {
            return Err(InterestError::LimitReached);
        }
        batch.id = batch_id(&batch)?;
        batch.validate()?;
        let mut next = inner.document.clone();
        next.jobs.push(JobRecord {
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

    pub(super) fn finish(
        &self,
        batch: &ObservationBatch,
        at_ms: u64,
        outcome: ObservationOutcome,
    ) -> InterestResult<Vec<UpdateResult>> {
        let mut inner = self.lock()?;
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
            return match (&old.status, &outcome) {
                (JobStatus::Completed, ObservationOutcome::Completed(updates))
                    if old.updates == *updates =>
                {
                    Ok(old.results.clone())
                }
                (JobStatus::Failed(first), ObservationOutcome::Failed(second))
                    if first == second =>
                {
                    Ok(vec![])
                }
                _ => Err(InterestError::Conflict),
            };
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

fn persist(inner: &mut Inner, next: Document) -> InterestResult<()> {
    let bytes = encode(&next)?;
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
    if document.jobs.len() > MAX_JOBS || document.interests.len() > MAX_INTERESTS {
        return Err(InterestError::LimitReached);
    }
    let bytes = serde_json::to_vec(document).map_err(|_| InterestError::InvalidInput)?;
    // Interrupted 比 Running 多四个字节；写入时预留，保证下次启动能保存中断结局。
    let recovery_bytes = document
        .jobs
        .iter()
        .filter(|record| record.job.status == JobStatus::Running)
        .count()
        * ("Interrupted".len() - "Running".len());
    if bytes.len().saturating_add(recovery_bytes) > MAX_STATE_BYTES {
        return Err(InterestError::LimitReached);
    }
    Ok(bytes)
}

fn validate_document(document: &Document) -> InterestResult<()> {
    let invalid = || InterestError::CorruptState;
    if document.jobs.len() > MAX_JOBS || document.interests.len() > MAX_INTERESTS {
        return Err(invalid());
    }
    let mut batches = BTreeMap::new();
    let mut consumed = BTreeSet::new();
    let mut last_start: BTreeMap<&MemoryScope, u64> = BTreeMap::new();
    for record in &document.jobs {
        let job = &record.job;
        job.batch.validate().map_err(|_| invalid())?;
        if job.batch.id != batch_id(&job.batch).map_err(|_| invalid())?
            || record.memory_revision == 0
            || batches.insert(job.batch.id.as_str(), job).is_some()
        {
            return Err(invalid());
        }
        // 同一作用域的批次按开始时间顺序追加，每条证据只被消费一次。
        let start = last_start.entry(&job.batch.scope).or_insert(0);
        if job.batch.started_at_ms < *start {
            return Err(invalid());
        }
        *start = job.batch.started_at_ms;
        for evidence in &job.batch.evidence {
            if !consumed.insert((&job.batch.scope, evidence.id.as_str())) {
                return Err(invalid());
            }
        }
        match &job.status {
            JobStatus::Running | JobStatus::Interrupted => {
                if job.finished_at_ms.is_some()
                    || !job.updates.is_empty()
                    || !job.results.is_empty()
                {
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
    }
    let mut ids = BTreeSet::new();
    for interest in &document.interests {
        interest.validate().map_err(|_| invalid())?;
        if !ids.insert(interest.id.as_str()) {
            return Err(invalid());
        }
        for statement in &interest.statements {
            let StatementOrigin::Observation { batch_id } = &statement.origin else {
                // 命令来源只允许撤回；兴趣与经验必须来自已送达交互的观察。
                if statement.kind != StatementKind::Withdrawal {
                    return Err(invalid());
                }
                continue;
            };
            // 观察来源必须能回溯到同一作用域已完成批次中的用户原文。
            let job = batches.get(batch_id.as_str()).ok_or_else(invalid)?;
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
            let job = batches
                .get(inference.batch_id.as_str())
                .ok_or_else(invalid)?;
            if job.status != JobStatus::Completed || job.batch.scope != interest.scope {
                return Err(invalid());
            }
        }
    }
    for record in &document.jobs {
        for result in &record.job.results {
            let id = match result {
                UpdateResult::Created { interest_id }
                | UpdateResult::Updated { interest_id, .. }
                | UpdateResult::Withdrawn { interest_id, .. } => interest_id,
                UpdateResult::Rejected { .. } => continue,
            };
            if !ids.contains(id.as_str()) {
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
