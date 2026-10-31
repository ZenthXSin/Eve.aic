mod support;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_learning_api::*;
use eve_learning_plugin::{FileLearningArchive, LearningController, LearningPlugin};
use eve_memory_api::{EvidenceSource, MemorySnapshot};
use eve_plugin_api::{PluginId, StateStore};
use serde_json::{Value, json};
use std::sync::Arc;
use support::{RecordingStore, memory, scope};

const DECISIONS: &str = "learning.decisions.v1";
fn owner() -> PluginId {
    PluginId::new(LEARNING_PLUGIN_ID).unwrap()
}
fn options() -> LearningOptions {
    LearningOptions {
        min_new_evidence: 1,
        cooldown_ms: 0,
        ..LearningOptions::default()
    }
}
async fn open(
    state: Arc<dyn StateStore>,
    archive: Option<Arc<dyn LearningArchiveStore>>,
) -> (Kernel, LearningController) {
    let k = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let mut p = LearningPlugin::new().unwrap();
    if let Some(a) = archive {
        p = p.with_archive(a);
    }
    let a = p.controller();
    k.register(Box::new(p)).unwrap();
    k.start_all().await.unwrap();
    (k, a)
}
fn complete(a: &LearningController, source: &MemorySnapshot) -> LearningBatch {
    let batch = a.reserve(source, 100, "v1", &options()).unwrap().unwrap();
    a.finish(
        &batch,
        110,
        LearningOutcome::Completed(vec![CandidateDraft {
            text: "先给结论".into(),
            confidence: 80,
            evidence_ids: vec![batch.evidence[0].id.clone()],
        }]),
    )
    .unwrap();
    batch
}
fn decision(a: &LearningController, source: &MemorySnapshot) -> LearningDecision {
    let c = a.snapshot(&source.scope).unwrap().jobs[0].candidates[0].clone();
    LearningDecision {
        candidate_id: c.id,
        batch_id: c.batch_id,
        policy_version: "p1".into(),
        memory_revision: source.revision,
        evidence_ids: c.draft.evidence_ids,
        action: LearningDecisionAction::Confirm,
        reason: DecisionReason::Eligible,
    }
}
fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn legacy_migration_restart_consumption_decisions_and_corruption_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let cold = Arc::new(FileLearningArchive::open(root.path().join("cold")).unwrap());
    let source = memory(scope("old", "u"), 1);
    let (k, a) = open(state.clone(), None).await;
    complete(&a, &source);
    let d = decision(&a, &source);
    a.record_decision(&source.scope, d.clone(), 111).unwrap();
    let old = a.snapshot(&source.scope).unwrap();
    k.stop_all().await.unwrap();
    let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let (k, a) = open(state.clone(), Some(cold.clone())).await;
    assert_eq!(a.snapshot(&source.scope).unwrap(), old);
    assert_eq!(before, state.get(&owner(), LEARNING_STATE_KEY).unwrap());
    let other = memory(scope("new", "u"), 1);
    complete(&a, &other);
    let mut revoked = d.clone();
    revoked.action = LearningDecisionAction::Reject;
    revoked.reason = DecisionReason::RevokedConflict;
    assert_eq!(
        a.record_decision(&source.scope, revoked.clone(), 120)
            .unwrap()
            .sequence,
        2
    );
    for key in [LEARNING_STATE_KEY, DECISIONS] {
        let doc: Value =
            serde_json::from_slice(&state.get(&owner(), key).unwrap().unwrap()).unwrap();
        assert_eq!(doc["format_version"], 2);
        assert!(doc.get("jobs").is_none());
    }
    assert!(a.reserve(&source, 200, "v2", &options()).unwrap().is_none());
    k.stop_all().await.unwrap();
    let (k, a) = open(state.clone(), Some(cold.clone())).await;
    assert_eq!(a.snapshot(&source.scope).unwrap(), old);
    let ds = a.decisions(&source.scope).unwrap();
    assert_eq!(ds.len(), 2);
    assert!(ds[1].decision.same_outcome(&revoked));
    a.reserve(
        &memory(scope("pending-at-corruption", "u"), 1),
        200,
        "v1",
        &options(),
    )
    .unwrap()
    .unwrap();
    let index: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    let path = root.path().join("cold").join(format!(
        "{}.json",
        index["records"][0]["hash"].as_str().unwrap()
    ));
    std::fs::write(&path, b"corrupted").unwrap();
    let writes = state.writes();
    assert!(a.snapshot(&other.scope).is_err());
    assert_eq!(a.decisions(&source.scope), Err(LearningError::Unavailable));
    assert_eq!(state.writes(), writes);
    k.stop_all().await.unwrap();
    let k = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    });
    k.register(Box::new(LearningPlugin::new().unwrap().with_archive(cold)))
        .unwrap();
    assert!(k.start_all().await.is_err());
    assert_eq!(state.writes(), writes);
    assert_eq!(std::fs::read(path).unwrap(), b"corrupted");
}

