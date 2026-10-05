use crate::{MEMORY_STATE_KEY, strict_json};
use eve_memory_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionTurnStatus;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    scopes: Vec<ScopeRecord>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeRecord {
    snapshot: MemorySnapshot,
    operations: Vec<Operation>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    revision: u64,
    change: PreferenceChange,
}
impl ScopeRecord {
    fn empty(scope: MemoryScope) -> Self {
        Self {
            snapshot: MemorySnapshot {
                scope,
                revision: 0,
                evidence: vec![],
                preferences: vec![],
            },
            operations: vec![],
        }
    }
    fn revision(&self, expected: u64) -> MemoryResult<u64> {
        if self.snapshot.revision != expected {
            return Err(MemoryError::StaleRevision);
        }
        expected.checked_add(1).ok_or(MemoryError::LimitReached)
    }
    fn import(&mut self, expected: u64, mut evidence: InteractionEvidence) -> MemoryResult<bool> {
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
}
pub(super) struct StoredMemory {
    inner: Mutex<Inner>,
}
impl StoredMemory {
    pub(super) fn open(context: PluginContext) -> MemoryResult<Self> {
        let document = match context
            .state_get(MEMORY_STATE_KEY)
            .map_err(|_| MemoryError::Storage)?
        {
            None => Document {
                format_version: MEMORY_FORMAT_VERSION,
                scopes: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(MemoryError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| MemoryError::CorruptState)?;
                let document: Document =
                    serde_json::from_value(value).map_err(|_| MemoryError::CorruptState)?;
                if document.format_version != MEMORY_FORMAT_VERSION {
                    return Err(MemoryError::UnsupportedVersion);
                }
                validate_document(&document).map_err(|_| MemoryError::CorruptState)?;
                document
            }
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                document,
                context: Some(context),
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
    pub(super) fn import_completed(
        &self,
        scope: &MemoryScope,
        expected: u64,
        interaction: CompletedInteraction,
    ) -> MemoryResult<MemorySnapshot> {
        self.mutate(scope, |record| {
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
            )
        })
    }
    pub(super) fn update_preference(
        &self,
        scope: &MemoryScope,
        expected: u64,
        change: PreferenceChange,
    ) -> MemoryResult<MemorySnapshot> {
        self.mutate(scope, |record| record.change(expected, change))
    }
    fn mutate(
        &self,
        scope: &MemoryScope,
        apply: impl FnOnce(&mut ScopeRecord) -> MemoryResult<bool>,
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
        let changed = apply(&mut next.scopes[index])?;
        let snapshot = next.scopes[index].snapshot.clone();
        if !changed {
            return Ok(snapshot);
        }
        validate_limits(&next)?;
        let bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::InvalidInput)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(MemoryError::LimitReached);
        }
        // 后端可能已提交后才返回错误；不允许旧缓存继续读取或再覆盖实际磁盘状态。
        let context = inner.context.as_ref().ok_or(MemoryError::Unavailable)?;
        if context.state_set(MEMORY_STATE_KEY, bytes).is_err() {
            inner.context = None;
            return Err(MemoryError::Storage);
        }
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
fn validate_evidence(evidence: &InteractionEvidence) -> MemoryResult<()> {
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
    if document.scopes.len() > MAX_EVIDENCE
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
    for record in &document.scopes {
        record.snapshot.scope.validate()?;
        if !scopes.insert(&record.snapshot.scope)
            || record.snapshot.revision == 0
            || record.snapshot.revision > (MAX_EVIDENCE + MAX_HISTORY) as u64
        {
            return Err(MemoryError::CorruptState);
        }
        // 从不可删除的来源与操作逐修订重放，校验任何快照字段、证据引用和历史。
        let mut replay = ScopeRecord::empty(record.snapshot.scope.clone());
        for revision in 1..=record.snapshot.revision {
            let operation = record
                .operations
                .iter()
                .find(|operation| operation.revision == revision);
            let changed = if let Some(operation) = operation {
                replay.change(revision - 1, operation.change.clone())?
            } else {
                let evidence = record
                    .snapshot
                    .evidence
                    .iter()
                    .find(|value| value.revision == revision)
                    .ok_or(MemoryError::CorruptState)?;
                if !matches!(evidence.source, EvidenceSource::CompletedInteraction { .. }) {
                    return Err(MemoryError::CorruptState);
                }
                replay.import(revision - 1, evidence.clone())?
            };
            if !changed {
                return Err(MemoryError::CorruptState);
            }
        }
        if replay != *record {
            return Err(MemoryError::CorruptState);
        }
    }
    Ok(())
}
