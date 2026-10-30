use eve_memory_api::*;
use eve_memory_plugin::ScopedLexicalRecall;
use std::{collections::VecDeque, sync::Arc};

fn owner() -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: "owner".into(),
        user_id: "alice".into(),
    }
}
fn evidence(n: u64) -> InteractionEvidence {
    InteractionEvidence {
        id: format!("e-{n}"),
        revision: n,
        at_ms: n,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("m-{n}"),
            session_revision: n * 2,
            turn_id: n,
            user_text: format!("needle original {n}"),
            assistant_text: "verified reply".into(),
        },
    }
}
#[derive(Clone)]
struct Cursor {
    summary: MemoryScanSummary,
    pages: VecDeque<Vec<InteractionEvidence>>,
}
impl MemoryEvidenceCursor for Cursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        Ok(self.pages.pop_front().unwrap_or_default())
    }
}
struct ScanOnly(Cursor);
impl MemoryService for ScanOnly {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("streaming recall must not load a whole snapshot")
    }
    fn complete_snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("streaming recall must not merge a whole archive")
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        Ok(Box::new(self.0.clone()))
    }
}
fn cursor() -> Cursor {
    Cursor {
        summary: MemoryScanSummary {
            scope: owner(),
            revision: 3,
            preferences: vec![],
            evidence_count: 3,
        },
        pages: vec![vec![evidence(1)], vec![evidence(2)], vec![evidence(3)]].into(),
    }
}
fn recall(cursor: Cursor) -> MemoryResult<MemoryRecallResponse> {
    ScopedLexicalRecall::new(owner(), Arc::new(ScanOnly(cursor)))
        .unwrap()
        .recall(&MemoryRecallRequest {
            query: "needle".into(),
            limit: 2,
        })
}
#[test]
fn native_scan_recalls_original_sources_without_complete_snapshot() {
    let response = recall(cursor()).unwrap();
    assert_eq!(response.hits.len(), 2);
    assert_eq!(response.hits[0].excerpt, "needle original 3");
    assert!(
        matches!(&response.hits[0].source, MemoryRecallSource::CompletedInteraction { evidence_id, evidence_revision: 3, turn_id: 3, .. } if evidence_id == "e-3")
    );
}
#[test]
fn bad_cursor_scope_counts_order_and_aliases_are_rejected() {
    let mut cases = vec![];
    let mut foreign = cursor();
    foreign.summary.scope.user_id = "bob".into();
    cases.push(foreign);
    let mut short = cursor();
    short.pages.pop_back();
    cases.push(short);
    let mut repeated = cursor();
    repeated.pages[1][0] = evidence(1);
    cases.push(repeated);
    let mut alias = cursor();
    alias.pages[2][0].id = "e-1".into();
    cases.push(alias);
    let mut oversized = cursor();
    oversized.pages[0] = vec![evidence(1); MAX_ARCHIVE_PAGE + 1];
    cases.push(oversized);
    for bad in cases {
        assert!(matches!(recall(bad), Err(MemoryError::CorruptState)));
    }
}