#[tokio::test]
async fn ambiguous_index_commit_requires_reopen_and_never_replays() {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(root.path());
        let cold = Arc::new(FileLearningArchive::open(root.path().join("cold")).unwrap());
        let (k, a) = open(state.clone(), Some(cold.clone())).await;
        let source = memory(scope("s", "u"), 1);
        if after {
            state.fail_after(true)
        } else {
            state.fail_before(true)
        }
        assert_eq!(
            a.reserve(&source, 100, "v1", &options()),
            Err(LearningError::Storage)
        );
        assert_eq!(a.snapshot(&source.scope), Err(LearningError::Unavailable));
        k.stop_all().await.unwrap();
        state.fail_after(false);
        state.fail_before(false);
        let (k, a) = open(state.clone(), Some(cold)).await;
        let jobs = a.snapshot(&source.scope).unwrap().jobs;
        if after {
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, JobStatus::Interrupted);
            assert!(a.reserve(&source, 200, "v1", &options()).unwrap().is_none());
        } else {
            assert!(jobs.is_empty());
            assert!(a.reserve(&source, 200, "v1", &options()).unwrap().is_some());
        }
        k.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn more_than_32_mib_and_legacy_job_limit_retains_exact_sources_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let cold = Arc::new(FileLearningArchive::open(root.path()).unwrap());
    let mut refs = vec![];
    let mut total = 0;
    let mut first = None;
    // 每条种子经真实准入和完成，再组合大索引，避免逐条重写整本大账。
    for n in 0..1124 {
        let seed = Arc::new(MemoryStateStore::default());
        let (k, a) = open(seed.clone(), None).await;
        let mut source = memory(scope(&format!("s{n}"), "u"), 8);
        for e in &mut source.evidence {
            if let EvidenceSource::CompletedInteraction { user_text, .. } = &mut e.source {
                *user_text = "a".repeat(3600)
            }
        }
        let b = a.reserve(&source, 100, "v1", &options()).unwrap().unwrap();
        let drafts = (0..3)
            .map(|_| CandidateDraft {
                text: "x".repeat(900),
                confidence: 80,
                evidence_ids: vec![b.evidence[0].id.clone()],
            })
            .collect();
        a.finish(&b, 110, LearningOutcome::Completed(drafts))
            .unwrap();
        let doc: Value =
            serde_json::from_slice(&seed.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap())
                .unwrap();
        let bytes = serde_json::to_vec(&doc["jobs"][0]).unwrap();
        let key = hash(&bytes);
        cold.put(&key, &bytes).unwrap();
        refs.push(json!({"hash":key,"bytes":bytes.len()}));
        total += bytes.len();
        if n == 0 {
            first = Some((source, a.snapshot(&b.scope).unwrap()));
        }
        k.stop_all().await.unwrap();
    }
    assert!(total > MAX_STATE_BYTES);
    assert!(refs.len() > MAX_JOBS);
    state
        .set(
            &owner(),
            LEARNING_STATE_KEY.into(),
            serde_json::to_vec(&json!({"format_version":2,"kind":"jobs","records":refs})).unwrap(),
        )
        .unwrap();
    let (k, a) = open(state.clone(), Some(cold.clone())).await;
    let (source, saved) = first.unwrap();
    assert_eq!(a.snapshot(&source.scope).unwrap(), saved);
    assert!(a.reserve(&source, 200, "v2", &options()).unwrap().is_none());
    let new = memory(scope("after-capacity", "u"), 1);
    complete(&a, &new);
    k.stop_all().await.unwrap();
    let (k, a) = open(state, Some(cold)).await;
    assert_eq!(a.snapshot(&source.scope).unwrap(), saved);
    assert_eq!(a.snapshot(&new.scope).unwrap().jobs.len(), 1);
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn decision_history_crosses_old_count_limit_and_keeps_sequences_and_revocation() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let cold = Arc::new(FileLearningArchive::open(root.path()).unwrap());
    let source = memory(scope("decision-history", "u"), 1);
    let (k, a) = open(state.clone(), None).await;
    complete(&a, &source);
    let original = decision(&a, &source);
    k.stop_all().await.unwrap();
    let records: Vec<_> = (1..=MAX_DECISIONS)
        .map(|seq| {
            let mut d = original.clone();
            if seq % 2 == 0 {
                d.action = LearningDecisionAction::Reject;
                d.reason = DecisionReason::RevokedConflict;
            }
            json!({"scope":source.scope,"record":{"sequence":seq,"at_ms":120,"decision":d}})
        })
        .collect();
    state
        .set(
            &owner(),
            DECISIONS.into(),
            serde_json::to_vec(&json!({"format_version":1,"records":records})).unwrap(),
        )
        .unwrap();
    let (k, a) = open(state.clone(), Some(cold.clone())).await;
    let next = a
        .record_decision(&source.scope, original.clone(), 121)
        .unwrap();
    assert_eq!(next.sequence, MAX_DECISIONS as u64 + 1);
    assert_eq!(
        a.record_decision(&source.scope, original, 130)
            .unwrap()
            .sequence,
        next.sequence
    );
    k.stop_all().await.unwrap();
    let (k, a) = open(state.clone(), Some(cold)).await;
    let history = a.decisions(&source.scope).unwrap();
    assert_eq!(history.len(), MAX_DECISIONS + 1);
    assert_eq!(
        history[MAX_DECISIONS - 1].decision.reason,
        DecisionReason::RevokedConflict
    );
    assert_eq!(history.last().unwrap().sequence, MAX_DECISIONS as u64 + 1);
    k.stop_all().await.unwrap();
    let before = state.get(&owner(), DECISIONS).unwrap();
    let k = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    });
    k.register(Box::new(LearningPlugin::new().unwrap()))
        .unwrap();
    assert!(k.start_all().await.is_err());
    assert_eq!(before, state.get(&owner(), DECISIONS).unwrap());
}

