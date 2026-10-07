//! 只投影公开记忆快照；正文须显式开启，并始终限制预览大小。
use crate::AppError;
use eve_memory_api::{
    EvidenceSource, InteractionEvidence, MemoryError, MemoryScope, MemorySnapshot, Preference,
    PreferenceStatus, PreferenceVersion, validate_id,
};
use serde_json::{Value, json};

pub(crate) const PAGE_SIZE: usize = 20;
pub(crate) const MAX_PAGE_SIZE: usize = 50;
const PREVIEW_BYTES: usize = 512;
const REFERENCE_LIMIT: usize = 50;

pub(crate) fn status(snapshot: &MemorySnapshot) -> Value {
    json!({
        "evidence_count": snapshot.evidence.len(),
        "preference_count": snapshot.preferences.len(),
        "confirmed_count": snapshot.preferences.iter()
            .filter(|value| value.status == PreferenceStatus::Confirmed).count(),
        "revoked_count": snapshot.preferences.iter()
            .filter(|value| value.status == PreferenceStatus::Revoked).count(),
        "history_count": snapshot.preferences.iter()
            .map(|value| value.history.len()).sum::<usize>(),
    })
}

pub(crate) fn list(
    snapshot: &MemorySnapshot,
    page: usize,
    page_size: usize,
    include_content: bool,
) -> Result<Value, AppError> {
    let pagination = Pagination::new(snapshot.preferences.len(), page, page_size)?;
    let mut preferences: Vec<_> = snapshot.preferences.iter().collect();
    preferences.sort_by(|left, right| left.id.cmp(&right.id));
    let rows: Vec<_> = preferences
        .into_iter()
        .skip(pagination.offset)
        .take(page_size)
        .map(|preference| preference_view(preference, include_content))
        .collect();
    let mut result = json!({ "preferences": rows });
    pagination.write(&mut result);
    Ok(result)
}

pub(crate) fn show(
    snapshot: &MemorySnapshot,
    id: &str,
    page: usize,
    page_size: usize,
    include_content: bool,
) -> Result<Value, AppError> {
    validate_id(id)?;
    let preference = snapshot
        .preferences
        .iter()
        .find(|preference| preference.id == id)
        .ok_or(MemoryError::NotFound)?;
    let pagination = Pagination::new(preference.history.len(), page, page_size)?;
    let mut history: Vec<_> = preference.history.iter().collect();
    history.sort_by_key(|version| version.revision);
    let rows: Vec<_> = history
        .into_iter()
        .skip(pagination.offset)
        .take(page_size)
        .map(|version| history_view(snapshot, preference, version, include_content))
        .collect();
    let mut result = json!({
        "preference": preference_view(preference, include_content),
        "history": rows,
        "history_order": "revision_ascending",
    });
    pagination.write(&mut result);
    Ok(result)
}

pub(crate) fn evidence(
    snapshot: &MemorySnapshot,
    id: &str,
    include_content: bool,
) -> Result<Value, AppError> {
    validate_id(id)?;
    let evidence = snapshot
        .evidence
        .iter()
        .find(|evidence| evidence.id == id)
        .ok_or(MemoryError::NotFound)?;
    let mut references: Vec<_> = snapshot
        .preferences
        .iter()
        .flat_map(|preference| {
            preference
                .history
                .iter()
                .filter(move |version| version.evidence_id == id)
                .map(move |version| (preference, version))
        })
        .collect();
    references.sort_by(|(left_preference, left), (right_preference, right)| {
        left_preference
            .id
            .cmp(&right_preference.id)
            .then(left.revision.cmp(&right.revision))
    });
    let total = references.len();
    let items: Vec<_> = references
        .into_iter()
        .take(REFERENCE_LIMIT)
        .map(|(preference, version)| {
            json!({
                "preference_id": preference.id,
                "revision": version.revision,
                "status": version.status,
                "at_ms": version.at_ms,
                "is_current": version.revision == preference.revision,
                "current_effective": version_is_effective(preference, version),
            })
        })
        .collect();
    Ok(json!({
        "evidence": evidence_view(evidence, include_content),
        "references": {
            "total": total,
            "limit": REFERENCE_LIMIT,
            "truncated": total > REFERENCE_LIMIT,
            "items": items,
        },
    }))
}

pub(crate) fn scopes(
    mut scopes: Vec<MemoryScope>,
    page: usize,
    page_size: usize,
) -> Result<Value, AppError> {
    let pagination = Pagination::new(scopes.len(), page, page_size)?;
    scopes.sort();
    let rows: Vec<_> = scopes
        .into_iter()
        .skip(pagination.offset)
        .take(page_size)
        .map(|scope| {
            json!({
                "channel": scope.channel,
                "session_id": scope.session_id,
                "user_id": scope.user_id,
            })
        })
        .collect();
    let mut result = json!({ "scopes": rows });
    pagination.write(&mut result);
    Ok(result)
}

