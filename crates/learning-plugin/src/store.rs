use crate::strict_json;
use eve_learning_api::*;
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
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobRecord {
    job: LearningJob,
    /// 只保存批次来源的修订，不复制偏好和未消费的原始记忆。
    memory_revision: u64,
}
struct Inner {
    document: Document,
    context: Option<PluginContext>,
}
pub(super) struct StoredLearning {
    inner: Mutex<Inner>,
}
impl StoredLearning {
    pub(super) fn open(context: PluginContext) -> LearningResult<Self> {
        let mut document = match context
            .state_get(LEARNING_STATE_KEY)
            .map_err(|_| LearningError::Storage)?
        {
            None => Document {
                format_version: FORMAT_VERSION,
                jobs: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(LearningError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| LearningError::CorruptState)?;
                let document: Document =
                    serde_json::from_value(value).map_err(|_| LearningError::CorruptState)?;
                if document.format_version != FORMAT_VERSION {
                    return Err(LearningError::UnsupportedVersion);
                }
                validate_document(&document).map_err(|_| LearningError::CorruptState)?;
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
        // 先保存中断结局再公开实例；从不重试已经消费的输入，也不伪造完成时钟。
        if interrupted {
            let bytes = encode(&document)?;
            context
                .state_set(LEARNING_STATE_KEY, bytes)
                .map_err(|_| LearningError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                document,
                context: Some(context),
            }),
        })
    }
    fn lock(&self) -> LearningResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| LearningError::Unavailable)?;
        if inner.context.is_none() {
            return Err(LearningError::Unavailable);
        }
        Ok(inner)
    }
    pub(super) fn snapshot(&self, scope: &MemoryScope) -> LearningResult<LearningSnapshot> {
        let inner = self.lock()?;
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        Ok(LearningSnapshot {
            scope: scope.clone(),
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
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        let mut inner = self.lock()?;
        options.validate()?;
        validate_id(extractor_version).map_err(|_| LearningError::InvalidInput)?;
        validate_memory(memory)?;
        let previous: Vec<_> = inner
            .document
            .jobs
            .iter()
            .filter(|record| record.job.batch.scope == memory.scope)
            .collect();
        let mut consumed = BTreeSet::new();
        for record in &previous {
            if memory.revision < record.memory_revision {
                return Err(LearningError::Conflict);
            }
            for evidence in &record.job.batch.evidence {
                if !memory.evidence.iter().any(|value| value == evidence) {
                    return Err(LearningError::Conflict);
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
        let mut batch = LearningBatch {
            // 摘要长度固定；先用等长占位值计算完整输入 JSON 的预算。
            id: format!("learning-batch-{}", "0".repeat(64)),
            scope: memory.scope.clone(),
            extractor_version: extractor_version.into(),
            started_at_ms: now_ms,
            evidence: vec![],
        };
        for next in available {
            let size = serde_json::to_vec(next)
                .map_err(|_| LearningError::InvalidInput)?
                .len();
            if size > MAX_EVIDENCE_BYTES {
                continue;
            }
            batch.evidence.push(next.clone());
            if serde_json::to_vec(&batch)
                .map_err(|_| LearningError::InvalidInput)?
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
            return Err(LearningError::LimitReached);
        }
        batch.id = batch_id(&memory.scope, extractor_version, &batch.evidence)?;
        let mut next = inner.document.clone();
        next.jobs.push(JobRecord {
            memory_revision: memory.revision,
            job: LearningJob {
                batch: batch.clone(),
                status: JobStatus::Running,
                finished_at_ms: None,
                candidates: vec![],
            },
        });
        persist(&mut inner, next)?;
        Ok(Some(batch))
    }
    pub(super) fn finish(
        &self,
        batch: &LearningBatch,
        at_ms: u64,
        outcome: LearningOutcome,
    ) -> LearningResult<()> {
        let mut inner = self.lock()?;
        let index = inner
            .document
            .jobs
            .iter()
            .position(|record| record.job.batch.id == batch.id)
            .ok_or(LearningError::Conflict)?;
        let old = &inner.document.jobs[index].job;
        if old.batch != *batch {
            return Err(LearningError::Conflict);
        }
        if old.status != JobStatus::Running {
            let replay = match (&old.status, &outcome) {
                (JobStatus::Completed, LearningOutcome::Completed(drafts)) => {
                    old.candidates.len() == drafts.len()
                        && old
                            .candidates
                            .iter()
                            .zip(drafts)
                            .all(|(candidate, draft)| candidate.draft == *draft)
                }
                (JobStatus::Failed(first), LearningOutcome::Failed(second)) => first == second,
                _ => false,
            };
            return if replay {
                Ok(())
            } else {
                Err(LearningError::Conflict)
            };
        }
        if let LearningOutcome::Completed(drafts) = &outcome {
            validate_drafts(batch, drafts)?;
        }
        if at_ms < batch.started_at_ms {
            return Err(LearningError::InvalidInput);
        }
        let mut next = inner.document.clone();
        let job = &mut next.jobs[index].job;
        job.finished_at_ms = Some(at_ms);
        match outcome {
            LearningOutcome::Failed(reason) => job.status = JobStatus::Failed(reason),
            LearningOutcome::Completed(drafts) => {
                let expires_at_ms = at_ms
                    .checked_add(CANDIDATE_TTL_MS)
                    .ok_or(LearningError::InvalidInput)?;
                job.status = JobStatus::Completed;
                job.candidates = drafts
                    .into_iter()
                    .enumerate()
                    .map(|(index, draft)| PreferenceCandidate {
                        id: candidate_id(&batch.id, index),
                        batch_id: batch.id.clone(),
                        draft,
                        created_at_ms: at_ms,
                        expires_at_ms,
                    })
                    .collect();
            }
        }
        persist(&mut inner, next)
    }
    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("偏好提炼状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn persist(inner: &mut Inner, next: Document) -> LearningResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(LearningError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(LEARNING_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(LearningError::Storage);
    }
    inner.document = next;
    Ok(())
}
fn encode(document: &Document) -> LearningResult<Vec<u8>> {
    if document.jobs.len() > MAX_JOBS {
        return Err(LearningError::LimitReached);
    }
    let bytes = serde_json::to_vec(document).map_err(|_| LearningError::InvalidInput)?;
    // Interrupted 比 Running 多四个 ASCII 字节。写入时保留恢复空间，避免状态
    // 刚好填满后，合法的中断标记反而无法在下次启动时原子保存。
    let recovery_bytes = document
        .jobs
        .iter()
        .filter(|record| record.job.status == JobStatus::Running)
        .count()
        * ("Interrupted".len() - "Running".len());
    if bytes.len().saturating_add(recovery_bytes) > MAX_STATE_BYTES {
        return Err(LearningError::LimitReached);
    }
    Ok(bytes)
}

/// 验证模型候选；不执行模型调用，不修改持久状态或确认偏好。
pub fn validate_drafts(batch: &LearningBatch, drafts: &[CandidateDraft]) -> LearningResult<()> {
    if drafts.len() > MAX_CANDIDATES {
        return Err(LearningError::InvalidInput);
    }
    let evidence_ids: BTreeSet<_> = batch.evidence.iter().map(|item| &item.id).collect();
    for draft in drafts {
        validate_text(&draft.text, MAX_CANDIDATE_BYTES).map_err(|_| LearningError::InvalidInput)?;
        if draft.confidence > 100 || draft.evidence_ids.is_empty() {
            return Err(LearningError::InvalidInput);
        }
        let mut references = BTreeSet::new();
        for id in &draft.evidence_ids {
            if !evidence_ids.contains(id) || !references.insert(id) {
                return Err(LearningError::InvalidInput);
            }
        }
    }
    Ok(())
}
fn batch_id(
    scope: &MemoryScope,
    version: &str,
    evidence: &[InteractionEvidence],
) -> LearningResult<String> {
    let stable_evidence: Vec<_> = evidence
        .iter()
        .map(|value| {
            let mut stable = value.clone();
            stable.at_ms = 0;
            stable
        })
        .collect();
    let bytes = serde_json::to_vec(&(scope, version, stable_evidence))
        .map_err(|_| LearningError::InvalidInput)?;
    Ok(hash_id("learning-batch-", &bytes))
}
fn candidate_id(batch_id: &str, index: usize) -> String {
    // 批次标识为固定十六进制摘要；分隔符和整数索引没有连接歧义。
    hash_id(
        "learning-candidate-",
        format!("{batch_id}:{index}").as_bytes(),
    )
}
fn hash_id(prefix: &str, bytes: &[u8]) -> String {
    let mut id = String::from(prefix);
    for byte in digest(&SHA256, bytes).as_ref() {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

fn validate_evidence(evidence: &InteractionEvidence) -> LearningResult<()> {
    validate_id(&evidence.id).map_err(|_| LearningError::InvalidInput)?;
    if evidence.revision == 0 {
        return Err(LearningError::InvalidInput);
    }
    let text_limit = eve_memory_api::MAX_TEXT_BYTES;
    match &evidence.source {
        EvidenceSource::UserStatement { message_id, text } => {
            validate_id(message_id).map_err(|_| LearningError::InvalidInput)?;
            validate_text(text, text_limit).map_err(|_| LearningError::InvalidInput)?;
        }
        EvidenceSource::CompletedInteraction {
            message_id,
            session_revision,
            turn_id,
            user_text,
            assistant_text,
        } => {
            validate_id(message_id).map_err(|_| LearningError::InvalidInput)?;
            validate_text(user_text, text_limit).map_err(|_| LearningError::InvalidInput)?;
            validate_text(assistant_text, text_limit).map_err(|_| LearningError::InvalidInput)?;
            if *turn_id == 0
                || turn_id
                    .checked_mul(2)
                    .is_none_or(|minimum| *session_revision < minimum)
            {
                return Err(LearningError::InvalidInput);
            }
        }
    }
    Ok(())
}
fn source_message(evidence: &InteractionEvidence) -> &str {
    match &evidence.source {
        EvidenceSource::UserStatement { message_id, .. }
        | EvidenceSource::CompletedInteraction { message_id, .. } => message_id,
    }
}
fn source_turn(evidence: &InteractionEvidence) -> Option<u64> {
    match evidence.source {
        EvidenceSource::CompletedInteraction { turn_id, .. } => Some(turn_id),
        _ => None,
    }
}
fn validate_memory(memory: &MemorySnapshot) -> LearningResult<()> {
    memory
        .scope
        .validate()
        .map_err(|_| LearningError::InvalidInput)?;
    if memory.evidence.len() > eve_memory_api::MAX_EVIDENCE
        || memory.revision > (eve_memory_api::MAX_EVIDENCE + eve_memory_api::MAX_HISTORY) as u64
    {
        return Err(LearningError::InvalidInput);
    }
    let mut ids = BTreeSet::new();
    let mut messages = BTreeSet::new();
    let mut turns = BTreeSet::new();
    let mut revisions = BTreeSet::new();
    for evidence in &memory.evidence {
        validate_evidence(evidence)?;
        if evidence.revision > memory.revision
            || !ids.insert(&evidence.id)
            || !messages.insert(source_message(evidence))
            || !revisions.insert(evidence.revision)
            || source_turn(evidence).is_some_and(|turn| !turns.insert(turn))
        {
            return Err(LearningError::InvalidInput);
        }
    }
    Ok(())
}
fn validate_batch(batch: &LearningBatch, memory_revision: u64) -> LearningResult<()> {
    batch
        .scope
        .validate()
        .map_err(|_| LearningError::InvalidInput)?;
    validate_id(&batch.extractor_version).map_err(|_| LearningError::InvalidInput)?;
    if memory_revision == 0
        || memory_revision > (eve_memory_api::MAX_EVIDENCE + eve_memory_api::MAX_HISTORY) as u64
        || batch.evidence.is_empty()
        || batch.evidence.len() > MAX_BATCH_EVIDENCE
        || batch.id != batch_id(&batch.scope, &batch.extractor_version, &batch.evidence)?
    {
        return Err(LearningError::InvalidInput);
    }
    let mut previous_revision = 0;
    for evidence in &batch.evidence {
        validate_evidence(evidence)?;
        if !matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
            || evidence.revision <= previous_revision
            || evidence.revision > memory_revision
            || serde_json::to_vec(evidence)
                .map_err(|_| LearningError::InvalidInput)?
                .len()
                > MAX_EVIDENCE_BYTES
        {
            return Err(LearningError::InvalidInput);
        }
        previous_revision = evidence.revision;
    }
    if serde_json::to_vec(batch)
        .map_err(|_| LearningError::InvalidInput)?
        .len()
        > MAX_INPUT_BYTES
    {
        return Err(LearningError::InvalidInput);
    }
    Ok(())
}
fn validate_document(document: &Document) -> LearningResult<()> {
    if document.jobs.len() > MAX_JOBS {
        return Err(LearningError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    let mut consumed_ids = BTreeSet::new();
    let mut consumed_messages = BTreeSet::new();
    let mut consumed_turns = BTreeSet::new();
    let mut consumed_revisions = BTreeSet::new();
    let mut previous_by_scope = BTreeMap::new();
    for record in &document.jobs {
        let job = &record.job;
        let batch = &job.batch;
        validate_batch(batch, record.memory_revision)?;
        if !ids.insert(&batch.id) {
            return Err(LearningError::CorruptState);
        }
        if let Some((started_at_ms, memory_revision)) =
            previous_by_scope.insert(&batch.scope, (batch.started_at_ms, record.memory_revision))
            && (batch.started_at_ms < started_at_ms || record.memory_revision < memory_revision)
        {
            return Err(LearningError::CorruptState);
        }
        for evidence in &batch.evidence {
            if !consumed_ids.insert((&batch.scope, &evidence.id))
                || !consumed_messages.insert((&batch.scope, source_message(evidence)))
                || !consumed_turns.insert((&batch.scope, source_turn(evidence)))
                || !consumed_revisions.insert((&batch.scope, evidence.revision))
            {
                return Err(LearningError::CorruptState);
            }
        }
        let drafts: Vec<_> = job
            .candidates
            .iter()
            .map(|item| item.draft.clone())
            .collect();
        validate_drafts(batch, &drafts)?;
        match &job.status {
            JobStatus::Running | JobStatus::Interrupted => {
                if job.finished_at_ms.is_some() || !job.candidates.is_empty() {
                    return Err(LearningError::CorruptState);
                }
            }
            JobStatus::Failed(_) | JobStatus::Completed => {
                let at_ms = job.finished_at_ms.ok_or(LearningError::CorruptState)?;
                if at_ms < batch.started_at_ms {
                    return Err(LearningError::CorruptState);
                }
                if job.status == JobStatus::Completed
                    && at_ms.checked_add(CANDIDATE_TTL_MS).is_none()
                {
                    return Err(LearningError::CorruptState);
                }
                if matches!(job.status, JobStatus::Failed(_)) && !job.candidates.is_empty() {
                    return Err(LearningError::CorruptState);
                }
                for (index, candidate) in job.candidates.iter().enumerate() {
                    if candidate.id != candidate_id(&batch.id, index)
                        || candidate.batch_id != batch.id
                        || candidate.created_at_ms != at_ms
                        || at_ms.checked_add(CANDIDATE_TTL_MS) != Some(candidate.expires_at_ms)
                    {
                        return Err(LearningError::CorruptState);
                    }
                }
            }
        }
    }
    Ok(())
}
