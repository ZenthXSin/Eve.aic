mod support;
use eve_interest_api::*;
use eve_interest_plugin::{InterestController, InterestPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_memory_api::*;
use eve_plugin_api::{PluginId, StateStore};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{evidence, options, scope};

struct Reader {
    scope: MemoryScope,
    count: u64,
    fail_at: Option<u64>,
    tamper: Option<u64>,
    large_from: Option<u64>,
    read: Arc<AtomicUsize>,
}
struct Cursor {
    summary: MemoryScanSummary,
    next: u64,
    fail_at: Option<u64>,
    tamper: Option<u64>,
    large_from: Option<u64>,
    read: Arc<AtomicUsize>,
}
impl MemoryEvidenceCursor for Cursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        if self.fail_at.is_some_and(|n| self.next >= n) {
            return Err(MemoryError::Storage);
        }
        let end = (self.next + MAX_ARCHIVE_PAGE as u64).min(self.summary.evidence_count + 1);
        let page = (self.next..end)
            .map(|number| {
                self.read.fetch_add(1, Ordering::SeqCst);
                let text = if self.tamper == Some(number) {
                    "被篡改的来源".into()
                } else if self.large_from.is_some_and(|start| number >= start) {
                    "长原文".repeat(1500)
                } else {
                    format!("我喜欢 Mindustry 模组 {number}")
                };
                evidence(number, &text)
            })
            .collect();
        self.next = end;
        Ok(page)
    }
}
impl MemoryService for Reader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("paged observation must not snapshot")
    }
    fn complete_snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("paged observation must not load complete memory")
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        Ok(Box::new(Cursor {
            summary: MemoryScanSummary {
                scope: self.scope.clone(),
                revision: self.count,
                evidence_count: self.count,
                preferences: vec![],
            },
            next: 1,
            fail_at: self.fail_at,
            tamper: self.tamper,
            large_from: self.large_from,
            read: self.read.clone(),
        }))
    }
}
fn reader(count: u64) -> Reader {
    Reader {
        scope: scope("s", "u"),
        count,
        fail_at: None,
        tamper: None,
        large_from: None,
        read: Arc::new(AtomicUsize::new(0)),
    }
}
async fn open(state: Arc<dyn StateStore>) -> (Kernel, InterestController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = InterestPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}
fn raw(state: &dyn StateStore) -> Option<Vec<u8>> {
    state
        .get(
            &PluginId::new(INTEREST_PLUGIN_ID).unwrap(),
            INTEREST_STATE_KEY,
        )
        .unwrap()
}

#[tokio::test]
async fn scans_beyond_hot_capacity_but_keeps_only_the_selected_batch_originals() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state).await;
    let mut reader = reader(6000);
    reader.large_from = Some(9);
    let batch = admin
        .reserve_reader(&reader.scope, &reader, 100, "observer-v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(
        reader.read.load(Ordering::SeqCst),
        6000,
        "must validate pages after a full batch"
    );
    assert_eq!(batch.evidence.len(), 8);
    assert_eq!(
        batch
            .evidence
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        (1..=8).map(|n| format!("e-{n}")).collect::<Vec<_>>()
    );
    assert!(serde_json::to_vec(&batch).unwrap().len() < MAX_INPUT_BYTES);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn later_page_error_cannot_commit_partial_selection_even_during_cooldown() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state.clone()).await;
    let mut source = reader(MAX_ARCHIVE_PAGE as u64 * 2 + 5);
    source.fail_at = Some(MAX_ARCHIVE_PAGE as u64 + 1);
    assert_eq!(
        admin
            .reserve_reader(&source.scope, &source, 100, "observer-v1", &options())
            .err(),
        Some(InterestError::Memory(MemoryError::Storage))
    );
    assert_eq!(raw(state.as_ref()), None);
    source.fail_at = None;
    let batch = admin
        .reserve_reader(&source.scope, &source, 100, "observer-v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(&batch, 101, ObservationOutcome::Completed(vec![]))
        .unwrap();
    let before = raw(state.as_ref());
    source.fail_at = Some(MAX_ARCHIVE_PAGE as u64 + 1);
    let cooling = ObservationOptions {
        cooldown_ms: 1000,
        ..options()
    };
    assert_eq!(
        admin
            .reserve_reader(&source.scope, &source, 102, "observer-v1", &cooling)
            .err(),
        Some(InterestError::Memory(MemoryError::Storage))
    );
    assert_eq!(raw(state.as_ref()), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn matches_snapshot_batches_and_checks_consumed_originals_before_reuse() {
    let (left, legacy) = open(Arc::new(MemoryStateStore::default())).await;
    let state = Arc::new(MemoryStateStore::default());
    let (right, paged) = open(state.clone()).await;
    let source = reader(3);
    let snapshot = MemorySnapshot {
        scope: source.scope.clone(),
        revision: 3,
        preferences: vec![],
        archived_evidence: 0,
        evidence: (1..=3)
            .map(|n| evidence(n, &format!("我喜欢 Mindustry 模组 {n}")))
            .collect(),
    };
    let a = legacy
        .reserve(&snapshot, 100, "observer-v1", &options())
        .unwrap()
        .unwrap();
    let b = paged
        .reserve_reader(&source.scope, &source, 100, "observer-v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(a, b);
    legacy
        .finish(
            &a,
            101,
            ObservationOutcome::Failed(ObservationFailure::Provider),
        )
        .unwrap();
    paged
        .finish(
            &b,
            101,
            ObservationOutcome::Failed(ObservationFailure::Provider),
        )
        .unwrap();
    assert!(
        paged
            .reserve_reader(&source.scope, &source, 200, "observer-v1", &options())
            .unwrap()
            .is_none()
    );
    let before = raw(state.as_ref());
    let mut tampered = reader(4);
    tampered.tamper = Some(1);
    assert_eq!(
        paged.reserve_reader(&source.scope, &tampered, 300, "observer-v1", &options()),
        Err(InterestError::Conflict)
    );
    assert_eq!(raw(state.as_ref()), before);
    let next = reader(4);
    let b = paged
        .reserve_reader(&next.scope, &next, 300, "observer-v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(b.evidence.len(), 1);
    assert_eq!(b.evidence[0].id, "e-4");
    assert_eq!(paged.snapshot(&source.scope).unwrap().jobs.len(), 2);
    left.stop_all().await.unwrap();
    right.stop_all().await.unwrap();
}

#[tokio::test]
async fn foreign_scope_never_reserves() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state.clone()).await;
    let source = reader(3);
    assert_eq!(
        admin
            .reserve_reader(
                &scope("other", "u"),
                &source,
                100,
                "observer-v1",
                &options()
            )
            .err(),
        Some(InterestError::Memory(MemoryError::CorruptState))
    );
    assert_eq!(raw(state.as_ref()), None);
    kernel.stop_all().await.unwrap();
}
