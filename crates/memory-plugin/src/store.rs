use crate::{
    MEMORY_STATE_KEY,
    archive::{ArchiveConfig, ChunkRecord, encode_chunk, load_chunk, now_ms},
    strict_json,
};
use eve_memory_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionTurnStatus;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

/// 格式 1：全部证据都在热层。格式 2：出现过归档后写入，含分块索引；
/// 旧版程序读取格式 2 时按未知版本拒绝启动，不会误判为空或清空。
const ARCHIVED_FORMAT_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    scopes: Vec<ScopeRecord>,
    /// 下一个分块序号；从未归档时为 0 且不写出，保持格式 1 原样。
    #[serde(default, skip_serializing_if = "is_zero")]
    next_chunk: u64,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeRecord {
    snapshot: MemorySnapshot,
    operations: Vec<Operation>,
    /// 本作用域的分块索引，按序号升序，修订段同样递增。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archive: Vec<ChunkRecord>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    revision: u64,
    change: PreferenceChange,
}
/// 导入时用于核对归档中同一轮次的读取器；启动重放不读分块。
type ArchivedLoader<'a> = &'a dyn Fn(&ChunkRecord) -> MemoryResult<Vec<InteractionEvidence>>;
impl ScopeRecord {
    fn empty(scope: MemoryScope) -> Self {
        Self {
            snapshot: MemorySnapshot {
                scope,
                revision: 0,
                evidence: vec![],
                preferences: vec![],
                archived_evidence: 0,
            },
            operations: vec![],
            archive: vec![],
        }
    }
    fn revision(&self, expected: u64) -> MemoryResult<u64> {
        if self.snapshot.revision != expected {
            return Err(MemoryError::StaleRevision);
        }
        expected.checked_add(1).ok_or(MemoryError::LimitReached)
    }
    fn import(
        &mut self,
        expected: u64,
        mut evidence: InteractionEvidence,
        archived: ArchivedLoader<'_>,
    ) -> MemoryResult<bool> {
        validate_evidence(&evidence)?;
        if let Some(old) = self
            .snapshot
            .evidence
            .iter()
            .find(|old| old.id == evidence.id)
        {
            return if same_source(&old.source, &evidence.source) {
                Ok(false)
            } else {
                Err(MemoryError::Conflict)
            };
        }
        reject_origin_alias(&self.snapshot.evidence, &evidence.source)?;
        if let EvidenceSource::CompletedInteraction { turn_id, .. } = evidence.source {
            // 已归档的同一轮次：相同来源重放视为已保存，其余均为冲突。
            for chunk in &self.archive {
                if !(chunk.first_turn..=chunk.last_turn).contains(&turn_id) {
                    continue;
                }
                for old in archived(chunk)? {
                    if old.id == evidence.id && same_source(&old.source, &evidence.source) {
                        return Ok(false);
                    }
                    if old.id == evidence.id
                        || reject_origin_alias(std::slice::from_ref(&old), &evidence.source)
                            .is_err()
                    {
                        return Err(MemoryError::Conflict);
                    }
                }
            }
        }
        let revision = self.revision(expected)?;
        evidence.revision = revision;
        self.snapshot.evidence.push(evidence);
        self.snapshot.revision = revision;
        Ok(true)
    }
    fn change(&mut self, expected: u64, change: PreferenceChange) -> MemoryResult<bool> {
        validate_change(&change)?;
        if let Some(old) = self
            .operations
            .iter()
            .find(|old| old.change.operation_id == change.operation_id)
        {
            return if same_change(&old.change, &change) {
                Ok(false)
            } else {
                Err(MemoryError::Conflict)
            };
        }
        let revision = self.revision(expected)?;
        let evidence_id = match &change.evidence {
            PreferenceEvidence::Existing(id) => {
                if !self
                    .snapshot
                    .evidence
                    .iter()
                    .any(|evidence| evidence.id == *id)
                {
                    return Err(MemoryError::NotFound);
                }
                id.clone()
            }
            PreferenceEvidence::Statement(statement) => {
                let source = EvidenceSource::UserStatement {
                    message_id: statement.message_id.clone(),
                    text: statement.text.clone(),
                };
                if let Some(old) = self
                    .snapshot
                    .evidence
                    .iter()
                    .find(|value| value.id == statement.evidence_id)
                {
                    if !same_source(&old.source, &source) {
                        return Err(MemoryError::Conflict);
                    }
                } else {
                    reject_origin_alias(&self.snapshot.evidence, &source)?;
                    self.snapshot.evidence.push(InteractionEvidence {
                        id: statement.evidence_id.clone(),
                        revision,
                        at_ms: statement.at_ms,
                        source,
                    });
                }
                statement.evidence_id.clone()
            }
        };
        let (id, text) = match &change.action {
            PreferenceAction::Confirm { id, text } | PreferenceAction::Correct { id, text } => {
                (id, Some(text))
            }
            PreferenceAction::Revoke { id } => (id, None),
        };
        let index = self
            .snapshot
            .preferences
            .iter()
            .position(|value| value.id == *id);
        match (&change.action, index) {
            (PreferenceAction::Confirm { .. }, Some(_)) => return Err(MemoryError::Conflict),
            (PreferenceAction::Confirm { text, .. }, None) => {
                self.snapshot.preferences.push(Preference {
                    id: id.clone(),
                    text: text.clone(),
                    status: PreferenceStatus::Confirmed,
                    revision: 0,
                    history: vec![],
                });
            }
            (_, None) => return Err(MemoryError::NotFound),
            (_, Some(index))
                if self.snapshot.preferences[index].status != PreferenceStatus::Confirmed =>
            {
                return Err(MemoryError::Conflict);
            }
            _ => {}
        }
        let index = index.unwrap_or(self.snapshot.preferences.len() - 1);
        let preference = &mut self.snapshot.preferences[index];
        preference.revision = preference
            .revision
            .checked_add(1)
            .ok_or(MemoryError::LimitReached)?;
        if let Some(text) = text {
            preference.text = text.clone();
        }
        if matches!(change.action, PreferenceAction::Revoke { .. }) {
            preference.status = PreferenceStatus::Revoked;
        }
        preference.history.push(PreferenceVersion {
            revision: preference.revision,
            evidence_id,
            at_ms: change.at_ms,
            text: preference.text.clone(),
            status: preference.status.clone(),
        });
        self.operations.push(Operation { revision, change });
        self.snapshot.revision = revision;
        Ok(true)
    }
}

