use eve_learning_api::*;
use eve_memory_api::{MemoryScope, MemorySnapshot};
fn scope() -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: "s".into(),
        user_id: "u".into(),
    }
}
struct Legacy {
    wrong: bool,
}
impl LearningAdmin for Legacy {
    fn snapshot(&self, s: &MemoryScope) -> LearningResult<LearningSnapshot> {
        let mut actual = s.clone();
        if self.wrong {
            actual.user_id = "other".into();
        }
        Ok(LearningSnapshot {
            scope: actual,
            jobs: vec![],
        })
    }
    fn reserve(
        &self,
        _: &MemorySnapshot,
        _: u64,
        _: &str,
        _: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        Err(LearningError::Unavailable)
    }
    fn finish(&self, _: &LearningBatch, _: u64, _: LearningOutcome) -> LearningResult<()> {
        Err(LearningError::Unavailable)
    }
}
#[test]
fn snapshot_pages_preserve_order_and_explicit_end() {
    let expected: Vec<_> = (0..MAX_LEARNING_PAGE * 2 + 3).collect();
    let mut cursor = SnapshotHistoryCursor::new(scope(), expected.clone()).unwrap();
    assert_eq!(cursor.scope(), &scope());
    let mut actual = vec![];
    loop {
        let page = cursor.next_page().unwrap();
        assert!(page.len() <= MAX_LEARNING_PAGE);
        if page.is_empty() {
            break;
        }
        actual.extend(page);
    }
    assert_eq!(actual, expected);
    assert!(cursor.next_page().unwrap().is_empty());
}
#[test]
fn default_adapter_preserves_legacy_scope_checks_and_missing_capabilities() {
    assert!(Legacy { wrong: true }.scan_jobs(&scope()).is_err());
    let mut empty = Legacy { wrong: false }.scan_jobs(&scope()).unwrap();
    assert!(empty.next_page().unwrap().is_empty());
    assert!(Legacy { wrong: false }.scan_decisions(&scope()).is_err());
    assert!(!Legacy { wrong: false }.archived_history().unwrap());
}
