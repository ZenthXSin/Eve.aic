//! 可召回条目：与词项召回相同，只有现行已确认偏好与已导入的完成交互（用户、助手两个字段）。
use eve_memory_api::{
    EvidenceSource, MemoryError, MemoryRecallSource, MemoryResult, MemoryScope, MemorySnapshot,
    PreferenceStatus, RecallField,
};
use ring::digest::{Context, SHA256};
use std::collections::BTreeMap;

pub(crate) struct Item {
    /// 范围内稳定的条目键；偏好更正后键不变、正文摘要改变。
    pub key: String,
    pub text: String,
    pub source: MemoryRecallSource,
    pub evidence_revision: u64,
}

pub(crate) fn items(snapshot: &MemorySnapshot) -> MemoryResult<Vec<Item>> {
    let evidence: BTreeMap<_, _> = snapshot
        .evidence
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let mut items = Vec::new();
    for preference in &snapshot.preferences {
        if preference.status != PreferenceStatus::Confirmed {
            continue;
        }
        let latest = preference.history.last().ok_or(MemoryError::CorruptState)?;
        let source = evidence
            .get(latest.evidence_id.as_str())
            .ok_or(MemoryError::CorruptState)?;
        items.push(Item {
            key: format!("pref:{}", preference.id),
            text: preference.text.clone(),
            source: MemoryRecallSource::ConfirmedPreference {
                preference_id: preference.id.clone(),
                preference_revision: preference.revision,
                evidence_id: source.id.clone(),
                evidence_revision: source.revision,
                at_ms: latest.at_ms,
            },
            evidence_revision: source.revision,
        });
    }
    for entry in &snapshot.evidence {
        let EvidenceSource::CompletedInteraction {
            message_id,
            session_revision,
            turn_id,
            user_text,
            assistant_text,
        } = &entry.source
        else {
            continue;
        };
        for (field, text) in [
            (RecallField::User, user_text),
            (RecallField::Assistant, assistant_text),
        ] {
            items.push(Item {
                key: source_key_parts(&entry.id, Some(field)),
                text: text.clone(),
                source: MemoryRecallSource::CompletedInteraction {
                    evidence_id: entry.id.clone(),
                    evidence_revision: entry.revision,
                    message_id: message_id.clone(),
                    session_revision: *session_revision,
                    turn_id: *turn_id,
                    at_ms: entry.at_ms,
                    field,
                },
                evidence_revision: entry.revision,
            });
        }
    }
    Ok(items)
}

fn source_key_parts(evidence_id: &str, field: Option<RecallField>) -> String {
    match field {
        Some(RecallField::User) => format!("int:{evidence_id}:user"),
        Some(RecallField::Assistant) => format!("int:{evidence_id}:assistant"),
        None => format!("int:{evidence_id}"),
    }
}

/// 命中来源对应的条目键，与 `items` 一致。
pub(crate) fn source_key(source: &MemoryRecallSource) -> String {
    match source {
        MemoryRecallSource::ConfirmedPreference { preference_id, .. } => {
            format!("pref:{preference_id}")
        }
        MemoryRecallSource::CompletedInteraction {
            evidence_id, field, ..
        } => source_key_parts(evidence_id, Some(*field)),
    }
}

fn digest(parts: &[&str]) -> String {
    let mut context = Context::new(&SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 范围的摘要标识；索引不保存会话或用户 ID 原文。
pub(crate) fn scope_key(scope: &MemoryScope) -> String {
    digest(&[
        "semantic.scope:v1",
        &scope.channel,
        &scope.session_id,
        &scope.user_id,
    ])
}

pub(crate) fn content_hash(text: &str) -> String {
    digest(&["semantic.content:v1", text])
}

/// 不超过 `limit` 字节的完整字符前缀。
pub(crate) fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 送去建索引的正文：至多 2048 字节的完整字符前缀；空或含 NUL 的条目不建索引。
pub(crate) fn embed_input(item: &Item) -> Option<String> {
    let input = prefix(&item.text, eve_semantic_api::MAX_EMBED_INPUT_BYTES)
        .trim()
        .to_string();
    (!input.is_empty() && !input.contains('\0')).then_some(input)
}

/// 一段已归档证据：修订区间与其中的可召回条目。
pub(crate) struct Block {
    pub first_revision: u64,
    pub last_revision: u64,
    pub items: Vec<Item>,
}

/// 一个作用域的可召回条目在索引里的布局。
pub(crate) struct Layout {
    /// 热层身份与修订；不保留另一份完整热正文。
    pub scope: MemoryScope,
    pub revision: u64,
    /// 进 `index.v1` 的条目：现行偏好、热层证据与尚未凑满一段的归档证据。
    pub ledger: Vec<Item>,
    /// 凑满一段的归档证据，各自写成不可变的向量分段。
    pub blocks: Vec<Block>,
}

/// 读出作用域的完整可召回条目并分成热层与归档段；`with_segments` 为假时只看热层。
/// 热层与归档分两次读取，中途发生归档时条数对不上，重读至多三次。
pub(crate) fn layout(
    reader: &dyn eve_memory_api::MemoryService,
    with_segments: bool,
) -> MemoryResult<Layout> {
    for _ in 0..3 {
        let snapshot = reader.snapshot()?;
        if !with_segments || snapshot.archived_evidence == 0 {
            return Ok(Layout {
                ledger: items(&snapshot)?,
                scope: snapshot.scope,
                revision: snapshot.revision,
                blocks: vec![],
            });
        }
        let mut pages = Vec::new();
        let mut after = 0;
        loop {
            let page = reader.archived(after, crate::segments::SEGMENT_EVIDENCE)?;
            let next = page.next_after;
            if !page.evidence.is_empty() {
                pages.push(page.evidence);
            }
            match next {
                Some(next) => after = next,
                None => break,
            }
        }
        let total: usize = pages.iter().map(Vec::len).sum();
        if total as u64 != snapshot.archived_evidence {
            continue;
        }
        let scope = snapshot.scope.clone();
        let revision = snapshot.revision;
        let mut ledger_snapshot = snapshot;
        let mut blocks = Vec::new();
        for page in pages {
            if page.len() == crate::segments::SEGMENT_EVIDENCE {
                let first_revision = page.first().map_or(0, |item| item.revision);
                let last_revision = page.last().map_or(0, |item| item.revision);
                let block = MemorySnapshot {
                    scope: scope.clone(),
                    revision,
                    evidence: page,
                    preferences: vec![],
                    archived_evidence: 0,
                };
                blocks.push(Block {
                    first_revision,
                    last_revision,
                    items: items(&block)?,
                });
            } else {
                ledger_snapshot.evidence.extend(page);
            }
        }
        ledger_snapshot.evidence.sort_by_key(|item| item.revision);
        return Ok(Layout {
            ledger: items(&ledger_snapshot)?,
            scope,
            revision,
            blocks,
        });
    }
    Err(MemoryError::StaleRevision)
}
