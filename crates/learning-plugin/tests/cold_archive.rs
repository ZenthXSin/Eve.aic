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
