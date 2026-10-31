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
    /// 仅供向量输入与召回片段使用的 UTF-8 前缀；原文仍由 Memory 保存。
    pub text: String,
    pub source: MemoryRecallSource,
    pub evidence_revision: u64,
}

// 两个消费者都只读取前缀；不能把不参与向量或片段的完整正文留在整个布局中。
fn item_text(text: &str) -> String {
    let max = eve_semantic_api::MAX_EMBED_INPUT_BYTES.max(eve_memory_api::MAX_RECALL_EXCERPT_BYTES);
    prefix(text, max).to_owned()
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
            text: item_text(&preference.text),
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
                text: item_text(text),
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
        let scope = snapshot.scope.clone();
        let revision = snapshot.revision;
        let mut ledger = items(&snapshot)?;
        let archived_count = snapshot.archived_evidence;
        // 热快照正文不参与归档扫描，先释放；每次只保留当前归档页的完整正文。
        drop(snapshot);
        let mut blocks = Vec::new();
        let mut total = 0u64;
        let mut after = 0;
        loop {
            let page = reader.archived(after, crate::segments::SEGMENT_EVIDENCE)?;
            if page.evidence.len() > crate::segments::SEGMENT_EVIDENCE {
                return Err(MemoryError::CorruptState);
            }
            let mut last = after;
            for entry in &page.evidence {
                if entry.revision <= last || entry.revision > revision {
                    return Err(MemoryError::CorruptState);
                }
                last = entry.revision;
            }
            if page
                .next_after
                .is_some_and(|next| next != last || next <= after)
            {
                return Err(MemoryError::CorruptState);
            }
            total = total
                .checked_add(page.evidence.len() as u64)
                .ok_or(MemoryError::CorruptState)?;
            if total > archived_count {
                break;
            }
            let next = page.next_after;
            if !page.evidence.is_empty() {
                let full = page.evidence.len() == crate::segments::SEGMENT_EVIDENCE;
                let first_revision = page.evidence.first().map_or(0, |entry| entry.revision);
                let last_revision = page.evidence.last().map_or(0, |entry| entry.revision);
                let block = MemorySnapshot {
                    scope: scope.clone(),
                    revision,
                    evidence: page.evidence,
                    preferences: vec![],
                    archived_evidence: 0,
                };
                let compact = items(&block)?;
                if full {
                    blocks.push(Block {
                        first_revision,
                        last_revision,
                        items: compact,
                    });
                } else {
                    ledger.extend(compact);
                }
            }
            match next {
                Some(next) => after = next,
                None => break,
            }
        }
        if total != archived_count {
            continue;
        }
        // 保持旧布局：偏好原顺序在前，其余热条目与不满一段的归档按来源修订排序。
        ledger.sort_by_key(|item| match item.source {
            MemoryRecallSource::ConfirmedPreference { .. } => (0, 0),
            MemoryRecallSource::CompletedInteraction { .. } => (1, item.evidence_revision),
        });
        return Ok(Layout {
            ledger,
            scope,
            revision,
            blocks,
        });
    }
    Err(MemoryError::StaleRevision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_memory_api::{ArchivedEvidencePage, InteractionEvidence, MemoryService};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scope() -> MemoryScope {
        MemoryScope {
            channel: "test".into(),
            session_id: "session".into(),
            user_id: "user".into(),
        }
    }
    fn evidence(revision: u64, text: &str) -> InteractionEvidence {
        InteractionEvidence {
            id: format!("e-{revision}"),
            revision,
            at_ms: revision,
            source: EvidenceSource::CompletedInteraction {
                message_id: format!("m-{revision}"),
                session_revision: revision * 2,
                turn_id: revision,
                user_text: text.into(),
                assistant_text: text.into(),
            },
        }
    }
    fn snapshot(evidence: Vec<InteractionEvidence>, archived_evidence: u64) -> MemorySnapshot {
        MemorySnapshot {
            scope: scope(),
            revision: 200,
            evidence,
            preferences: vec![],
            archived_evidence,
        }
    }
    #[test]
    fn compact_items_keep_exact_embedding_hash_and_excerpt_for_long_unicode_and_controls() {
        for text in [
            "简短正文".to_owned(),
            "汉字与换行\n".repeat(4000),
            format!("{}中后文", "a".repeat(2047)),
            format!("{}\0later", "a".repeat(4096)),
            " \n".repeat(2000),
        ] {
            let source = snapshot(vec![evidence(1, &text)], 0);
            for item in items(&source).unwrap() {
                assert!(item.text.len() <= eve_semantic_api::MAX_EMBED_INPUT_BYTES);
                let original = prefix(&text, eve_semantic_api::MAX_EMBED_INPUT_BYTES).trim();
                let expected =
                    (!original.is_empty() && !original.contains('\0')).then(|| original.to_owned());
                assert_eq!(embed_input(&item), expected);
                assert_eq!(
                    content_hash(
                        prefix(&item.text, eve_semantic_api::MAX_EMBED_INPUT_BYTES).trim()
                    ),
                    content_hash(original)
                );
                assert_eq!(
                    prefix(
                        &item.text,
                        eve_memory_api::MAX_RECALL_EXCERPT_BYTES - "…".len()
                    ),
                    prefix(&text, eve_memory_api::MAX_RECALL_EXCERPT_BYTES - "…".len())
                );
                assert_eq!(
                    item.text.len() > eve_memory_api::MAX_RECALL_EXCERPT_BYTES,
                    text.len() > eve_memory_api::MAX_RECALL_EXCERPT_BYTES
                );
            }
        }
    }

    struct Paged {
        mode: usize,
        snapshots: AtomicUsize,
        pages: AtomicUsize,
    }
    impl MemoryService for Paged {
        fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
            self.snapshots.fetch_add(1, Ordering::SeqCst);
            Ok(snapshot(
                vec![evidence(170, "热层原文")],
                if self.mode == 3 { 130 } else { 129 },
            ))
        }
        fn archived(&self, after: u64, limit: usize) -> MemoryResult<ArchivedEvidencePage> {
            self.pages.fetch_add(1, Ordering::SeqCst);
            assert_eq!(limit, crate::segments::SEGMENT_EVIDENCE);
            if self.mode == 1 && after >= 64 {
                return Err(MemoryError::CorruptState);
            }
            let end = (after + limit as u64).min(129);
            Ok(ArchivedEvidencePage {
                evidence: (after + 1..=end)
                    .map(|revision| evidence(revision, &"旧原文中文".repeat(2000)))
                    .collect(),
                next_after: if self.mode == 2 {
                    Some(after)
                } else {
                    (end < 129).then_some(end)
                },
            })
        }
    }
    fn reader(mode: usize) -> Paged {
        Paged {
            mode,
            snapshots: AtomicUsize::new(0),
            pages: AtomicUsize::new(0),
        }
    }
    #[test]
    fn multiple_pages_keep_segment_keys_order_and_only_required_text_prefixes() {
        let reader = reader(0);
        let result = layout(&reader, true).unwrap();
        assert_eq!(reader.pages.load(Ordering::SeqCst), 3);
        assert_eq!(result.blocks.len(), 2);
        assert_eq!(
            (
                result.blocks[0].first_revision,
                result.blocks[0].last_revision
            ),
            (1, 64)
        );
        assert_eq!(
            (
                result.blocks[1].first_revision,
                result.blocks[1].last_revision
            ),
            (65, 128)
        );
        assert_eq!(
            result
                .ledger
                .iter()
                .map(|item| item.evidence_revision)
                .collect::<Vec<_>>(),
            [129, 129, 170, 170]
        );
        for block in &result.blocks {
            assert_eq!(block.items.len(), 128);
            let pairs: Vec<_> = block
                .items
                .iter()
                .map(|item| (item.key.clone(), content_hash(&embed_input(item).unwrap())))
                .collect();
            let original = "旧原文中文".repeat(2000);
            let hash =
                content_hash(prefix(&original, eve_semantic_api::MAX_EMBED_INPUT_BYTES).trim());
            let expected: Vec<_> = (block.first_revision..=block.last_revision)
                .flat_map(|revision| {
                    ["user", "assistant"]
                        .map(|field| (format!("int:e-{revision}:{field}"), hash.clone()))
                })
                .collect();
            assert_eq!(
                crate::segments::content_key(&pairs),
                crate::segments::content_key(&expected)
            );
            assert!(
                block
                    .items
                    .iter()
                    .all(|item| item.text.len() <= eve_semantic_api::MAX_EMBED_INPUT_BYTES)
            );
        }
    }
    #[test]
    fn late_page_errors_bad_cursors_and_repeated_count_mismatch_never_return_partial_layout() {
        for mode in 1..=3 {
            let reader = reader(mode);
            assert!(matches!(
                layout(&reader, true),
                Err(MemoryError::CorruptState | MemoryError::StaleRevision)
            ));
            assert!(reader.pages.load(Ordering::SeqCst) <= 9);
            assert!(reader.snapshots.load(Ordering::SeqCst) <= 3);
        }
    }
}