fn preference_view(preference: &Preference, include_content: bool) -> Value {
    let current = preference
        .history
        .iter()
        .find(|version| version.revision == preference.revision);
    let mut value = json!({
        "id": preference.id,
        "status": preference.status,
        "revision": preference.revision,
        "history_count": preference.history.len(),
        "current_evidence_id": current.map(|version| &version.evidence_id),
        "current_effective": preference.status == PreferenceStatus::Confirmed,
    });
    add_text(&mut value, &preference.text, include_content);
    value
}

fn history_view(
    snapshot: &MemorySnapshot,
    preference: &Preference,
    version: &PreferenceVersion,
    include_content: bool,
) -> Value {
    let source = snapshot
        .evidence
        .iter()
        .find(|evidence| evidence.id == version.evidence_id);
    let mut reference = json!({
        "id": version.evidence_id,
        "source_present": source.is_some(),
    });
    if let Some(source) = source {
        reference["revision"] = json!(source.revision);
        reference["at_ms"] = json!(source.at_ms);
        // 历史只提供证据引用；正文必须通过 evidence 命令显式读取。
        reference["source"] = source_metadata(&source.source);
    }
    let mut value = json!({
        "revision": version.revision,
        "status": version.status,
        "at_ms": version.at_ms,
        "evidence_id": version.evidence_id,
        "evidence": reference,
        "is_current": version.revision == preference.revision,
        "current_effective": version_is_effective(preference, version),
    });
    add_text(&mut value, &version.text, include_content);
    value
}

fn version_is_effective(preference: &Preference, version: &PreferenceVersion) -> bool {
    version.revision == preference.revision
        && preference.status == PreferenceStatus::Confirmed
        && version.status == PreferenceStatus::Confirmed
}

fn evidence_view(evidence: &InteractionEvidence, include_content: bool) -> Value {
    let mut source = source_metadata(&evidence.source);
    match &evidence.source {
        EvidenceSource::UserStatement { text, .. } => {
            add_text(&mut source, text, include_content);
        }
        EvidenceSource::CompletedInteraction {
            user_text,
            assistant_text,
            ..
        } => {
            // 完成交互的用户输入与助手回复分别列出，不把助手文本当成用户事实。
            let mut user = json!({});
            let mut assistant = json!({});
            add_text(&mut user, user_text, include_content);
            add_text(&mut assistant, assistant_text, include_content);
            source["user"] = user;
            source["assistant"] = assistant;
        }
    }
    json!({
        "id": evidence.id,
        "revision": evidence.revision,
        "at_ms": evidence.at_ms,
        "source": source,
    })
}

fn source_metadata(source: &EvidenceSource) -> Value {
    match source {
        EvidenceSource::UserStatement { message_id, .. } => json!({
            "kind": "UserStatement",
            "message_id": message_id,
        }),
        EvidenceSource::CompletedInteraction {
            message_id,
            session_revision,
            turn_id,
            ..
        } => json!({
            "kind": "CompletedInteraction",
            "message_id": message_id,
            "session_revision": session_revision,
            "turn_id": turn_id,
        }),
    }
}

fn add_text(value: &mut Value, text: &str, include_content: bool) {
    value["text_bytes"] = json!(text.len());
    if include_content {
        let mut end = text.len().min(PREVIEW_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        value["content"] = json!({
            "text": &text[..end],
            "original_bytes": text.len(),
            "truncated": end < text.len(),
        });
    }
}

struct Pagination {
    total: usize,
    page: usize,
    page_size: usize,
    pages: usize,
    offset: usize,
}

impl Pagination {
    fn new(total: usize, page: usize, page_size: usize) -> Result<Self, AppError> {
        if page == 0 || !(1..=MAX_PAGE_SIZE).contains(&page_size) {
            return Err(MemoryError::InvalidInput.into());
        }
        let pages = total.div_ceil(page_size);
        if page > pages.max(1) {
            return Err(MemoryError::InvalidInput.into());
        }
        let offset = (page - 1)
            .checked_mul(page_size)
            .ok_or(MemoryError::InvalidInput)?;
        Ok(Self {
            total,
            page,
            page_size,
            pages,
            offset,
        })
    }

    fn write(&self, value: &mut Value) {
        value["total"] = json!(self.total);
        value["page"] = json!(self.page);
        value["page_size"] = json!(self.page_size);
        value["pages"] = json!(self.pages);
        value["next_page"] = json!((self.page < self.pages).then(|| self.page + 1));
    }
}