struct Inner {
    document: Document,
    context: Option<PluginContext>,
    archive: Option<ArchiveConfig>,
    /// 存储中存在但索引未引用的分块名；只用于报告，不删除。
    unreferenced: BTreeSet<String>,
    last_error: Option<String>,
    /// 读取时发现分块损坏或缺失后不再新建分块，保留现场等待人工核对。
    archive_blocked: bool,
    hot_bytes: usize,
}
pub(super) struct StoredMemory {
    inner: Mutex<Inner>,
}
impl StoredMemory {
    pub(super) fn open(
        context: PluginContext,
        archive: Option<ArchiveConfig>,
    ) -> MemoryResult<Self> {
        if let Some(config) = &archive {
            config.policy.validate()?;
        }
        let (document, hot_bytes) = match context
            .state_get(MEMORY_STATE_KEY)
            .map_err(|_| MemoryError::Storage)?
        {
            None => (
                Document {
                    format_version: MEMORY_FORMAT_VERSION,
                    scopes: vec![],
                    next_chunk: 0,
                },
                0,
            ),
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(MemoryError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| MemoryError::CorruptState)?;
                let document: Document =
                    serde_json::from_value(value).map_err(|_| MemoryError::CorruptState)?;
                match document.format_version {
                    MEMORY_FORMAT_VERSION if document.next_chunk == 0 => {}
                    ARCHIVED_FORMAT_VERSION if document.next_chunk > 0 => {}
                    MEMORY_FORMAT_VERSION | ARCHIVED_FORMAT_VERSION => {
                        return Err(MemoryError::CorruptState);
                    }
                    _ => return Err(MemoryError::UnsupportedVersion),
                }
                validate_document(&document).map_err(|_| MemoryError::CorruptState)?;
                (document, bytes.len())
            }
        };
        let referenced: BTreeSet<String> = document
            .scopes
            .iter()
            .flat_map(|record| record.archive.iter().map(ChunkRecord::name))
            .collect();
        let mut unreferenced = BTreeSet::new();
        match &archive {
            // 已有归档却未装配存储时拒绝启动，避免把缺失的历史当作不存在。
            None if !referenced.is_empty() => return Err(MemoryError::Unavailable),
            None => {}
            Some(config) => {
                let present: BTreeSet<String> = config.store.list()?.into_iter().collect();
                if !referenced.is_subset(&present) {
                    return Err(MemoryError::CorruptState);
                }
                unreferenced = present.difference(&referenced).cloned().collect();
            }
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                document,
                context: Some(context),
                archive,
                unreferenced,
                last_error: None,
                archive_blocked: false,
                hot_bytes,
            }),
        })
    }
    fn lock(&self) -> MemoryResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        Ok(inner)
    }
    pub(super) fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
        Ok(self
            .lock()?
            .document
            .scopes
            .iter()
            .map(|record| record.snapshot.scope.clone())
            .collect())
    }
    pub(super) fn snapshot(&self, scope: &MemoryScope) -> MemoryResult<MemorySnapshot> {
        let inner = self.lock()?;
        Ok(inner
            .document
            .scopes
            .iter()
            .find(|value| value.snapshot.scope == *scope)
            .map(|value| value.snapshot.clone())
            .unwrap_or_else(|| ScopeRecord::empty(scope.clone()).snapshot))
    }
    pub(super) fn archived(
        &self,
        scope: &MemoryScope,
        after: u64,
        limit: usize,
    ) -> MemoryResult<ArchivedEvidencePage> {
        validate_archive_page_limit(limit)?;
        let (chunks, store) = {
            let inner = self.lock()?;
            let chunks: Vec<ChunkRecord> = inner
                .document
                .scopes
                .iter()
                .find(|value| value.snapshot.scope == *scope)
                .map(|record| {
                    record
                        .archive
                        .iter()
                        .filter(|chunk| chunk.last_revision() > after)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if chunks.is_empty() {
                return Ok(ArchivedEvidencePage {
                    evidence: vec![],
                    next_after: None,
                });
            }
            let store = inner
                .archive
                .as_ref()
                .ok_or(MemoryError::Unavailable)?
                .store
                .clone();
            (chunks, store)
        };
        // 读分块时不持锁，避免慢存储阻塞写入；分块不可变，索引只追加。
        let mut evidence = Vec::new();
        for chunk in &chunks {
            let loaded = match load_chunk(store.as_ref(), scope, chunk) {
                Ok(loaded) => loaded,
                Err(error) => {
                    self.block_archive(&error);
                    return Err(error);
                }
            };
            for item in loaded.into_iter().filter(|item| item.revision > after) {
                evidence.push(item);
                if evidence.len() == limit {
                    let next_after = evidence.last().map(|item| item.revision);
                    return Ok(ArchivedEvidencePage {
                        evidence,
                        next_after,
                    });
                }
            }
        }
        Ok(ArchivedEvidencePage {
            evidence,
            next_after: None,
        })
    }
    fn block_archive(&self, error: &MemoryError) {
        if let Ok(mut inner) = self.inner.lock()
            && matches!(error, MemoryError::CorruptState)
        {
            inner.archive_blocked = true;
            inner.last_error = Some("归档分块校验失败；已停止新建分块，保留现场".into());
        }
    }
    pub(super) fn archive_status(&self) -> MemoryResult<MemoryArchiveStatus> {
        let inner = self.lock()?;
        let chunks = inner.document.scopes.iter().flat_map(|r| &r.archive);
        Ok(MemoryArchiveStatus {
            enabled: inner.archive.is_some(),
            hot_evidence: inner
                .document
                .scopes
                .iter()
                .map(|record| record.snapshot.evidence.len())
                .sum(),
            hot_bytes: inner.hot_bytes,
            chunks: chunks.clone().count() as u64,
            archived_evidence: chunks.clone().map(|chunk| chunk.count).sum(),
            archived_bytes: chunks.map(|chunk| chunk.bytes).sum(),
            unreferenced_chunks: inner.unreferenced.len(),
            last_error: inner.last_error.clone(),
        })
    }
    pub(super) fn import_completed(
        &self,
        scope: &MemoryScope,
        expected: u64,
        interaction: CompletedInteraction,
    ) -> MemoryResult<MemorySnapshot> {
        self.mutate(scope, |record, archived| {
            interaction
                .snapshot
                .validate()
                .map_err(|_| MemoryError::InvalidInput)?;
            if interaction.snapshot.key.session_id != scope.session_id
                || interaction.snapshot.key.user_id != scope.user_id
            {
                return Err(MemoryError::InvalidInput);
            }
            let turn = interaction
                .snapshot
                .turns
                .iter()
                .find(|turn| turn.id == interaction.turn_id)
                .ok_or(MemoryError::InvalidInput)?;
            let SessionTurnStatus::Completed { messages } = &turn.status else {
                return Err(MemoryError::InvalidInput);
            };
            let reply = messages
                .last()
                .and_then(|message| message.text.as_ref())
                .ok_or(MemoryError::InvalidInput)?;
            record.import(
                expected,
                InteractionEvidence {
                    id: interaction.evidence_id,
                    revision: 0,
                    at_ms: interaction.at_ms,
                    source: EvidenceSource::CompletedInteraction {
                        message_id: interaction.message_id,
                        session_revision: interaction.snapshot.revision,
                        turn_id: interaction.turn_id,
                        user_text: turn.input.clone(),
                        assistant_text: reply.clone(),
                    },
                },
                archived,
            )
        })
    }
    pub(super) fn update_preference(
        &self,
        scope: &MemoryScope,
        expected: u64,
        change: PreferenceChange,
    ) -> MemoryResult<MemorySnapshot> {
        self.mutate(scope, |record, _| record.change(expected, change))
    }
    fn mutate(
        &self,
        scope: &MemoryScope,
        apply: impl FnOnce(&mut ScopeRecord, ArchivedLoader<'_>) -> MemoryResult<bool>,
    ) -> MemoryResult<MemorySnapshot> {
        let mut inner = self.lock()?;
        scope.validate()?;
        let mut next = inner.document.clone();
        let index = next
            .scopes
            .iter()
            .position(|value| value.snapshot.scope == *scope)
            .unwrap_or_else(|| {
                next.scopes.push(ScopeRecord::empty(scope.clone()));
                next.scopes.len() - 1
            });
        let store = inner.archive.as_ref().map(|config| config.store.clone());
        let loader = |chunk: &ChunkRecord| match &store {
            Some(store) => load_chunk(store.as_ref(), scope, chunk),
            None => Err(MemoryError::Unavailable),
        };
        let changed = apply(&mut next.scopes[index], &loader)?;
        if !changed {
            return Ok(next.scopes[index].snapshot.clone());
        }
        let mut bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::InvalidInput)?;
        let mut archived_name = None;
        if let Some(config) = inner.archive.clone()
            && !inner.archive_blocked
        {
            match archive_step(&mut next, &config, bytes.len()) {
                Ok(Some(name)) => {
                    bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::InvalidInput)?;
                    archived_name = Some(name);
                }
                Ok(None) => {}
                // 分块未写成时热层保持原样；仍在硬上限内就照常提交新记录。
                Err(_) => {
                    inner.last_error = Some("写入归档分块失败；热层未变化".into());
                }
            }
        }
        validate_limits(&next)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(MemoryError::LimitReached);
        }
        // 后端可能已提交后才返回错误；不允许旧缓存继续读取或再覆盖实际磁盘状态。
        // 分块已先行持久，提交不确定时它最多成为未被引用的分块，不会丢失证据。
        let context = inner.context.as_ref().ok_or(MemoryError::Unavailable)?;
        if context.state_set(MEMORY_STATE_KEY, bytes.clone()).is_err() {
            inner.context = None;
            return Err(MemoryError::Storage);
        }
        if let Some(name) = archived_name {
            inner.unreferenced.remove(&name);
            inner.last_error = None;
        }
        inner.hot_bytes = bytes.len();
        let snapshot = next.scopes[index].snapshot.clone();
        inner.document = next;
        Ok(snapshot)
    }
    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("交互记忆状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

/// 热层超过阈值时，把一批最早的合格完成交互写成分块并从热层移除。
/// 分块先持久写入；只有成功后才改动 `next`，失败时 `next` 保持原样。
fn archive_step(
    next: &mut Document,
    config: &ArchiveConfig,
    hot_bytes: usize,
) -> MemoryResult<Option<String>> {
    let policy = &config.policy;
    let hot: usize = next.scopes.iter().map(|r| r.snapshot.evidence.len()).sum();
    if hot <= policy.trigger_evidence && hot_bytes <= policy.trigger_bytes {
        return Ok(None);
    }
    let pressure = hot >= policy.pressure_evidence || hot_bytes >= policy.pressure_bytes;
    let now = now_ms();
    let mut best: Option<(u64, usize, Vec<usize>)> = None;
    for (scope_index, record) in next.scopes.iter().enumerate() {
        let referenced: BTreeSet<&str> = record
            .snapshot
            .preferences
            .iter()
            .flat_map(|p| p.history.iter().map(|v| v.evidence_id.as_str()))
            .collect();
        let mut completed: Vec<usize> = record
            .snapshot
            .evidence
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.source, EvidenceSource::CompletedInteraction { .. }))
            .map(|(i, _)| i)
            .collect();
        completed.sort_by_key(|&i| record.snapshot.evidence[i].revision);
        completed.truncate(completed.len().saturating_sub(policy.keep_recent));
        // 分块修订段按序号递增，读取时可按页提前结束；更早的残留只留在热层。
        let floor = record.archive.last().map_or(0, ChunkRecord::last_revision);
        let eligible: Vec<usize> = completed
            .into_iter()
            .filter(|&i| {
                let e = &record.snapshot.evidence[i];
                e.revision > floor
                    && !referenced.contains(e.id.as_str())
                    && (pressure || e.at_ms.saturating_add(policy.min_age_ms) <= now)
            })
            .collect();
        let minimum = if pressure { 1 } else { policy.min_chunk };
        let Some(&first) = eligible.first() else {
            continue;
        };
        if eligible.len() < minimum {
            continue;
        }
        let oldest = record.snapshot.evidence[first].at_ms;
        if best.as_ref().is_none_or(|(at, _, _)| oldest < *at) {
            best = Some((oldest, scope_index, eligible));
        }
    }
    let Some((_, scope_index, eligible)) = best else {
        return Ok(None);
    };
    let record = &next.scopes[scope_index];
    let mut chosen = Vec::new();
    let mut size = 0;
    for &i in eligible.iter().take(policy.max_chunk_evidence) {
        let entry = &record.snapshot.evidence[i];
        let entry_size = serde_json::to_vec(entry)
            .map_err(|_| MemoryError::InvalidInput)?
            .len()
            + 1;
        if !chosen.is_empty() && size + entry_size > policy.max_chunk_bytes {
            break;
        }
        size += entry_size;
        chosen.push(entry.clone());
    }
    let sequence = next.next_chunk;
    let (chunk, bytes) = encode_chunk(sequence, &record.snapshot.scope, chosen)?;
    let name = chunk.name();
    config.store.put(&name, &bytes)?;
    let record = &mut next.scopes[scope_index];
    let moved: BTreeSet<u64> = record
        .snapshot
        .evidence
        .iter()
        .filter(|e| chunk.contains_revision(e.revision))
        .map(|e| e.revision)
        .collect();
    if moved.len() as u64 != chunk.count {
        return Err(MemoryError::CorruptState);
    }
    record
        .snapshot
        .evidence
        .retain(|e| !moved.contains(&e.revision));
    record.snapshot.archived_evidence += chunk.count;
    record.archive.push(chunk);
    next.next_chunk = sequence + 1;
    next.format_version = ARCHIVED_FORMAT_VERSION;
    Ok(Some(name))
}

