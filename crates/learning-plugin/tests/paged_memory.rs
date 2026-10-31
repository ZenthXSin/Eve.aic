mod support;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_learning_api::*;
use eve_learning_plugin::{LearningController, LearningPlugin};
use eve_memory_api::*;
use eve_plugin_api::{PluginId, StateStore};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{memory, scope};

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
                    format!("请先给结论，这是第 {number} 次交互。")
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
async fn open(state: Arc<dyn StateStore>) -> (Kernel, LearningController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}
fn raw(state: &dyn StateStore) -> Option<Vec<u8>> {
    state
        .get(
            &PluginId::new(LEARNING_PLUGIN_ID).unwrap(),
            LEARNING_STATE_KEY,
        )
        .unwrap()
}

fn evidence(number: u64, text: &str) -> InteractionEvidence {
    InteractionEvidence {
        id: format!("e-{number}"),
        revision: number,
        at_ms: number,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("message-{number}"),
            turn_id: number,
            session_revision: number * 2,
            user_text: text.into(),
            assistant_text: format!("第 {number} 次已完成的回复。"),
        },
    }
}
fn options() -> LearningOptions {
    LearningOptions {
        min_new_evidence: 1,
        cooldown_ms: 0,
        ..LearningOptions::default()
    }
}

#[tokio::test]
async fn new_batch_scans_six_thousand_sources_without_snapshot_and_matches_existing_batch_ids() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state).await;
    let mut source = reader(6000);
    source.large_from = Some(9);
    let batch = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(source.read.load(Ordering::SeqCst), 6000);
    assert_eq!(batch.evidence.len(), 8);
    assert_eq!(batch.evidence[0].id, "e-1");
    assert_eq!(batch.evidence[7].id, "e-8");
    let (other, legacy) = open(Arc::new(MemoryStateStore::default())).await;
    let small = memory(source.scope.clone(), 8);
    let expected = legacy
        .reserve(&small, 100, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(batch, expected);
    other.stop_all().await.unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn later_page_failure_or_changed_consumed_original_never_commits_another_batch() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state.clone()).await;
    let mut source = reader(MAX_ARCHIVE_PAGE as u64 * 2 + 1);
    source.fail_at = Some(MAX_ARCHIVE_PAGE as u64 + 1);
    assert_eq!(
        admin
            .reserve_reader(&source.scope, &source, 100, "v1", &options())
            .err(),
        Some(LearningError::Memory(MemoryError::Storage))
    );
    assert_eq!(raw(state.as_ref()), None);
    source.fail_at = None;
    let batch = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(&batch, 101, LearningOutcome::Completed(vec![]))
        .unwrap();
    let before = raw(state.as_ref());
    source.fail_at = Some(MAX_ARCHIVE_PAGE as u64 + 1);
    assert_eq!(
        admin
            .reserve_reader(
                &source.scope,
                &source,
                102,
                "v1",
                &LearningOptions {
                    cooldown_ms: 1000,
                    ..options()
                }
            )
            .err(),
        Some(LearningError::Memory(MemoryError::Storage))
    );
    assert_eq!(raw(state.as_ref()), before);
    source.fail_at = None;
    source.tamper = Some(1);
    assert_eq!(
        admin.reserve_reader(&source.scope, &source, 2000, "v1", &options()),
        Err(LearningError::Conflict)
    );
    assert_eq!(raw(state.as_ref()), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn repair_pages_preserve_attempt_fencing_and_bounded_recovery_without_reconsumption() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state.clone()).await;
    let source = reader(1);
    let first = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(
            &first,
            110,
            LearningOutcome::Failed(LearningFailure::TransientProvider),
        )
        .unwrap();
    assert!(
        admin
            .reserve_reader(&source.scope, &source, 200, "v1", &options())
            .unwrap()
            .is_none()
    );
    assert!(
        admin
            .reserve_repair_reader(&source.scope, &source, 110 + REPAIR_COOLDOWN_MS - 1, "v1")
            .unwrap()
            .is_none()
    );
    let second = admin
        .reserve_repair_reader(&source.scope, &source, 110 + REPAIR_COOLDOWN_MS, "v1")
        .unwrap()
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.evidence, first.evidence);
    assert_eq!(second.attempt, 1);
    assert_eq!(
        admin.finish(
            &first,
            second.started_at_ms + 1,
            LearningOutcome::Completed(vec![])
        ),
        Err(LearningError::Conflict)
    );
    admin
        .finish(
            &second,
            second.started_at_ms + 1,
            LearningOutcome::Failed(LearningFailure::Timeout),
        )
        .unwrap();
    let third = admin
        .reserve_repair_reader(
            &source.scope,
            &source,
            second.started_at_ms + 1 + REPAIR_COOLDOWN_MS,
            "v1",
        )
        .unwrap()
        .unwrap();
    assert_eq!(third.attempt, 2);
    assert_eq!(third.id, first.id);
    admin
        .finish(
            &third,
            third.started_at_ms + 1,
            LearningOutcome::Failed(LearningFailure::TransientProvider),
        )
        .unwrap();
    assert!(
        admin
            .reserve_repair_reader(&source.scope, &source, 10 * REPAIR_COOLDOWN_MS, "v1")
            .unwrap()
            .is_none()
    );
    assert_eq!(admin.snapshot(&source.scope).unwrap().jobs.len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn repair_checks_later_pages_and_originals_before_updating_attempt_or_cooldown() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state.clone()).await;
    let mut source = reader(MAX_ARCHIVE_PAGE as u64 * 2 + 1);
    let first = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(
            &first,
            110,
            LearningOutcome::Failed(LearningFailure::TransientProvider),
        )
        .unwrap();
    let before = raw(state.as_ref());
    source.fail_at = Some(MAX_ARCHIVE_PAGE as u64 + 1);
    let at = 110 + REPAIR_COOLDOWN_MS;
    assert_eq!(
        admin
            .reserve_repair_reader(&source.scope, &source, at, "v1")
            .err(),
        Some(LearningError::Memory(MemoryError::Storage))
    );
    assert_eq!(raw(state.as_ref()), before);
    source.fail_at = None;
    source.tamper = Some(1);
    assert_eq!(
        admin.reserve_repair_reader(&source.scope, &source, at, "v1"),
        Err(LearningError::Conflict)
    );
    assert_eq!(raw(state.as_ref()), before);
    source.tamper = None;
    let repaired = admin
        .reserve_repair_reader(&source.scope, &source, at, "v1")
        .unwrap()
        .unwrap();
    assert_eq!(repaired.attempt, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn single_aged_source_still_triggers_maintenance_and_foreign_scope_is_rejected() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(state.clone()).await;
    let source = reader(1);
    let strict = LearningOptions {
        min_new_evidence: 3,
        ..options()
    };
    assert!(
        admin
            .reserve_reader(&source.scope, &source, 100, "v1", &strict)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        admin
            .reserve_reader(
                &scope("other", "u"),
                &source,
                MAINTENANCE_DELAY_MS + 1,
                "v1",
                &strict
            )
            .err(),
        Some(LearningError::Memory(MemoryError::CorruptState))
    );
    assert_eq!(raw(state.as_ref()), None);
    let batch = admin
        .reserve_reader(
            &source.scope,
            &source,
            MAINTENANCE_DELAY_MS + 1,
            "v1",
            &strict,
        )
        .unwrap()
        .unwrap();
    assert_eq!(batch.evidence.len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn permanent_provider_cancelled_and_invalid_output_are_not_repaired() {
    for failure in [
        LearningFailure::Provider,
        LearningFailure::Cancelled,
        LearningFailure::InvalidOutput,
    ] {
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, admin) = open(state.clone()).await;
        let source = reader(1);
        let batch = admin
            .reserve_reader(&source.scope, &source, 100, "v1", &options())
            .unwrap()
            .unwrap();
        admin
            .finish(&batch, 110, LearningOutcome::Failed(failure))
            .unwrap();
        let before = raw(state.as_ref());
        assert!(
            admin
                .reserve_repair_reader(&source.scope, &source, 2 * REPAIR_COOLDOWN_MS, "v1")
                .unwrap()
                .is_none()
        );
        assert_eq!(raw(state.as_ref()), before);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn reader_admission_commit_errors_close_old_handles_and_reopen_the_actual_result() {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = support::RecordingStore::open(root.path());
        let (kernel, admin) = open(store.clone()).await;
        store.fail_before(!after);
        store.fail_after(after);
        let source = reader(1);
        let writes = store.writes();
        assert_eq!(
            admin.reserve_reader(&source.scope, &source, 100, "v1", &options()),
            Err(LearningError::Storage)
        );
        assert_eq!(store.writes(), writes + 1);
        assert_eq!(
            admin.snapshot(&source.scope).err(),
            Some(LearningError::Unavailable)
        );
        kernel.stop_all().await.unwrap();
        store.fail_before(false);
        store.fail_after(false);
        let (kernel, reopened) = open(store).await;
        let jobs = reopened.snapshot(&source.scope).unwrap().jobs;
        assert_eq!(jobs.len(), usize::from(after));
        if after {
            assert_eq!(jobs[0].status, JobStatus::Interrupted);
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn reader_admission_and_repair_keep_cold_ledger_consumption_across_restart() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let kernel = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap().with_archive(Arc::new(
        eve_learning_plugin::FileLearningArchive::open(root.path()).unwrap(),
    ));
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let source = reader(2);
    let first = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(&first, 101, LearningOutcome::Completed(vec![]))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&raw(state.as_ref()).unwrap()).unwrap()["format_version"],
        2
    );
    kernel.stop_all().await.unwrap();
    kernel.start_all().await.unwrap();
    assert!(
        admin
            .reserve_reader(&source.scope, &source, 200, "v1", &options())
            .unwrap()
            .is_none()
    );
    let newer = reader(3);
    let next = admin
        .reserve_reader(&newer.scope, &newer, 200, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(next.evidence.len(), 1);
    assert_eq!(next.evidence[0].id, "e-3");
    admin
        .finish(
            &next,
            201,
            LearningOutcome::Failed(LearningFailure::TransientProvider),
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    kernel.start_all().await.unwrap();
    let retry = admin
        .reserve_repair_reader(&newer.scope, &newer, 201 + REPAIR_COOLDOWN_MS, "v1")
        .unwrap()
        .unwrap();
    assert_eq!(retry.id, next.id);
    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.evidence, next.evidence);
    assert_eq!(admin.snapshot(&newer.scope).unwrap().jobs.len(), 2);
    kernel.stop_all().await.unwrap();
}

async fn open_cold(
    state: Arc<dyn StateStore>,
    path: &std::path::Path,
) -> (Kernel, LearningController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let archive = Arc::new(eve_learning_plugin::FileLearningArchive::open(path).unwrap());
    let plugin = LearningPlugin::new().unwrap().with_archive(archive);
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}

#[tokio::test]
async fn cold_admission_scans_six_thousand_sources_and_matches_legacy_batches_after_consumption() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open_cold(state, root.path()).await;
    let source = reader(6000);
    let first = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(&first, 110, LearningOutcome::Completed(vec![]))
        .unwrap();
    let second = admin
        .reserve_reader(&source.scope, &source, 200, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(source.read.load(Ordering::SeqCst), 12000);
    let (legacy_kernel, legacy) = open(Arc::new(MemoryStateStore::default())).await;
    let small = memory(source.scope.clone(), 8);
    let first_legacy = legacy
        .reserve(&small, 100, "v1", &options())
        .unwrap()
        .unwrap();
    legacy
        .finish(&first_legacy, 110, LearningOutcome::Completed(vec![]))
        .unwrap();
    let larger = memory(source.scope.clone(), 16);
    let second_legacy = legacy
        .reserve(&larger, 200, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(first, first_legacy);
    assert_eq!(second, second_legacy);
    assert_eq!(second.evidence.len(), 8);
    assert_eq!(second.evidence[0].id, "e-9");
    assert_eq!(second.evidence[7].id, "e-16");
    kernel.stop_all().await.unwrap();
    legacy_kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_admission_keeps_all_late_source_checks_during_cooldown_and_repair_checks() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open_cold(state.clone(), root.path()).await;
    let initial = reader(8);
    let first = admin
        .reserve_reader(&initial.scope, &initial, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(
            &first,
            110,
            LearningOutcome::Failed(LearningFailure::Timeout),
        )
        .unwrap();
    let before = raw(state.as_ref());
    let mut bad = reader(500);
    bad.fail_at = Some(128);
    let mut cooling = options();
    cooling.cooldown_ms = 5_000;
    assert!(matches!(
        admin.reserve_reader(&bad.scope, &bad, 200, "v1", &cooling),
        Err(LearningError::Memory(MemoryError::Storage))
    ));
    assert!(matches!(
        admin.reserve_repair_reader(&bad.scope, &bad, 110 + REPAIR_COOLDOWN_MS, "v1"),
        Err(LearningError::Memory(MemoryError::Storage))
    ));
    bad.fail_at = None;
    bad.tamper = Some(1);
    assert_eq!(
        admin.reserve_reader(&bad.scope, &bad, 200, "v1", &cooling),
        Err(LearningError::Conflict)
    );
    assert_eq!(
        admin.reserve_repair_reader(&bad.scope, &bad, 110 + REPAIR_COOLDOWN_MS, "v1"),
        Err(LearningError::Conflict)
    );
    assert_eq!(raw(state.as_ref()), before);
    assert_eq!(
        admin.snapshot(&initial.scope).unwrap().jobs[0].status,
        JobStatus::Failed(LearningFailure::Timeout)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_repair_keeps_two_attempt_limit_original_quotes_version_and_old_attempt_rejection() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open_cold(state.clone(), root.path()).await;
    let source = reader(8);
    let first = admin
        .reserve_reader(&source.scope, &source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(
            &first,
            110,
            LearningOutcome::Failed(LearningFailure::Timeout),
        )
        .unwrap();
    assert!(
        admin
            .reserve_repair_reader(&source.scope, &source, 110 + REPAIR_COOLDOWN_MS, "v2")
            .unwrap()
            .is_none()
    );
    assert!(
        admin
            .reserve_repair_reader(&source.scope, &source, 109 + REPAIR_COOLDOWN_MS, "v1")
            .unwrap()
            .is_none()
    );
    let mut previous = first.clone();
    let mut at = 110;
    for attempt in 1..=2 {
        let next = admin
            .reserve_repair_reader(&source.scope, &source, at + REPAIR_COOLDOWN_MS, "v1")
            .unwrap()
            .unwrap();
        assert_eq!(next.id, first.id);
        assert_eq!(next.evidence, first.evidence);
        assert_eq!(next.attempt, attempt);
        assert_eq!(
            admin.finish(
                &previous,
                next.started_at_ms + 1,
                LearningOutcome::Completed(vec![])
            ),
            Err(LearningError::Conflict)
        );
        assert!(
            admin
                .reserve_repair_reader(
                    &source.scope,
                    &source,
                    next.started_at_ms + REPAIR_COOLDOWN_MS,
                    "v1"
                )
                .unwrap()
                .is_none()
        );
        at = next.started_at_ms + 1;
        admin
            .finish(&next, at, LearningOutcome::Failed(LearningFailure::Timeout))
            .unwrap();
        previous = next;
    }
    let before = raw(state.as_ref());
    assert!(
        admin
            .reserve_repair_reader(&source.scope, &source, at + REPAIR_COOLDOWN_MS, "v1")
            .unwrap()
            .is_none()
    );
    assert_eq!(raw(state.as_ref()), before);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open_cold(state, root.path()).await;
    assert!(
        admin
            .reserve_repair_reader(&source.scope, &source, at + 2 * REPAIR_COOLDOWN_MS, "v1")
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}
