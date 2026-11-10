//! 兴趣终态冷记录：正文不可变，根目录保留完整消费摘要；不启动观察或重放输入。
use crate::{store::JobRecord, strict_json};
use eve_interest_api::*;
use eve_memory_api::{InteractionEvidence, MemoryScope};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consumed {
    pub id: String,
    pub revision: u64,
    pub sha256: String,
}

pub(super) fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(super) fn consumed(evidence: &InteractionEvidence) -> InterestResult<Consumed> {
    Ok(Consumed {
        id: evidence.id.clone(),
        revision: evidence.revision,
        sha256: hash(&serde_json::to_vec(evidence).map_err(|_| InterestError::InvalidInput)?),
    })
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobRef {
    pub sequence: u64,
    pub id: String,
    pub scope: MemoryScope,
    pub status: JobStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub memory_revision: u64,
    pub sha256: String,
    pub bytes: usize,
    pub consumed: Vec<Consumed>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format_version: u32,
    record: JobRecord,
}

pub(super) fn encode(record: &JobRecord) -> InterestResult<(JobRef, Vec<u8>)> {
    if record.sequence == 0 || record.job.status == JobStatus::Running {
        return Err(InterestError::CorruptState);
    }
    let bytes = serde_json::to_vec(&Envelope {
        format_version: 1,
        record: record.clone(),
    })
    .map_err(|_| InterestError::InvalidInput)?;
    if bytes.len() > MAX_JOB_ARCHIVE_RECORD_BYTES {
        return Err(InterestError::LimitReached);
    }
    let job = &record.job;
    let reference = JobRef {
        sequence: record.sequence,
        id: job.batch.id.clone(),
        scope: job.batch.scope.clone(),
        status: job.status.clone(),
        started_at_ms: job.batch.started_at_ms,
        finished_at_ms: job.finished_at_ms,
        memory_revision: record.memory_revision,
        sha256: hash(&bytes),
        bytes: bytes.len(),
        consumed: job
            .batch
            .evidence
            .iter()
            .map(consumed)
            .collect::<InterestResult<_>>()?,
    };
    Ok((reference, bytes))
}

pub(super) fn load(
    store: &dyn InterestJobArchiveStore,
    reference: &JobRef,
) -> InterestResult<JobRecord> {
    if reference.bytes == 0
        || reference.bytes > MAX_JOB_ARCHIVE_RECORD_BYTES
        || reference.sha256.len() != 64
        || !reference
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(InterestError::CorruptState);
    }
    let bytes = store.get(&reference.sha256)?;
    if bytes.len() != reference.bytes || hash(&bytes) != reference.sha256 {
        return Err(InterestError::CorruptState);
    }
    let value = strict_json::from_slice(&bytes).map_err(|_| InterestError::CorruptState)?;
    let envelope: Envelope =
        serde_json::from_value(value).map_err(|_| InterestError::CorruptState)?;
    if envelope.format_version != 1 {
        return Err(InterestError::UnsupportedVersion);
    }
    let (actual, _) = encode(&envelope.record).map_err(|_| InterestError::CorruptState)?;
    if actual != *reference {
        return Err(InterestError::CorruptState);
    }
    Ok(envelope.record)
}