struct FailedArchive {
    inner: FileLearningArchive,
    after: bool,
}
impl LearningArchiveStore for FailedArchive {
    fn put(&self, key: &str, bytes: &[u8]) -> LearningResult<()> {
        if self.after {
            self.inner.put(key, bytes)?;
        }
        Err(LearningError::Storage)
    }
    fn get(&self, key: &str) -> LearningResult<Vec<u8>> {
        self.inner.get(key)
    }
}
#[tokio::test]
async fn failed_cold_writes_never_commit_an_index_or_erase_legacy_data() {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let source = memory(scope("legacy", "u"), 1);
        let (k, a) = open(state.clone(), None).await;
        complete(&a, &source);
        let saved = a.snapshot(&source.scope).unwrap();
        k.stop_all().await.unwrap();
        let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let writes = state.writes();
        let (k, a) = open(
            state.clone(),
            Some(Arc::new(FailedArchive {
                inner: FileLearningArchive::open(root.path().join("cold")).unwrap(),
                after,
            })),
        )
        .await;
        assert_eq!(
            a.reserve(&memory(scope("new", "u"), 1), 200, "v1", &options()),
            Err(LearningError::Storage)
        );
        assert_eq!(before, state.get(&owner(), LEARNING_STATE_KEY).unwrap());
        assert_eq!(writes, state.writes());
        assert_eq!(a.snapshot(&source.scope).unwrap(), saved);
        k.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn paged_history_pins_versions_scopes_and_does_not_reactivate_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let cold = Arc::new(FileLearningArchive::open(root.path()).unwrap());
    let (k, a) = open(state.clone(), Some(cold.clone())).await;
    let target = scope("paged-person", "u");
    for n in 1..=20 {
        complete(&a, &memory(target.clone(), n));
    }
    complete(&a, &memory(scope("foreign", "other-user"), 1));
    let mut jobs = a.scan_jobs(&target).unwrap();
    assert_eq!(jobs.scope(), &target);
    let first = jobs.next_page().unwrap();
    assert_eq!(first.len(), MAX_LEARNING_PAGE);
    complete(&a, &memory(target.clone(), 21));
    let tail = jobs.next_page().unwrap();
    assert_eq!(tail.len(), 4);
    assert!(tail.iter().all(|j| j.batch.scope == target));
    assert!(jobs.next_page().unwrap().is_empty());
    assert_eq!(a.snapshot(&target).unwrap().jobs.len(), 21);
    let d = decision(&a, &memory(target.clone(), 21));
    let mut rejected = d.clone();
    rejected.action = LearningDecisionAction::Reject;
    rejected.reason = DecisionReason::RevokedConflict;
    a.record_decision(&target, d.clone(), 120).unwrap();
    a.record_decision(&target, rejected, 121).unwrap();
    let mut decisions = a.scan_decisions(&target).unwrap();
    a.record_decision(&target, d, 122).unwrap();
    assert_eq!(decisions.next_page().unwrap().len(), 2);
    assert!(decisions.next_page().unwrap().is_empty());
    assert_eq!(a.decisions(&target).unwrap().len(), 3);
    // 创建时的 Running 版本与后来的 Completed 版本不能模糊切换。
    let pending = memory(scope("pending-page", "u"), 1);
    let batch = a.reserve(&pending, 100, "v1", &options()).unwrap().unwrap();
    let mut old = a.scan_jobs(&pending.scope).unwrap();
    a.finish(&batch, 110, LearningOutcome::Completed(vec![]))
        .unwrap();
    assert_eq!(old.next_page().unwrap()[0].status, JobStatus::Running);
    assert_eq!(
        a.snapshot(&pending.scope).unwrap().jobs[0].status,
        JobStatus::Completed
    );
    k.stop_all().await.unwrap();
    assert!(jobs.next_page().is_err());
    assert!(decisions.next_page().is_err());
    let (k, _) = open(state, Some(cold)).await;
    assert!(old.next_page().is_err());
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_page_never_skips_corrupt_record_even_if_file_is_restored() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let cold = Arc::new(FileLearningArchive::open(root.path()).unwrap());
    let (k, a) = open(state.clone(), Some(cold.clone())).await;
    let target = scope("paged-damage", "u");
    for n in 1..=20 {
        complete(&a, &memory(target.clone(), n));
    }
    let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let index: Value = serde_json::from_slice(before.as_ref().unwrap()).unwrap();
    let path = root.path().join(format!(
        "{}.json",
        index["records"][17]["hash"].as_str().unwrap()
    ));
    let original = std::fs::read(&path).unwrap();
    let mut cursor = a.scan_jobs(&target).unwrap();
    assert_eq!(cursor.next_page().unwrap().len(), MAX_LEARNING_PAGE);
    std::fs::write(&path, b"broken").unwrap();
    assert!(cursor.next_page().is_err());
    std::fs::write(&path, original).unwrap();
    assert!(cursor.next_page().is_err());
    assert_eq!(a.snapshot(&target), Err(LearningError::Unavailable));
    assert_eq!(before, state.get(&owner(), LEARNING_STATE_KEY).unwrap());
    k.stop_all().await.unwrap();
    let (k, a) = open(state, Some(cold)).await;
    assert_eq!(a.snapshot(&target).unwrap().jobs.len(), 20);
    assert!(cursor.next_page().is_err());
    k.stop_all().await.unwrap();
}