fn validate_change(change: &PreferenceChange) -> MemoryResult<()> {
    validate_id(&change.operation_id)?;
    match &change.action {
        PreferenceAction::Confirm { id, text } | PreferenceAction::Correct { id, text } => {
            validate_id(id)?;
            validate_text(text, MAX_PREFERENCE_BYTES)?;
        }
        PreferenceAction::Revoke { id } => validate_id(id)?,
    }
    match &change.evidence {
        PreferenceEvidence::Existing(id) => validate_id(id)?,
        PreferenceEvidence::Statement(statement) => {
            validate_id(&statement.evidence_id)?;
            validate_id(&statement.message_id)?;
            validate_text(&statement.text, MAX_TEXT_BYTES)?;
        }
    }
    Ok(())
}
pub(crate) fn validate_evidence(evidence: &InteractionEvidence) -> MemoryResult<()> {
    validate_id(&evidence.id)?;
    match &evidence.source {
        EvidenceSource::UserStatement { message_id, text } => {
            validate_id(message_id)?;
            validate_text(text, MAX_TEXT_BYTES)?;
        }
        EvidenceSource::CompletedInteraction {
            message_id,
            session_revision,
            turn_id,
            user_text,
            assistant_text,
        } => {
            validate_id(message_id)?;
            validate_text(user_text, MAX_TEXT_BYTES)?;
            validate_text(assistant_text, MAX_TEXT_BYTES)?;
            if *turn_id == 0
                || turn_id
                    .checked_mul(2)
                    .is_none_or(|minimum| *session_revision < minimum)
            {
                return Err(MemoryError::InvalidInput);
            }
        }
    }
    Ok(())
}
fn source_message(source: &EvidenceSource) -> &str {
    match source {
        EvidenceSource::UserStatement { message_id, .. }
        | EvidenceSource::CompletedInteraction { message_id, .. } => message_id,
    }
}
fn reject_origin_alias(
    evidence: &[InteractionEvidence],
    source: &EvidenceSource,
) -> MemoryResult<()> {
    for previous in evidence {
        if source_message(&previous.source) == source_message(source)
            || matches!((&previous.source, source),
            (EvidenceSource::CompletedInteraction { turn_id: before, .. }, EvidenceSource::CompletedInteraction { turn_id: after, .. }) if before == after)
        {
            return Err(MemoryError::Conflict);
        }
    }
    Ok(())
}
fn same_source(first: &EvidenceSource, second: &EvidenceSource) -> bool {
    match (first, second) {
        (
            EvidenceSource::UserStatement {
                message_id: a_id,
                text: a_text,
            },
            EvidenceSource::UserStatement {
                message_id: b_id,
                text: b_text,
            },
        ) => a_id == b_id && a_text == b_text,
        (
            EvidenceSource::CompletedInteraction {
                message_id: a_id,
                turn_id: a_turn,
                user_text: a_user,
                assistant_text: a_assistant,
                ..
            },
            EvidenceSource::CompletedInteraction {
                message_id: b_id,
                turn_id: b_turn,
                user_text: b_user,
                assistant_text: b_assistant,
                ..
            },
        ) => a_id == b_id && a_turn == b_turn && a_user == b_user && a_assistant == b_assistant,
        _ => false,
    }
}
fn same_change(first: &PreferenceChange, second: &PreferenceChange) -> bool {
    let mut first = first.clone();
    let mut second = second.clone();
    first.at_ms = 0;
    second.at_ms = 0;
    for change in [&mut first, &mut second] {
        if let PreferenceEvidence::Statement(value) = &mut change.evidence {
            value.at_ms = 0;
        }
    }
    first == second
}
fn validate_limits(document: &Document) -> MemoryResult<()> {
    let evidence: usize = document
        .scopes
        .iter()
        .map(|record| record.snapshot.evidence.len())
        .sum();
    let preferences: usize = document
        .scopes
        .iter()
        .map(|record| record.snapshot.preferences.len())
        .sum();
    let history: usize = document
        .scopes
        .iter()
        .flat_map(|record| &record.snapshot.preferences)
        .map(|preference| preference.history.len())
        .sum();
    let operations: usize = document
        .scopes
        .iter()
        .map(|record| record.operations.len())
        .sum();
    if document.scopes.len() > MAX_SCOPES
        || evidence > MAX_EVIDENCE
        || preferences > MAX_PREFERENCES
        || history > MAX_HISTORY
        || operations > MAX_HISTORY
    {
        return Err(MemoryError::LimitReached);
    }
    Ok(())
}
fn validate_document(document: &Document) -> MemoryResult<()> {
    validate_limits(document)?;
    let mut scopes = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    for record in &document.scopes {
        record.snapshot.scope.validate()?;
        let archived = record.snapshot.archived_evidence;
        if !scopes.insert(&record.snapshot.scope)
            || record.snapshot.revision == 0
            || record.snapshot.revision > (MAX_EVIDENCE + MAX_HISTORY) as u64 + archived
            || record.archive.iter().map(|chunk| chunk.count).sum::<u64>() != archived
        {
            return Err(MemoryError::CorruptState);
        }
        let mut floor = 0;
        for chunk in &record.archive {
            chunk.validate()?;
            if chunk.sequence >= document.next_chunk
                || !sequences.insert(chunk.sequence)
                || chunk.revisions[0][0] <= floor
                || chunk.last_revision() > record.snapshot.revision
            {
                return Err(MemoryError::CorruptState);
            }
            floor = chunk.last_revision();
        }
        let operations: std::collections::BTreeMap<u64, &Operation> = record
            .operations
            .iter()
            .map(|operation| (operation.revision, operation))
            .collect();
        let evidence: std::collections::BTreeMap<u64, &InteractionEvidence> = record
            .snapshot
            .evidence
            .iter()
            .map(|value| (value.revision, value))
            .collect();
        if operations.len() != record.operations.len()
            || evidence.len() != record.snapshot.evidence.len()
        {
            return Err(MemoryError::CorruptState);
        }
        let mut runs = record
            .archive
            .iter()
            .flat_map(|chunk| chunk.revisions.iter());
        let mut run = runs.next();
        // 从不可删除的来源与操作逐修订重放，校验任何快照字段、证据引用和历史；
        // 已归档修订只核对占位，原文在读取分块时逐项核对。
        let mut replay = ScopeRecord::empty(record.snapshot.scope.clone());
        for revision in 1..=record.snapshot.revision {
            while run.is_some_and(|[_, end]| *end < revision) {
                run = runs.next();
            }
            if run.is_some_and(|[start, _]| *start <= revision) {
                if operations.contains_key(&revision) || evidence.contains_key(&revision) {
                    return Err(MemoryError::CorruptState);
                }
                replay.snapshot.revision = revision;
                continue;
            }
            let changed = if let Some(operation) = operations.get(&revision) {
                replay.change(revision - 1, operation.change.clone())?
            } else {
                let evidence = evidence.get(&revision).ok_or(MemoryError::CorruptState)?;
                if !matches!(evidence.source, EvidenceSource::CompletedInteraction { .. }) {
                    return Err(MemoryError::CorruptState);
                }
                replay.import(revision - 1, (*evidence).clone(), &|_| Ok(vec![]))?
            };
            if !changed {
                return Err(MemoryError::CorruptState);
            }
        }
        replay.snapshot.archived_evidence = archived;
        replay.archive = record.archive.clone();
        if replay != *record {
            return Err(MemoryError::CorruptState);
        }
    }
    Ok(())
}
