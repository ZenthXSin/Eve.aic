mod support;

use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_learning_api::*;
use eve_learning_plugin::{LearningController, LearningPlugin};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::{
    CompletedInteraction, EvidenceSource, InteractionEvidence, MEMORY_PLUGIN_ID, MemoryAdmin,
    MemoryScope, MemorySnapshot,
};
use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryPlugin};
use eve_plugin_api::{PluginId, StateStore};
use eve_session_api::{SessionKey, SessionSnapshot, SessionTurn, SessionTurnStatus};
use serde_json::{Value, json};
use std::sync::{Arc, Barrier};
use support::{RecordingStore, memory, scope};

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
async fn open(state: Arc<dyn StateStore>) -> (Kernel, LearningController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap();
    let admin = plugin.controller();
    assert_eq!(
        admin.snapshot(&scope("s", "u")),
        Err(LearningError::Unavailable)
    );
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}
fn draft(batch: &LearningBatch) -> CandidateDraft {
    CandidateDraft {
        text: "用户可能偏好先看结论".into(),
        confidence: 75,
        evidence_ids: vec![batch.evidence[0].id.clone()],
    }
}
fn reserve(admin: &dyn LearningAdmin, memory: &MemorySnapshot, at_ms: u64) -> LearningBatch {
    admin
        .reserve(memory, at_ms, "extractor-v1", &options())
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn reserve_is_durable_before_return_and_candidates_do_not_confirm_memory() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let plugin = MemoryPlugin::new().unwrap();
    let memories = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let one = scope("group-a", "alice");
    let input = " 请先给结论。\n保留原文空白。 ";
    let reply = "结论：可以。\n下一步请确认。";
    let completed = SessionSnapshot {
        key: SessionKey::new(&one.session_id, &one.user_id).unwrap(),
        revision: 2,
        turns: vec![SessionTurn {
            id: 1,
            input: input.into(),
            status: SessionTurnStatus::Completed {
                messages: vec![
                    ChatMessage::text(ChatRole::User, input),
                    ChatMessage::text(ChatRole::Assistant, reply),
                ],
            },
        }],
    };
    completed.validate().unwrap();
    let source = memories
        .import_completed(
            &one,
            0,
            CompletedInteraction {
                evidence_id: "delivered-evidence".into(),
                message_id: "opaque-platform-message".into(),
                at_ms: 10,
                snapshot: completed,
                turn_id: 1,
            },
        )
        .unwrap();
    let memory_owner = PluginId::new(MEMORY_PLUGIN_ID).unwrap();
    let original = store.get(&memory_owner, MEMORY_STATE_KEY).unwrap();
    let batch = reserve(&admin, &source, 100);
    assert_eq!(batch.evidence, source.evidence);
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap();
    let persisted: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(persisted["jobs"][0]["job"]["status"], "Running");
    assert_eq!(
        serde_json::from_value::<LearningBatch>(persisted["jobs"][0]["job"]["batch"].clone())
            .unwrap(),
        batch
    );
    let candidate = draft(&batch);
    admin
        .finish(
            &batch,
            110,
            LearningOutcome::Completed(vec![candidate.clone()]),
        )
        .unwrap();
    let job = admin.snapshot(&one).unwrap().jobs.remove(0);
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.finished_at_ms, Some(110));
    assert_eq!(job.candidates.len(), 1);
    assert_eq!(job.candidates[0].draft, candidate);
    assert_eq!(job.candidates[0].batch_id, batch.id);
    assert_eq!(job.candidates[0].created_at_ms, 110);
    assert_eq!(job.candidates[0].expires_at_ms, 110 + CANDIDATE_TTL_MS);
    assert!(!job.candidates[0].id.is_empty());
    assert_eq!(memories.reader(one).unwrap().snapshot().unwrap(), source);
    assert_eq!(
        store.get(&memory_owner, MEMORY_STATE_KEY).unwrap(),
        original
    );
    let debug = format!("{batch:?} {job:?} {candidate:?}");
    for private in [input, reply, "alice", "group-a", "用户可能偏好"] {
        assert!(!debug.contains(private));
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicit_statements_never_trigger_or_enter_a_batch() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let mut source = memory(scope("s", "u"), 2);
    source.revision = 3;
    source.evidence.push(InteractionEvidence {
        id: "user-statement".into(),
        revision: 3,
        at_ms: 3,
        source: EvidenceSource::UserStatement {
            message_id: "statement-message".into(),
            text: "/remember 请称呼我博士".into(),
        },
    });
    assert!(
        admin
            .reserve(&source, 100, "v1", &LearningOptions::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(store.writes(), 0);
    let batch = reserve(&admin, &source, 100);
    assert_eq!(batch.evidence, source.evidence[..2]);
    assert!(
        batch
            .evidence
            .iter()
            .all(|item| matches!(item.source, EvidenceSource::CompletedInteraction { .. }))
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn completed_empty_and_failed_batches_consume_evidence_across_versions_and_restarts() {
    for failure in [None, Some(LearningFailure::Provider)] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        let (kernel, admin) = open(store.clone()).await;
        let source = memory(scope("s", "u"), 3);
        let batch = reserve(&admin, &source, 100);
        let outcome = || match &failure {
            Some(reason) => LearningOutcome::Failed(reason.clone()),
            None => LearningOutcome::Completed(vec![]),
        };
        admin.finish(&batch, 110, outcome()).unwrap();
        let saved = admin.snapshot(&source.scope).unwrap();
        let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let writes = store.writes();
        admin.finish(&batch, 999, outcome()).unwrap();
        assert!(
            admin
                .reserve(&source, 1000, "v2", &options())
                .unwrap()
                .is_none()
        );
        assert_eq!(store.writes(), writes);
        assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
        kernel.stop_all().await.unwrap();
        assert_eq!(
            admin.snapshot(&source.scope),
            Err(LearningError::Unavailable)
        );
        drop(kernel);
        drop(store);
        let store = RecordingStore::open(directory.path());
        let (kernel, restored) = open(store.clone()).await;
        assert_eq!(restored.snapshot(&source.scope).unwrap(), saved);
        assert!(
            restored
                .reserve(&source, 2000, "v3", &options())
                .unwrap()
                .is_none()
        );
        restored.finish(&batch, 3000, outcome()).unwrap();
        assert_eq!(store.writes(), 0);
        assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn pending_batch_recovers_as_interrupted_once_and_is_never_retried() {
    let directory = tempfile::tempdir().unwrap();
    let source = memory(scope("s", "u"), 3);
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let batch = reserve(&admin, &source, 100);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let (kernel, restored) = open(store.clone()).await;
    let recovered = restored.snapshot(&source.scope).unwrap();
    assert_eq!(recovered.jobs[0].status, JobStatus::Interrupted);
    assert_eq!(recovered.jobs[0].batch, batch);
    assert_eq!(recovered.jobs[0].finished_at_ms, None);
    assert!(recovered.jobs[0].candidates.is_empty());
    assert_eq!(store.writes(), 1);
    assert!(
        restored
            .reserve(&source, 200, "v2", &options())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        restored.finish(&batch, 200, LearningOutcome::Completed(vec![])),
        Err(LearningError::Conflict)
    );
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
    assert_eq!(store.writes(), 1);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let (kernel, third) = open(store.clone()).await;
    assert_eq!(third.snapshot(&source.scope).unwrap(), recovered);
    assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
    assert_eq!(store.writes(), 0);
    assert_eq!(
        admin.snapshot(&source.scope),
        Err(LearningError::Unavailable)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn channel_session_and_user_scopes_have_separate_consumption_and_cooldowns() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store).await;
    let base = scope("group-a", "alice");
    let variants = [
        base.clone(),
        scope("group-b", "alice"),
        scope("group-a", "bob"),
        MemoryScope {
            channel: "console".into(),
            ..base
        },
    ];
    let mut ids = std::collections::BTreeSet::new();
    for one in &variants {
        let source = memory(one.clone(), 3);
        let batch = admin
            .reserve(&source, 100, "v1", &LearningOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(batch.scope, *one);
        assert!(ids.insert(batch.id));
        let jobs = admin.snapshot(one).unwrap().jobs;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].batch.scope, *one);
    }
    assert!(
        admin
            .snapshot(&scope("missing", "unknown"))
            .unwrap()
            .jobs
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn persisted_cooldown_and_backward_clock_hold_new_evidence_until_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let first = memory(scope("s", "u"), 3);
    let batch = admin
        .reserve(&first, 1000, "v1", &LearningOptions::default())
        .unwrap()
        .unwrap();
    admin
        .finish(&batch, 1010, LearningOutcome::Completed(vec![]))
        .unwrap();
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let (kernel, restored) = open(store.clone()).await;
    let next = memory(first.scope, 6);
    for time in [999, 1000, 300_999] {
        assert!(
            restored
                .reserve(&next, time, "v1", &LearningOptions::default())
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(store.writes(), 0);
    let batch = restored
        .reserve(&next, 301_000, "v1", &LearningOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(batch.evidence, next.evidence[3..]);
    assert_eq!(store.writes(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn simultaneous_reservations_make_one_durable_batch() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("s", "u"), 3);
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let admin = admin.clone();
            let source = source.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                admin.reserve(&source, 100, "v1", &options()).unwrap()
            })
        })
        .collect();
    let batches: Vec<_> = handles
        .into_iter()
        .filter_map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(batches.len(), 1);
    assert_eq!(admin.snapshot(&source.scope).unwrap().jobs.len(), 1);
    assert_eq!(store.writes(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn consumed_sources_cannot_be_removed_rewritten_aliased_or_rewound() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("s", "u"), 3);
    reserve(&admin, &source, 100);
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let writes = store.writes();
    let mut variants = vec![];
    let mut removed = source.clone();
    removed.evidence.remove(0);
    variants.push(removed);
    let mut changed = source.clone();
    let EvidenceSource::CompletedInteraction { user_text, .. } = &mut changed.evidence[0].source
    else {
        unreachable!()
    };
    *user_text = "不能覆盖旧来源".into();
    variants.push(changed);
    let mut aliased = source.clone();
    aliased.evidence[0].id = "new-id-same-interaction".into();
    variants.push(aliased);
    variants.push(memory(source.scope.clone(), 2));
    for invalid in variants {
        assert_eq!(
            admin.reserve(&invalid, 200, "v2", &options()),
            Err(LearningError::Conflict)
        );
    }
    assert_eq!(store.writes(), writes);
    assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn candidate_validation_rejects_foreign_references_and_never_partially_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("s", "u"), 3);
    let batch = reserve(&admin, &source, 100);
    let valid = draft(&batch);
    let mut invalid = vec![];
    for text in [
        String::new(),
        "  ".into(),
        "has\0nul".into(),
        "长".repeat(MAX_CANDIDATE_BYTES),
    ] {
        invalid.push(CandidateDraft {
            text,
            ..valid.clone()
        });
    }
    invalid.push(CandidateDraft {
        confidence: 101,
        ..valid.clone()
    });
    for evidence_ids in [
        vec![],
        vec!["foreign-evidence".into()],
        vec!["e-1".into(), "e-1".into()],
    ] {
        invalid.push(CandidateDraft {
            evidence_ids,
            ..valid.clone()
        });
    }
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let writes = store.writes();
    for candidate in invalid {
        assert_eq!(
            admin.finish(&batch, 110, LearningOutcome::Completed(vec![candidate])),
            Err(LearningError::InvalidInput)
        );
    }
    assert_eq!(
        admin.finish(
            &batch,
            110,
            LearningOutcome::Completed(vec![valid.clone(); MAX_CANDIDATES + 1])
        ),
        Err(LearningError::InvalidInput)
    );
    assert_eq!(
        admin.finish(&batch, 99, LearningOutcome::Completed(vec![valid.clone()])),
        Err(LearningError::InvalidInput)
    );
    assert_eq!(
        admin.finish(
            &batch,
            u64::MAX,
            LearningOutcome::Completed(vec![valid.clone()])
        ),
        Err(LearningError::InvalidInput)
    );
    assert_eq!(store.writes(), writes);
    assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
    assert_eq!(
        admin.snapshot(&source.scope).unwrap().jobs[0].status,
        JobStatus::Running
    );
    admin
        .finish(&batch, 110, LearningOutcome::Completed(vec![valid.clone()]))
        .unwrap();
    let saved = admin.snapshot(&source.scope).unwrap();
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let writes = store.writes();
    admin
        .finish(
            &batch,
            1000,
            LearningOutcome::Completed(vec![valid.clone()]),
        )
        .unwrap();
    assert_eq!(
        admin.finish(
            &batch,
            111,
            LearningOutcome::Completed(vec![CandidateDraft {
                text: String::new(),
                ..valid.clone()
            }])
        ),
        Err(LearningError::Conflict)
    );
    assert_eq!(
        admin.finish(
            &batch,
            111,
            LearningOutcome::Failed(LearningFailure::Provider)
        ),
        Err(LearningError::Conflict)
    );
    let mut forged = batch.clone();
    forged.scope.user_id = "other-user".into();
    assert_eq!(
        admin.finish(&forged, 111, LearningOutcome::Completed(vec![valid])),
        Err(LearningError::Conflict)
    );
    assert_eq!(admin.snapshot(&source.scope).unwrap(), saved);
    assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn oversized_evidence_is_skipped_without_truncation_and_batches_obey_byte_and_count_limits() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store).await;
    // 长身份和提炼器版本也占用输入预算，不能只计 evidence 数组。
    let mut source = memory(
        MemoryScope {
            channel: "q".repeat(256),
            session_id: "s".repeat(256),
            user_id: "u".repeat(256),
        },
        12,
    );
    for (index, evidence) in source.evidence.iter_mut().enumerate() {
        let EvidenceSource::CompletedInteraction { user_text, .. } = &mut evidence.source else {
            unreachable!()
        };
        *user_text = "a".repeat(if index == 0 {
            MAX_EVIDENCE_BYTES + 1
        } else {
            7900
        });
    }
    let first = admin
        .reserve(&source, 100, &"v".repeat(256), &options())
        .unwrap()
        .unwrap();
    assert!(first.evidence.len() < MAX_BATCH_EVIDENCE);
    assert_eq!(first.evidence, source.evidence[1..1 + first.evidence.len()]);
    assert!(serde_json::to_vec(&first.evidence).unwrap().len() <= MAX_INPUT_BYTES);
    assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_INPUT_BYTES);
    assert!(
        first
            .evidence
            .iter()
            .all(|item| serde_json::to_vec(item).unwrap().len() <= MAX_EVIDENCE_BYTES)
    );
    let second = reserve(&admin, &source, 101);
    assert!(
        second
            .evidence
            .iter()
            .all(|item| !first.evidence.contains(item))
    );
    let small = memory(scope("different", "u"), MAX_BATCH_EVIDENCE + 2);
    let limited = reserve(&admin, &small, 100);
    assert_eq!(limited.evidence.len(), MAX_BATCH_EVIDENCE);
    assert_eq!(limited.evidence, small.evidence[..MAX_BATCH_EVIDENCE]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn reserve_storage_failure_closes_all_scopes_and_recovery_obeys_committed_bytes() {
    for after_write in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        let (kernel, admin) = open(store.clone()).await;
        let source = memory(scope("s", "u"), 1);
        store.fail_before(!after_write);
        store.fail_after(after_write);
        let error = admin.reserve(&source, 100, "v1", &options()).unwrap_err();
        assert_eq!(error, LearningError::Storage);
        assert!(!format!("{error:?} {error}").contains("private-storage-detail"));
        assert_eq!(
            store.get(&owner(), LEARNING_STATE_KEY).unwrap().is_some(),
            after_write
        );
        for one in [&source.scope, &scope("other", "other")] {
            assert_eq!(admin.snapshot(one), Err(LearningError::Unavailable));
        }
        let writes = store.writes();
        store.fail_before(false);
        store.fail_after(false);
        assert_eq!(
            admin.reserve(&source, 200, "v1", &options()),
            Err(LearningError::Unavailable)
        );
        assert_eq!(store.writes(), writes);
        kernel.stop_all().await.unwrap();
        drop(kernel);
        drop(store);
        let store = RecordingStore::open(directory.path());
        let (kernel, restored) = open(store.clone()).await;
        let snapshot = restored.snapshot(&source.scope).unwrap();
        if after_write {
            assert_eq!(snapshot.jobs.len(), 1);
            assert_eq!(snapshot.jobs[0].status, JobStatus::Interrupted);
            assert!(
                restored
                    .reserve(&source, 200, "v1", &options())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(store.writes(), 1);
        } else {
            assert!(snapshot.jobs.is_empty());
            assert_eq!(store.writes(), 0);
            reserve(&restored, &source, 200);
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn finish_storage_failure_recovers_either_interruption_or_exact_candidates() {
    for after_write in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        let (kernel, admin) = open(store.clone()).await;
        let source = memory(scope("s", "u"), 1);
        let batch = reserve(&admin, &source, 100);
        let candidate = draft(&batch);
        let before = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
        store.fail_before(!after_write);
        store.fail_after(after_write);
        assert_eq!(
            admin.finish(
                &batch,
                110,
                LearningOutcome::Completed(vec![candidate.clone()])
            ),
            Err(LearningError::Storage)
        );
        let committed = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
        assert_eq!(committed == before, !after_write);
        assert_eq!(
            admin.snapshot(&source.scope),
            Err(LearningError::Unavailable)
        );
        assert_eq!(
            admin.snapshot(&scope("other", "other")),
            Err(LearningError::Unavailable)
        );
        store.fail_before(false);
        store.fail_after(false);
        let writes = store.writes();
        assert_eq!(
            admin.finish(
                &batch,
                110,
                LearningOutcome::Completed(vec![candidate.clone()])
            ),
            Err(LearningError::Unavailable)
        );
        assert_eq!(store.writes(), writes);
        kernel.stop_all().await.unwrap();
        drop(kernel);
        drop(store);
        let store = RecordingStore::open(directory.path());
        let (kernel, restored) = open(store.clone()).await;
        let job = restored.snapshot(&source.scope).unwrap().jobs.remove(0);
        assert!(
            restored
                .reserve(&source, 200, "v2", &options())
                .unwrap()
                .is_none()
        );
        if after_write {
            assert_eq!(job.status, JobStatus::Completed);
            assert_eq!(job.candidates[0].draft, candidate);
            restored
                .finish(&batch, 200, LearningOutcome::Completed(vec![candidate]))
                .unwrap();
            assert_eq!(store.writes(), 0);
            assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), committed);
        } else {
            assert_eq!(job.status, JobStatus::Interrupted);
            assert!(job.candidates.is_empty());
            assert_eq!(store.writes(), 1);
        }
        kernel.stop_all().await.unwrap();
    }
}

async fn rejects_persisted_bytes(bytes: Vec<u8>) {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    store
        .set(&owner(), LEARNING_STATE_KEY.into(), bytes.clone())
        .unwrap();
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let writes = store.writes();
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(
        admin.snapshot(&scope("s", "u")),
        Err(LearningError::Unavailable)
    );
    assert_eq!(store.writes(), writes);
    assert_eq!(
        store.get(&owner(), LEARNING_STATE_KEY).unwrap(),
        Some(bytes)
    );
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
}

#[tokio::test]
async fn corrupted_unknown_and_inconsistent_records_preserve_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let batch = reserve(&admin, &memory(scope("s", "u"), 3), 100);
    admin
        .finish(&batch, 110, LearningOutcome::Completed(vec![draft(&batch)]))
        .unwrap();
    let valid: Value =
        serde_json::from_slice(&store.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    kernel.stop_all().await.unwrap();
    let cases: [fn(&mut Value); 13] = [
        |value| value["format_version"] = json!(2),
        |value| value["unknown"] = json!(true),
        |value| value["jobs"][0]["memory_revision"] = json!(0),
        |value| {
            value["jobs"][0]["job"]["batch"]["evidence"][0]["source"]["user_text"] = json!("被篡改")
        },
        |value| value["jobs"][0]["job"]["batch"]["scope"]["user_id"] = json!("other"),
        |value| value["jobs"][0]["job"]["finished_at_ms"] = json!(99),
        |value| value["jobs"][0]["job"]["status"] = json!("Running"),
        |value| value["jobs"][0]["job"]["candidates"][0]["id"] = json!("forged"),
        |value| value["jobs"][0]["job"]["candidates"][0]["batch_id"] = json!("foreign"),
        |value| {
            value["jobs"][0]["job"]["candidates"][0]["draft"]["evidence_ids"] = json!(["foreign"])
        },
        |value| value["jobs"][0]["job"]["candidates"][0]["expires_at_ms"] = json!(111),
        |value| value["jobs"][0]["job"]["candidates"][0]["created_at_ms"] = json!(109),
        |value| {
            let duplicate = value["jobs"][0].clone();
            value["jobs"].as_array_mut().unwrap().push(duplicate);
        },
    ];
    for mutate in cases {
        let mut damaged = valid.clone();
        mutate(&mut damaged);
        rejects_persisted_bytes(serde_json::to_vec(&damaged).unwrap()).await;
    }
    for bytes in [
        b"{truncated".to_vec(),
        b"null".to_vec(),
        vec![b' '; MAX_STATE_BYTES + 1],
    ] {
        rejects_persisted_bytes(bytes).await;
    }
}

#[tokio::test]
async fn duplicate_json_keys_at_document_evidence_and_candidate_levels_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let batch = reserve(&admin, &memory(scope("s", "u"), 1), 100);
    admin
        .finish(&batch, 110, LearningOutcome::Completed(vec![draft(&batch)]))
        .unwrap();
    let valid =
        String::from_utf8(store.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap()).unwrap();
    kernel.stop_all().await.unwrap();
    for field in [
        "\"format_version\":1",
        "\"message_id\":\"message-1\"",
        "\"confidence\":75",
    ] {
        assert!(valid.contains(field));
        let damaged = valid.replacen(field, &format!("{field},{field}"), 1);
        rejects_persisted_bytes(damaged.into_bytes()).await;
    }
}

#[tokio::test]
async fn recovery_must_persist_interruption_before_exposing_an_instance() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("s", "u"), 1);
    reserve(&admin, &source, 100);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let before = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
    store.fail_before(true);
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap();
    let closed = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(
        closed.snapshot(&source.scope),
        Err(LearningError::Unavailable)
    );
    assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), before);
    store.fail_before(false);
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let (kernel, restored) = open(store).await;
    assert_eq!(
        restored.snapshot(&source.scope).unwrap().jobs[0].status,
        JobStatus::Interrupted
    );
    assert_eq!(
        closed.snapshot(&source.scope),
        Err(LearningError::Unavailable)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn global_job_capacity_preserves_existing_jobs_and_read_only_replays() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let mut first = None;
    for index in 0..MAX_JOBS {
        let source = memory(scope(&format!("s-{index}"), "u"), 1);
        let batch = reserve(&admin, &source, 100);
        admin
            .finish(&batch, 110, LearningOutcome::Completed(vec![]))
            .unwrap();
        if index == 0 {
            first = Some((source, batch));
        }
    }
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
    let writes = store.writes();
    assert_eq!(
        admin.reserve(&memory(scope("overflow", "u"), 1), 100, "v1", &options()),
        Err(LearningError::LimitReached)
    );
    assert!(
        admin
            .snapshot(&scope("overflow", "u"))
            .unwrap()
            .jobs
            .is_empty()
    );
    let (source, batch) = first.unwrap();
    assert!(
        admin
            .reserve(&source, 200, "v2", &options())
            .unwrap()
            .is_none()
    );
    admin
        .finish(&batch, 200, LearningOutcome::Completed(vec![]))
        .unwrap();
    assert_eq!(store.writes(), writes);
    assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn global_byte_capacity_rejects_new_bytes_without_removing_old_sources() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    let mut reached = false;
    for index in 0..MAX_JOBS {
        let mut source = memory(
            scope(&format!("byte-scope-{index}"), "u"),
            MAX_BATCH_EVIDENCE,
        );
        for evidence in &mut source.evidence {
            let EvidenceSource::CompletedInteraction { user_text, .. } = &mut evidence.source
            else {
                unreachable!()
            };
            *user_text = "a".repeat(3600);
        }
        let before = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let batch = match admin.reserve(&source, 100, "v1", &options()) {
            Ok(Some(batch)) => batch,
            Err(LearningError::LimitReached) => {
                assert_eq!(store.get(&owner(), LEARNING_STATE_KEY).unwrap(), before);
                assert!(admin.snapshot(&source.scope).unwrap().jobs.is_empty());
                reached = true;
                break;
            }
            other => panic!("unexpected reservation: {other:?}"),
        };
        assert_eq!(batch.evidence.len(), MAX_BATCH_EVIDENCE);
        let before_finish = store.get(&owner(), LEARNING_STATE_KEY).unwrap();
        let drafts = (0..MAX_CANDIDATES)
            .map(|candidate| CandidateDraft {
                text: format!("{candidate}{}", "x".repeat(999)),
                ..draft(&batch)
            })
            .collect();
        match admin.finish(&batch, 110, LearningOutcome::Completed(drafts)) {
            Ok(()) => {}
            Err(LearningError::LimitReached) => {
                assert_eq!(
                    store.get(&owner(), LEARNING_STATE_KEY).unwrap(),
                    before_finish
                );
                let job = admin.snapshot(&source.scope).unwrap().jobs.remove(0);
                assert_eq!(job.status, JobStatus::Running);
                assert!(job.candidates.is_empty());
                reached = true;
                break;
            }
            other => panic!("unexpected completion: {other:?}"),
        }
    }
    assert!(reached, "byte budget must be reached before the job limit");
    let bytes = store.get(&owner(), LEARNING_STATE_KEY).unwrap().unwrap();
    assert!(bytes.len() <= MAX_STATE_BYTES);
    assert_eq!(
        admin.snapshot(&scope("byte-scope-0", "u")).unwrap().jobs[0].status,
        JobStatus::Completed
    );
    kernel.stop_all().await.unwrap();

    // 用真实公开 API 生成新记录，构造合法的大状态夹具，让 reserve 精确触及
    // 边界：仍须留出 Running 恢复为 Interrupted 所需的四个字节。
    let addition_source = memory(scope("capacity-boundary", "u"), 1);
    let probe_store = Arc::new(MemoryStateStore::default());
    let (probe_kernel, probe) = open(probe_store.clone()).await;
    reserve(&probe, &addition_source, 100);
    let probe_document: Value = serde_json::from_slice(
        &probe_store
            .get(&owner(), LEARNING_STATE_KEY)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let addition_bytes = serde_json::to_vec(&probe_document["jobs"][0])
        .unwrap()
        .len()
        + 1;
    probe_kernel.stop_all().await.unwrap();
    let mut fixture: Value = serde_json::from_slice(&bytes).unwrap();
    let jobs = fixture["jobs"].as_array_mut().unwrap();
    assert!(jobs.len() < MAX_JOBS);
    for record in jobs.iter_mut() {
        if record["job"]["status"] == "Running" {
            record["job"]["status"] = json!({"Failed": "Cancelled"});
            record["job"]["finished_at_ms"] = json!(110);
        }
        for candidate in record["job"]["candidates"].as_array_mut().unwrap() {
            candidate["draft"]["text"] = json!("x");
        }
    }
    let target = MAX_STATE_BYTES - addition_bytes - 3;
    let mut remaining = target - serde_json::to_vec(&fixture).unwrap().len();
    for record in fixture["jobs"].as_array_mut().unwrap() {
        for candidate in record["job"]["candidates"].as_array_mut().unwrap() {
            // U+0001 合法但 JSON 编码为六字节；原始文本仍不超过 1024 字节。
            let growth = remaining.min(5999);
            let body_bytes = growth + 1;
            candidate["draft"]["text"] = json!(format!(
                "{}{}",
                "\u{1}".repeat(body_bytes / 6),
                "x".repeat(body_bytes % 6)
            ));
            remaining -= growth;
        }
    }
    assert_eq!(remaining, 0);
    let oversized = serde_json::to_vec(&fixture).unwrap();
    assert_eq!(oversized.len(), target);
    store
        .set(&owner(), LEARNING_STATE_KEY.into(), oversized.clone())
        .unwrap();
    let (boundary_kernel, boundary) = open(store.clone()).await;
    assert_eq!(
        boundary.reserve(&addition_source, 100, "extractor-v1", &options()),
        Err(LearningError::LimitReached)
    );
    assert_eq!(
        store.get(&owner(), LEARNING_STATE_KEY).unwrap(),
        Some(oversized)
    );
    assert!(
        boundary
            .snapshot(&addition_source.scope)
            .unwrap()
            .jobs
            .is_empty()
    );
    boundary_kernel.stop_all().await.unwrap();

    // 多留一字节后可以提交；恢复恰好达到四 MiB，也必须完整保存中断结局。
    let padding = &mut fixture["jobs"][0]["job"]["candidates"][0]["draft"]["text"];
    let mut adjusted = padding.as_str().unwrap().to_owned();
    assert_eq!(adjusted.pop(), Some('\u{1}'));
    adjusted.push_str("xxxxx");
    *padding = json!(adjusted);
    let exact = serde_json::to_vec(&fixture).unwrap();
    assert_eq!(exact.len(), target - 1);
    store
        .set(&owner(), LEARNING_STATE_KEY.into(), exact)
        .unwrap();
    let (exact_kernel, exact_admin) = open(store.clone()).await;
    let accepted = reserve(&exact_admin, &addition_source, 100);
    assert_eq!(
        store
            .get(&owner(), LEARNING_STATE_KEY)
            .unwrap()
            .unwrap()
            .len(),
        MAX_STATE_BYTES - 4
    );
    exact_kernel.stop_all().await.unwrap();
    let (recovered_kernel, recovered) = open(store.clone()).await;
    let recovered_job = recovered
        .snapshot(&addition_source.scope)
        .unwrap()
        .jobs
        .remove(0);
    assert_eq!(recovered_job.batch, accepted);
    assert_eq!(recovered_job.status, JobStatus::Interrupted);
    assert!(recovered_job.candidates.is_empty());
    assert_eq!(
        store
            .get(&owner(), LEARNING_STATE_KEY)
            .unwrap()
            .unwrap()
            .len(),
        MAX_STATE_BYTES
    );
    recovered_kernel.stop_all().await.unwrap();
}