struct TracedArchive {
    inner: FileLearningArchive,
    puts: std::sync::atomic::AtomicUsize,
    gets: std::sync::atomic::AtomicUsize,
    failure: std::sync::atomic::AtomicUsize,
}
impl LearningArchiveStore for TracedArchive {
    fn get(&self, key: &str) -> LearningResult<Vec<u8>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(key)
    }
    fn put(&self, key: &str, bytes: &[u8]) -> LearningResult<()> {
        use std::sync::atomic::Ordering;
        self.puts.fetch_add(1, Ordering::SeqCst);
        match self.failure.load(Ordering::SeqCst) {
            1 => Err(LearningError::Storage),
            2 => {
                self.inner.put(key, bytes)?;
                Err(LearningError::Storage)
            }
            _ => self.inner.put(key, bytes),
        }
    }
}
fn traced(directory: &std::path::Path) -> Arc<TracedArchive> {
    Arc::new(TracedArchive {
        inner: FileLearningArchive::open(directory).unwrap(),
        puts: 0.into(),
        gets: 0.into(),
        failure: 0.into(),
    })
}
#[tokio::test]
async fn cold_decision_append_writes_one_body_and_preserves_pinned_history_and_replay_identity() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let source = memory(scope("append", "u"), 1);
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    complete(&admin, &source);
    let first = decision(&admin, &source);
    admin
        .record_decision(&source.scope, first.clone(), 111)
        .unwrap();
    let mut pinned = admin.scan_decisions(&source.scope).unwrap();
    let writes = state.writes();
    let puts = archive.puts.load(Ordering::SeqCst);
    let mut changed = first;
    changed.policy_version = "p2".into();
    let saved = admin
        .record_decision(&source.scope, changed.clone(), 112)
        .unwrap();
    assert_eq!(saved.sequence, 2);
    assert_eq!(
        archive.puts.load(Ordering::SeqCst),
        puts + 1,
        "不能为追加一条决策重写/同步全部旧冷文件"
    );
    assert_eq!(state.writes(), writes + 1);
    changed.memory_revision += 10;
    assert_eq!(
        admin.record_decision(&source.scope, changed, 120).unwrap(),
        saved
    );
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(state.writes(), writes + 1);
    assert_eq!(pinned.next_page().unwrap().len(), 1);
    assert!(pinned.next_page().unwrap().is_empty());
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(state, Some(archive)).await;
    let records = admin.decisions(&source.scope).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1], saved);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_decision_index_commit_uncertainty_closes_existing_cursors_and_recovers_actual_bytes()
{
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let source = memory(scope("uncertain-decision", "u"), 1);
        let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
        complete(&admin, &source);
        let mut next = decision(&admin, &source);
        admin
            .record_decision(&source.scope, next.clone(), 111)
            .unwrap();
        let mut old = admin.scan_decisions(&source.scope).unwrap();
        next.policy_version = "p2".into();
        state.fail_after(after);
        state.fail_before(!after);
        assert_eq!(
            admin.record_decision(&source.scope, next.clone(), 112),
            Err(LearningError::Storage)
        );
        assert_eq!(old.next_page(), Err(LearningError::Unavailable));
        assert_eq!(
            admin.snapshot(&source.scope),
            Err(LearningError::Unavailable)
        );
        kernel.stop_all().await.unwrap();
        state.fail_after(false);
        state.fail_before(false);
        let (kernel, admin) = open(state, Some(archive)).await;
        let records = admin.decisions(&source.scope).unwrap();
        assert_eq!(records.len(), if after { 2 } else { 1 });
        if after {
            assert!(records[1].decision.same_outcome(&next));
        }
        assert_eq!(old.next_page(), Err(LearningError::Unavailable));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn unreferenced_cold_write_failure_preserves_the_old_index_and_usable_history() {
    use std::sync::atomic::Ordering;
    for mode in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let source = memory(scope("cold-put", "u"), 1);
        let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
        complete(&admin, &source);
        let mut next = decision(&admin, &source);
        admin
            .record_decision(&source.scope, next.clone(), 111)
            .unwrap();
        let original = state.get(&owner(), DECISIONS).unwrap();
        let writes = state.writes();
        next.policy_version = "p2".into();
        archive.failure.store(mode, Ordering::SeqCst);
        assert_eq!(
            admin.record_decision(&source.scope, next, 112),
            Err(LearningError::Storage)
        );
        assert_eq!(state.get(&owner(), DECISIONS).unwrap(), original);
        assert_eq!(state.writes(), writes);
        assert_eq!(admin.decisions(&source.scope).unwrap().len(), 1);
        archive.failure.store(0, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn a_late_corrupt_job_blocks_cold_decision_append_before_any_new_file_or_index_write() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let source = memory(scope("target", "u"), 1);
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    complete(&admin, &source);
    let mut next = decision(&admin, &source);
    admin
        .record_decision(&source.scope, next.clone(), 111)
        .unwrap();
    complete(&admin, &memory(scope("unrelated-late", "u"), 1));
    let jobs: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    let path = root.path().join("cold").join(format!(
        "{}.json",
        jobs["records"].as_array().unwrap().last().unwrap()["hash"]
            .as_str()
            .unwrap()
    ));
    std::fs::write(&path, b"damaged").unwrap();
    let writes = state.writes();
    let puts = archive.puts.load(Ordering::SeqCst);
    let before = state.get(&owner(), DECISIONS).unwrap();
    next.policy_version = "p2".into();
    assert!(admin.record_decision(&source.scope, next, 112).is_err());
    assert_eq!(state.writes(), writes);
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts);
    assert_eq!(state.get(&owner(), DECISIONS).unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
    assert_eq!(
        admin.decisions(&source.scope),
        Err(LearningError::Unavailable)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn change_token_tracks_durable_changes_and_new_service_identity_without_loading_history() {
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let source = memory(scope("tokens", "u"), 1);
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    let initial = admin.change_token(&source.scope).unwrap().unwrap();
    assert!(admin.snapshot(&source.scope).unwrap().jobs.is_empty());
    assert_eq!(admin.change_token(&source.scope).unwrap(), Some(initial));
    let batch = admin
        .reserve(&source, 100, "v1", &options())
        .unwrap()
        .unwrap();
    let reserved = admin.change_token(&source.scope).unwrap().unwrap();
    assert_eq!(reserved.instance, initial.instance);
    assert!(reserved.revision > initial.revision);
    assert!(
        admin
            .reserve(&source, 101, "v1", &options())
            .unwrap()
            .is_none()
    );
    assert_eq!(admin.change_token(&source.scope).unwrap(), Some(reserved));
    admin
        .finish(
            &batch,
            110,
            LearningOutcome::Completed(vec![CandidateDraft {
                text: "先给结论".into(),
                confidence: 80,
                evidence_ids: vec![batch.evidence[0].id.clone()],
            }]),
        )
        .unwrap();
    let finished = admin.change_token(&source.scope).unwrap().unwrap();
    assert!(finished.revision > reserved.revision);
    let mut decision = decision(&admin, &source);
    admin
        .record_decision(&source.scope, decision.clone(), 111)
        .unwrap();
    let first = admin.change_token(&source.scope).unwrap().unwrap();
    assert!(first.revision > finished.revision);
    admin
        .record_decision(&source.scope, decision.clone(), 112)
        .unwrap();
    assert_eq!(admin.change_token(&source.scope).unwrap(), Some(first));
    decision.policy_version = "p2".into();
    admin.record_decision(&source.scope, decision, 113).unwrap();
    let second = admin.change_token(&source.scope).unwrap().unwrap();
    assert!(second.revision > first.revision);
    let other = memory(scope("unrelated-token", "u"), 1);
    admin
        .reserve(&other, 200, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(
        admin.change_token(&source.scope).unwrap(),
        Some(second),
        "另一范围保存不能唤醒本范围重扫"
    );
    assert!(admin.change_token(&other.scope).unwrap().unwrap().revision > 0);
    let reads = archive.gets.load(std::sync::atomic::Ordering::SeqCst);
    let writes = state.writes();
    for _ in 0..100 {
        assert_eq!(admin.change_token(&source.scope).unwrap(), Some(second));
    }
    assert_eq!(
        archive.gets.load(std::sync::atomic::Ordering::SeqCst),
        reads,
        "轻量标识不应读取冷正文"
    );
    assert_eq!(state.writes(), writes);
    kernel.stop_all().await.unwrap();
    assert_eq!(
        admin.change_token(&source.scope),
        Err(LearningError::Unavailable)
    );
    let (kernel, reopened) = open(state, Some(archive)).await;
    assert_ne!(
        reopened
            .change_token(&source.scope)
            .unwrap()
            .unwrap()
            .instance,
        second.instance
    );
    kernel.stop_all().await.unwrap();
}

async fn startup_fixture(
    state: Arc<RecordingStore>,
    archive: Arc<TracedArchive>,
) -> eve_memory_api::MemoryScope {
    let (kernel, admin) = open(state, Some(archive)).await;
    let first = memory(scope("startup-complete", "u"), 1);
    complete(&admin, &first);
    admin
        .record_decision(&first.scope, decision(&admin, &first), 111)
        .unwrap();
    complete(&admin, &memory(scope("startup-other", "u"), 1));
    let pending = memory(scope("startup-pending", "u"), 1);
    admin
        .reserve(&pending, 200, "v1", &options())
        .unwrap()
        .unwrap();
    kernel.stop_all().await.unwrap();
    pending.scope
}
async fn reject_startup(state: Arc<RecordingStore>, archive: Arc<TracedArchive>) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap().with_archive(archive);
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(
        controller.snapshot(&scope("startup-pending", "u")),
        Err(LearningError::Unavailable)
    );
}

#[tokio::test]
async fn streamed_startup_checks_every_record_and_only_rewrites_interrupted_jobs() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let pending = startup_fixture(state.clone(), archive.clone()).await;
    let before: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    let decisions = state.get(&owner(), DECISIONS).unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let gets = archive.gets.load(Ordering::SeqCst);
    let writes = state.writes();
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    assert_eq!(
        archive.gets.load(Ordering::SeqCst),
        gets + 5,
        "三批次和一决策逐条核验，只重读一个在途批次"
    );
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(state.writes(), writes + 1);
    let after: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(before["records"][0], after["records"][0]);
    assert_eq!(before["records"][1], after["records"][1]);
    assert_ne!(before["records"][2], after["records"][2]);
    assert_eq!(state.get(&owner(), DECISIONS).unwrap(), decisions);
    let job = &admin.snapshot(&pending).unwrap().jobs[0];
    assert_eq!(job.status, JobStatus::Interrupted);
    assert_eq!(job.finished_at_ms, None);
    kernel.stop_all().await.unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let gets = archive.gets.load(Ordering::SeqCst);
    let writes = state.writes();
    let (kernel, _) = open(state.clone(), Some(archive.clone())).await;
    assert_eq!(archive.gets.load(Ordering::SeqCst), gets + 4);
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts);
    assert_eq!(state.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn streamed_startup_late_invalid_decision_never_rewrites_pending_jobs() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    startup_fixture(state.clone(), archive.clone()).await;
    let old_jobs = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let mut index: Value =
        serde_json::from_slice(&state.get(&owner(), DECISIONS).unwrap().unwrap()).unwrap();
    let reference = &index["records"][0];
    let mut record: Value =
        serde_json::from_slice(&archive.get(reference["hash"].as_str().unwrap()).unwrap()).unwrap();
    record["record"]["sequence"] = json!(2);
    let bytes = serde_json::to_vec(&record).unwrap();
    let key = hash(&bytes);
    archive.put(&key, &bytes).unwrap();
    index["records"][0] = json!({"hash": key, "bytes": bytes.len()});
    state
        .set(
            &owner(),
            DECISIONS.into(),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
    let writes = state.writes();
    let puts = archive.puts.load(Ordering::SeqCst);
    reject_startup(state.clone(), archive.clone()).await;
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts);
    assert_eq!(state.writes(), writes);
    assert_eq!(state.get(&owner(), LEARNING_STATE_KEY).unwrap(), old_jobs);
}

#[tokio::test]
async fn streamed_startup_cold_write_uncertainty_keeps_old_index_and_retries_recovery_only() {
    use std::sync::atomic::Ordering;
    for mode in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let pending = startup_fixture(state.clone(), archive.clone()).await;
        let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let writes = state.writes();
        archive.failure.store(mode, Ordering::SeqCst);
        reject_startup(state.clone(), archive.clone()).await;
        assert_eq!(state.writes(), writes);
        assert_eq!(state.get(&owner(), LEARNING_STATE_KEY).unwrap(), before);
        archive.failure.store(0, Ordering::SeqCst);
        let (kernel, admin) = open(state, Some(archive)).await;
        let jobs = admin.snapshot(&pending).unwrap().jobs;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, JobStatus::Interrupted);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn streamed_startup_failed_index_commit_requires_reopen_and_uses_actual_saved_index() {
    use std::sync::atomic::Ordering;
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let pending = startup_fixture(state.clone(), archive.clone()).await;
        let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
        state.fail_before(!after);
        state.fail_after(after);
        reject_startup(state.clone(), archive.clone()).await;
        assert_eq!(
            state.get(&owner(), LEARNING_STATE_KEY).unwrap() == before,
            !after
        );
        state.fail_before(false);
        state.fail_after(false);
        let puts = archive.puts.load(Ordering::SeqCst);
        let writes = state.writes();
        let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
        assert_eq!(
            archive.puts.load(Ordering::SeqCst),
            puts + usize::from(!after)
        );
        assert_eq!(state.writes(), writes + usize::from(!after));
        assert_eq!(
            admin.snapshot(&pending).unwrap().jobs[0].status,
            JobStatus::Interrupted
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn streamed_startup_rejects_cross_record_duplicates_before_recovery_writes() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    startup_fixture(state.clone(), archive.clone()).await;
    let mut index: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    let duplicate = index["records"][0].clone();
    index["records"].as_array_mut().unwrap().push(duplicate);
    let bytes = serde_json::to_vec(&index).unwrap();
    state
        .set(&owner(), LEARNING_STATE_KEY.into(), bytes.clone())
        .unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let writes = state.writes();
    reject_startup(state.clone(), archive.clone()).await;
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts);
    assert_eq!(state.writes(), writes);
    assert_eq!(
        state.get(&owner(), LEARNING_STATE_KEY).unwrap(),
        Some(bytes)
    );
}

#[tokio::test]
async fn streamed_startup_keeps_both_mixed_legacy_formats_and_pending_recovery() {
    for legacy_key in [LEARNING_STATE_KEY, DECISIONS] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let pending = startup_fixture(state.clone(), archive.clone()).await;
        let index: Value =
            serde_json::from_slice(&state.get(&owner(), legacy_key).unwrap().unwrap()).unwrap();
        let records: Vec<Value> = index["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| {
                serde_json::from_slice(&archive.get(reference["hash"].as_str().unwrap()).unwrap())
                    .unwrap()
            })
            .collect();
        let hot = if legacy_key == LEARNING_STATE_KEY {
            json!({"format_version":1,"jobs":records})
        } else {
            json!({"format_version":1,"records":records})
        };
        state
            .set(
                &owner(),
                legacy_key.into(),
                serde_json::to_vec(&hot).unwrap(),
            )
            .unwrap();
        let (kernel, admin) = open(state, Some(archive)).await;
        assert_eq!(
            admin.snapshot(&pending).unwrap().jobs[0].status,
            JobStatus::Interrupted
        );
        assert_eq!(
            admin
                .snapshot(&scope("startup-complete", "u"))
                .unwrap()
                .jobs[0]
                .status,
            JobStatus::Completed
        );
        let decisions = admin.decisions(&scope("startup-complete", "u")).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].sequence, 1);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn streamed_startup_does_not_let_cold_jobs_bypass_legacy_decision_count_limit() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    startup_fixture(state.clone(), archive.clone()).await;
    let index: Value =
        serde_json::from_slice(&state.get(&owner(), DECISIONS).unwrap().unwrap()).unwrap();
    let entry: Value = serde_json::from_slice(
        &archive
            .get(index["records"][0]["hash"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let records: Vec<_> = (1..=MAX_DECISIONS + 1)
        .map(|sequence| {
            let mut value = entry.clone();
            value["record"]["sequence"] = json!(sequence);
            value["record"]["decision"]["policy_version"] = json!(format!("p-{sequence}"));
            value
        })
        .collect();
    let bytes = serde_json::to_vec(&json!({"format_version":1,"records":records})).unwrap();
    assert!(bytes.len() < MAX_STATE_BYTES);
    state.set(&owner(), DECISIONS.into(), bytes).unwrap();
    let writes = state.writes();
    let puts = archive.puts.load(Ordering::SeqCst);
    reject_startup(state.clone(), archive.clone()).await;
    assert_eq!(state.writes(), writes);
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts);
}

async fn finish_fixture(
    state: Arc<RecordingStore>,
    archive: Arc<TracedArchive>,
) -> (Kernel, LearningController, LearningBatch) {
    let (kernel, admin) = open(state, Some(archive)).await;
    let source = memory(scope("finish-before", "u"), 1);
    complete(&admin, &source);
    admin
        .record_decision(&source.scope, decision(&admin, &source), 111)
        .unwrap();
    let batch = admin
        .reserve(
            &memory(scope("finish-target", "u"), 1),
            200,
            "v1",
            &options(),
        )
        .unwrap()
        .unwrap();
    (kernel, admin, batch)
}
fn finish_outcome(batch: &LearningBatch) -> LearningOutcome {
    LearningOutcome::Completed(vec![CandidateDraft {
        text: "先给结论".into(),
        confidence: 80,
        evidence_ids: vec![batch.evidence[0].id.clone()],
    }])
}

#[tokio::test]
async fn cold_finish_writes_one_body_keeps_pinned_running_view_and_replays_without_writing() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let (kernel, admin, batch) = finish_fixture(state.clone(), archive.clone()).await;
    let mut pinned = admin.scan_jobs(&batch.scope).unwrap();
    let before: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    let decisions = state.get(&owner(), DECISIONS).unwrap();
    let token = admin.change_token(&batch.scope).unwrap();
    let other_token = admin.change_token(&scope("finish-before", "u")).unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let writes = state.writes();
    admin.finish(&batch, 210, finish_outcome(&batch)).unwrap();
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(state.writes(), writes + 1);
    assert_ne!(admin.change_token(&batch.scope).unwrap(), token);
    assert_eq!(
        admin.change_token(&scope("finish-before", "u")).unwrap(),
        other_token
    );
    let after: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(before["records"][0], after["records"][0]);
    assert_ne!(before["records"][1], after["records"][1]);
    assert_eq!(state.get(&owner(), DECISIONS).unwrap(), decisions);
    assert_eq!(pinned.next_page().unwrap()[0].status, JobStatus::Running);
    let saved = admin.snapshot(&batch.scope).unwrap();
    assert_eq!(saved.jobs[0].status, JobStatus::Completed);
    admin.finish(&batch, 999, finish_outcome(&batch)).unwrap();
    assert_eq!(admin.snapshot(&batch.scope).unwrap(), saved);
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(state.writes(), writes + 1);
    assert_eq!(
        admin.finish(
            &batch,
            210,
            LearningOutcome::Failed(LearningFailure::Timeout)
        ),
        Err(LearningError::Conflict)
    );
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(state, Some(archive)).await;
    assert_eq!(admin.snapshot(&batch.scope).unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_finish_failed_cold_put_preserves_running_and_accepts_the_same_result_after_recovery()
{
    use std::sync::atomic::Ordering;
    for mode in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let (kernel, admin, batch) = finish_fixture(state.clone(), archive.clone()).await;
        let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let writes = state.writes();
        archive.failure.store(mode, Ordering::SeqCst);
        assert_eq!(
            admin.finish(&batch, 210, finish_outcome(&batch)),
            Err(LearningError::Storage)
        );
        assert_eq!(state.get(&owner(), LEARNING_STATE_KEY).unwrap(), before);
        assert_eq!(state.writes(), writes);
        assert_eq!(
            admin.snapshot(&batch.scope).unwrap().jobs[0].status,
            JobStatus::Running
        );
        archive.failure.store(0, Ordering::SeqCst);
        admin.finish(&batch, 210, finish_outcome(&batch)).unwrap();
        assert_eq!(
            admin.snapshot(&batch.scope).unwrap().jobs[0]
                .candidates
                .len(),
            1
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_finish_index_commit_uncertainty_closes_all_handles_without_replaying_model_results() {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let (kernel, admin, batch) = finish_fixture(state.clone(), archive.clone()).await;
        let mut pinned = admin.scan_jobs(&batch.scope).unwrap();
        state.fail_before(!after);
        state.fail_after(after);
        assert_eq!(
            admin.finish(&batch, 210, finish_outcome(&batch)),
            Err(LearningError::Storage)
        );
        assert!(matches!(
            pinned.next_page(),
            Err(LearningError::Unavailable)
        ));
        assert_eq!(
            admin.snapshot(&batch.scope),
            Err(LearningError::Unavailable)
        );
        assert_eq!(
            admin.finish(&batch, 210, finish_outcome(&batch)),
            Err(LearningError::Unavailable)
        );
        kernel.stop_all().await.unwrap();
        state.fail_before(false);
        state.fail_after(false);
        let (kernel, admin) = open(state, Some(archive)).await;
        let saved = admin.snapshot(&batch.scope).unwrap();
        assert_eq!(
            saved.jobs[0].status,
            if after {
                JobStatus::Completed
            } else {
                JobStatus::Interrupted
            }
        );
        assert_eq!(saved.jobs[0].candidates.len(), usize::from(after));
        if after {
            admin.finish(&batch, 211, finish_outcome(&batch)).unwrap();
        } else {
            assert_eq!(
                admin.finish(&batch, 211, finish_outcome(&batch)),
                Err(LearningError::Conflict)
            );
        }
        assert!(matches!(
            pinned.next_page(),
            Err(LearningError::Unavailable)
        ));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_finish_late_corrupt_decision_closes_service_before_any_write() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let (kernel, admin, batch) = finish_fixture(state.clone(), archive.clone()).await;
    let index: Value =
        serde_json::from_slice(&state.get(&owner(), DECISIONS).unwrap().unwrap()).unwrap();
    let path = root.path().join("cold").join(format!(
        "{}.json",
        index["records"][0]["hash"].as_str().unwrap()
    ));
    std::fs::write(&path, b"damaged").unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let writes = state.writes();
    let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
    assert_eq!(
        admin.finish(&batch, 210, finish_outcome(&batch)),
        Err(LearningError::CorruptState)
    );
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts);
    assert_eq!(state.writes(), writes);
    assert_eq!(state.get(&owner(), LEARNING_STATE_KEY).unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
    assert_eq!(
        admin.snapshot(&batch.scope),
        Err(LearningError::Unavailable)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_finish_preserves_reflection_validation_and_exact_idempotent_receipts() {
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let (kernel, admin, batch) = finish_fixture(state.clone(), archive).await;
    let reflection = ReflectionDraft {
        observations: "用户反复要求先给结论".into(),
        actions: "回复中先给结论".into(),
        outcomes: "未记录后续效果".into(),
        lessons: "先给结论可能适用，仍需反馈".into(),
        next_checks: "核对下一次用户反馈".into(),
        evidence_ids: vec![batch.evidence[0].id.clone()],
    };
    let LearningOutcome::Completed(candidates) = finish_outcome(&batch) else {
        unreachable!()
    };
    let outcome = || {
        LearningOutcome::Maintained(MaintenanceExtraction {
            candidates: candidates.clone(),
            reflection: Some(reflection.clone()),
        })
    };
    admin.finish(&batch, 210, outcome()).unwrap();
    let receipts = admin.maintenance_receipts(&batch.scope).unwrap();
    assert_eq!(receipts[0].reflection, Some(reflection.clone()));
    let writes = state.writes();
    admin.finish(&batch, 999, outcome()).unwrap();
    assert_eq!(
        serde_json::to_value(admin.maintenance_receipts(&batch.scope).unwrap()).unwrap(),
        serde_json::to_value(receipts).unwrap()
    );
    let invalid = ReflectionDraft {
        evidence_ids: vec!["foreign".into()],
        ..reflection
    };
    assert!(
        admin
            .finish(
                &batch,
                210,
                LearningOutcome::Maintained(MaintenanceExtraction {
                    candidates: vec![],
                    reflection: Some(invalid)
                })
            )
            .is_err()
    );
    assert_eq!(state.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_finish_rejects_stale_input_and_invalid_time_and_keeps_failure_replay_read_only() {
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let (kernel, admin, batch) = finish_fixture(state.clone(), archive).await;
    let writes = state.writes();
    let mut stale = batch.clone();
    stale.attempt = 1;
    assert_eq!(
        admin.finish(&stale, 210, finish_outcome(&batch)),
        Err(LearningError::Conflict)
    );
    assert_eq!(
        admin.finish(&batch, 199, finish_outcome(&batch)),
        Err(LearningError::InvalidInput)
    );
    assert_eq!(
        admin.finish(&batch, u64::MAX, finish_outcome(&batch)),
        Err(LearningError::InvalidInput)
    );
    assert_eq!(state.writes(), writes);
    admin
        .finish(
            &batch,
            210,
            LearningOutcome::Failed(LearningFailure::Timeout),
        )
        .unwrap();
    admin
        .finish(
            &batch,
            999,
            LearningOutcome::Failed(LearningFailure::Timeout),
        )
        .unwrap();
    assert_eq!(state.writes(), writes + 1);
    assert_eq!(
        admin.finish(
            &batch,
            210,
            LearningOutcome::Failed(LearningFailure::Cancelled)
        ),
        Err(LearningError::Conflict)
    );
    assert_eq!(
        admin.snapshot(&batch.scope).unwrap().jobs[0].finished_at_ms,
        Some(210)
    );
    kernel.stop_all().await.unwrap();
}

struct SnapshotMemory(MemorySnapshot);
impl eve_memory_api::MemoryService for SnapshotMemory {
    fn snapshot(&self) -> eve_memory_api::MemoryResult<MemorySnapshot> {
        Ok(self.0.clone())
    }
}
#[tokio::test]
async fn cold_reader_admission_and_repair_only_put_the_new_or_selected_record() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&root.path().join("state"));
    let archive = traced(&root.path().join("cold"));
    let (kernel, admin, _) = finish_fixture(state.clone(), archive.clone()).await;
    let source = SnapshotMemory(memory(scope("admit-target", "u"), 1));
    let mut pinned = admin.scan_jobs(&source.0.scope).unwrap();
    let before: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let writes = state.writes();
    let batch = admin
        .reserve_reader(&source.0.scope, &source, 300, "v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(state.writes(), writes + 1);
    let after: Value =
        serde_json::from_slice(&state.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(
        before["records"].as_array().unwrap(),
        &after["records"].as_array().unwrap()[..2]
    );
    assert!(pinned.next_page().unwrap().is_empty());
    assert_eq!(batch.evidence, source.0.evidence);
    assert!(
        admin
            .reserve_reader(&source.0.scope, &source, 301, "v2", &options())
            .unwrap()
            .is_none()
    );
    assert_eq!(state.writes(), writes + 1);
    admin
        .finish(
            &batch,
            310,
            LearningOutcome::Failed(LearningFailure::Timeout),
        )
        .unwrap();
    let puts = archive.puts.load(Ordering::SeqCst);
    let writes = state.writes();
    let repair = admin
        .reserve_repair_reader(&source.0.scope, &source, 310 + REPAIR_COOLDOWN_MS, "v1")
        .unwrap()
        .unwrap();
    assert_eq!(archive.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(state.writes(), writes + 1);
    assert_eq!(repair.evidence, batch.evidence);
    assert_eq!(repair.id, batch.id);
    assert_eq!(repair.attempt, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_reader_admission_cold_put_failure_keeps_sources_unused_and_the_index_unchanged() {
    use std::sync::atomic::Ordering;
    for mode in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let (kernel, admin, _) = finish_fixture(state.clone(), archive.clone()).await;
        let source = SnapshotMemory(memory(scope("admit-failure", "u"), 1));
        let before = state.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let writes = state.writes();
        archive.failure.store(mode, Ordering::SeqCst);
        assert_eq!(
            admin.reserve_reader(&source.0.scope, &source, 300, "v1", &options()),
            Err(LearningError::Storage)
        );
        assert_eq!(state.get(&owner(), LEARNING_STATE_KEY).unwrap(), before);
        assert_eq!(state.writes(), writes);
        assert!(admin.snapshot(&source.0.scope).unwrap().jobs.is_empty());
        archive.failure.store(0, Ordering::SeqCst);
        let batch = admin
            .reserve_reader(&source.0.scope, &source, 300, "v1", &options())
            .unwrap()
            .unwrap();
        assert_eq!(batch.evidence, source.0.evidence);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_reader_admission_index_commit_uncertainty_closes_cursors_and_preserves_actual_consumption()
 {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let (kernel, admin, existing) = finish_fixture(state.clone(), archive.clone()).await;
        let mut pinned = admin.scan_jobs(&existing.scope).unwrap();
        let source = SnapshotMemory(memory(scope("admit-uncertain", "u"), 1));
        state.fail_before(!after);
        state.fail_after(after);
        assert_eq!(
            admin.reserve_reader(&source.0.scope, &source, 300, "v1", &options()),
            Err(LearningError::Storage)
        );
        assert!(matches!(
            pinned.next_page(),
            Err(LearningError::Unavailable)
        ));
        assert_eq!(
            admin.snapshot(&source.0.scope),
            Err(LearningError::Unavailable)
        );
        kernel.stop_all().await.unwrap();
        state.fail_before(false);
        state.fail_after(false);
        let (kernel, admin) = open(state, Some(archive)).await;
        let jobs = admin.snapshot(&source.0.scope).unwrap().jobs;
        assert_eq!(jobs.len(), usize::from(after));
        if after {
            assert_eq!(jobs[0].status, JobStatus::Interrupted);
            assert!(
                admin
                    .reserve_reader(&source.0.scope, &source, 301, "v1", &options())
                    .unwrap()
                    .is_none()
            );
        } else {
            assert!(
                admin
                    .reserve_reader(&source.0.scope, &source, 301, "v1", &options())
                    .unwrap()
                    .is_some()
            );
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_repair_commit_uncertainty_preserves_exact_attempt_generation_after_restart() {
    for after in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&root.path().join("state"));
        let archive = traced(&root.path().join("cold"));
        let (kernel, admin, batch) = finish_fixture(state.clone(), archive.clone()).await;
        admin
            .finish(
                &batch,
                210,
                LearningOutcome::Failed(LearningFailure::Timeout),
            )
            .unwrap();
        let source = SnapshotMemory(memory(batch.scope.clone(), 1));
        let at = 210 + REPAIR_COOLDOWN_MS;
        let mut pinned = admin.scan_jobs(&batch.scope).unwrap();
        state.fail_before(!after);
        state.fail_after(after);
        assert_eq!(
            admin.reserve_repair_reader(&batch.scope, &source, at, "v1"),
            Err(LearningError::Storage)
        );
        assert!(matches!(
            pinned.next_page(),
            Err(LearningError::Unavailable)
        ));
        kernel.stop_all().await.unwrap();
        state.fail_before(false);
        state.fail_after(false);
        let (kernel, admin) = open(state, Some(archive)).await;
        let saved = admin.snapshot(&batch.scope).unwrap().jobs;
        assert_eq!(saved[0].batch.attempt, u8::from(after));
        assert_eq!(
            saved[0].status,
            if after {
                JobStatus::Interrupted
            } else {
                JobStatus::Failed(LearningFailure::Timeout)
            }
        );
        let receipts = admin.maintenance_receipts(&batch.scope).unwrap();
        assert_eq!(receipts[0].previous_attempts.len(), usize::from(after));
        if after {
            assert_eq!(saved[0].batch.evidence, batch.evidence);
            assert_eq!(
                admin.finish(&batch, at + 1, LearningOutcome::Completed(vec![])),
                Err(LearningError::Conflict)
            );
            assert!(
                admin
                    .reserve_repair_reader(&batch.scope, &source, at + 1, "v1")
                    .unwrap()
                    .is_none()
            );
        } else {
            let next = admin
                .reserve_repair_reader(&batch.scope, &source, at + 1, "v1")
                .unwrap()
                .unwrap();
            assert_eq!(next.attempt, 1);
            assert_eq!(next.evidence, batch.evidence);
        }
        kernel.stop_all().await.unwrap();
    }
}
