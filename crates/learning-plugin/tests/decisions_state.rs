mod support;

use eve_kernel::{Kernel, KernelServices};
use eve_learning_api::*;
use eve_learning_plugin::{LearningController, LearningPlugin};
use eve_memory_api::{MemoryScope, MemorySnapshot};
use eve_plugin_api::{PluginId, StateStore};
use serde_json::{Value, json};
use std::sync::{Arc, Barrier};
use support::{RecordingStore, memory, scope};

const DECISIONS_KEY: &str = "learning.decisions.v1";

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

async fn open(store: Arc<dyn StateStore>) -> (Kernel, LearningController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new().unwrap();
    let admin = plugin.controller();
    assert_eq!(
        admin.decisions(&scope("session", "owner")),
        Err(LearningError::Unavailable)
    );
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}

fn complete(
    admin: &dyn LearningAdmin,
    source: &MemorySnapshot,
    at_ms: u64,
) -> (LearningBatch, PreferenceCandidate) {
    let batch = admin
        .reserve(source, at_ms, "extractor-v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(
            &batch,
            at_ms + 10,
            LearningOutcome::Completed(vec![CandidateDraft {
                text: "请先给结论，然后保留必要解释".into(),
                confidence: 90,
                evidence_ids: batch.evidence.iter().map(|item| item.id.clone()).collect(),
            }]),
        )
        .unwrap();
    let candidate = admin
        .snapshot(&source.scope)
        .unwrap()
        .jobs
        .into_iter()
        .find(|job| job.batch == batch)
        .unwrap()
        .candidates
        .remove(0);
    (batch, candidate)
}

fn decision(candidate: &PreferenceCandidate, memory_revision: u64) -> LearningDecision {
    LearningDecision {
        candidate_id: candidate.id.clone(),
        batch_id: candidate.batch_id.clone(),
        policy_version: "evidence-policy-v1".into(),
        memory_revision,
        evidence_ids: candidate.draft.evidence_ids.clone(),
        action: LearningDecisionAction::Confirm,
        reason: DecisionReason::Eligible,
    }
}

fn bytes(store: &dyn StateStore, key: &str) -> Option<Vec<u8>> {
    store.get(&owner(), key).unwrap()
}

#[tokio::test]
async fn old_learning_directory_without_decision_key_opens_and_reads_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 2);
    complete(&admin, &source, 100);
    let old_state = bytes(store.as_ref(), LEARNING_STATE_KEY).unwrap();
    let document: Value = serde_json::from_slice(&old_state).unwrap();
    assert_eq!(document["format_version"], 1);
    assert_eq!(document.as_object().unwrap().len(), 2);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), None);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let original_disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let (kernel, restored) = open(store.clone()).await;
    assert!(restored.decisions(&source.scope).unwrap().is_empty());
    assert!(
        restored
            .decisions(&scope("different-session", "other"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(restored.snapshot(&source.scope).unwrap().jobs.len(), 1);
    assert_eq!(store.writes(), 0);
    assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), Some(old_state));
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), None);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        original_disk
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn decisions_reference_completed_candidates_and_survive_restart_without_mutating_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let source = memory(scope("private-session", "private-user"), 2);
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let (batch, candidate) = complete(&admin, &source, 100);
    let old_state = bytes(store.as_ref(), LEARNING_STATE_KEY);
    let jobs = admin.snapshot(&source.scope).unwrap();
    let intended = decision(&candidate, source.revision);
    let record = admin
        .record_decision(&source.scope, intended.clone(), 120)
        .unwrap();
    assert_eq!(record.sequence, 1);
    assert_eq!(record.at_ms, 120);
    assert_eq!(record.decision, intended);
    assert_eq!(record.decision.batch_id, batch.id);
    assert_eq!(record.decision.evidence_ids, vec!["e-1", "e-2"]);
    assert_eq!(
        admin.decisions(&source.scope).unwrap(),
        vec![record.clone()]
    );
    assert_eq!(admin.snapshot(&source.scope).unwrap(), jobs);
    assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), old_state);
    let ledger = bytes(store.as_ref(), DECISIONS_KEY).unwrap();
    let persisted: Value = serde_json::from_slice(&ledger).unwrap();
    assert_eq!(persisted["format_version"], 1);
    assert_eq!(persisted["records"][0]["scope"], json!(source.scope));
    assert_eq!(persisted["records"][0]["record"], json!(record));
    assert!(
        !String::from_utf8(ledger.clone())
            .unwrap()
            .contains(&candidate.draft.text)
    );
    let debug = format!(
        "{record:?} {:?} {:?}",
        record.decision, record.decision.action
    );
    for private in [
        &candidate.id,
        &batch.id,
        &source.scope.user_id,
        &candidate.draft.text,
    ] {
        assert!(!debug.contains(private));
    }
    kernel.stop_all().await.unwrap();
    assert_eq!(
        admin.decisions(&source.scope),
        Err(LearningError::Unavailable)
    );
    drop(kernel);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let (kernel, restored) = open(store.clone()).await;
    assert_eq!(restored.decisions(&source.scope).unwrap(), vec![record]);
    assert_eq!(restored.snapshot(&source.scope).unwrap(), jobs);
    assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), old_state);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), Some(ledger));
    assert_eq!(store.writes(), 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn memory_revision_and_replay_clock_changes_reuse_last_outcome_without_writes_or_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 2);
    let (_, candidate) = complete(&admin, &source, 100);
    let intended = decision(&candidate, source.revision);
    let saved = admin
        .record_decision(&source.scope, intended.clone(), 120)
        .unwrap();
    let before = bytes(store.as_ref(), DECISIONS_KEY);
    let writes = store.writes();
    let mut replay = intended.clone();
    replay.memory_revision += 1;
    assert_eq!(
        admin
            .record_decision(&source.scope, replay.clone(), 999)
            .unwrap(),
        saved
    );
    assert_eq!(
        admin.record_decision(&source.scope, intended, 121).unwrap(),
        saved
    );
    assert_eq!(admin.decisions(&source.scope).unwrap(), vec![saved.clone()]);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), before);
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let (kernel, restored) = open(store.clone()).await;
    assert_eq!(
        restored
            .record_decision(&source.scope, replay, 1000)
            .unwrap(),
        saved
    );
    assert_eq!(restored.decisions(&source.scope).unwrap(), vec![saved]);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), before);
    assert_eq!(store.writes(), 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn simultaneous_replays_commit_one_record_and_share_the_same_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 2);
    let (_, candidate) = complete(&admin, &source, 100);
    let intended = decision(&candidate, source.revision);
    let writes = store.writes();
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let admin = admin.clone();
            let scope = source.scope.clone();
            let mut intended = intended.clone();
            intended.memory_revision += index;
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                admin
                    .record_decision(&scope, intended, 120 + index)
                    .unwrap()
            })
        })
        .collect();
    let records: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert!(records.iter().all(|record| record == &records[0]));
    assert_eq!(records[0].sequence, 1);
    assert_eq!(
        admin.decisions(&source.scope).unwrap(),
        vec![records[0].clone()]
    );
    assert_eq!(store.writes(), writes + 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn changed_outcomes_append_history_and_do_not_erase_an_earlier_matching_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 2);
    let (_, candidate) = complete(&admin, &source, 100);
    let first = decision(&candidate, source.revision);
    let deferred = LearningDecision {
        action: LearningDecisionAction::Defer,
        reason: DecisionReason::AmbiguousConflict,
        ..first.clone()
    };
    let update = LearningDecision {
        action: LearningDecisionAction::Update {
            preference_id: "existing-preference".into(),
            expected_revision: 3,
        },
        reason: DecisionReason::ExplicitRevisionUpdate,
        memory_revision: 4,
        ..first.clone()
    };
    let mut expected = vec![];
    for (index, intended) in [first.clone(), deferred, first.clone(), update]
        .into_iter()
        .enumerate()
    {
        let record = admin
            .record_decision(&source.scope, intended.clone(), 120 + index as u64)
            .unwrap();
        assert_eq!(record.sequence, index as u64 + 1);
        assert_eq!(record.decision, intended);
        expected.push(record);
    }
    assert_eq!(admin.decisions(&source.scope).unwrap(), expected);
    let mut new_policy = first;
    new_policy.policy_version = "evidence-policy-v2".into();
    let fifth = admin
        .record_decision(&source.scope, new_policy, 124)
        .unwrap();
    assert_eq!(fifth.sequence, 5);
    expected.push(fifth);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let (kernel, restored) = open(store.clone()).await;
    assert_eq!(restored.decisions(&source.scope).unwrap(), expected);
    assert_eq!(store.writes(), 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn channel_session_and_user_scopes_isolate_records_and_have_independent_sequences() {
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
    let mut expected = vec![];
    for one in &variants {
        let source = memory(one.clone(), 2);
        let (_, candidate) = complete(&admin, &source, 100);
        let first = admin
            .record_decision(one, decision(&candidate, source.revision), 120)
            .unwrap();
        assert_eq!(first.sequence, 1);
        expected.push(vec![first]);
    }
    for (index, one) in variants.iter().enumerate() {
        assert_eq!(admin.decisions(one).unwrap(), expected[index]);
    }
    assert!(
        admin
            .decisions(&scope("missing", "nobody"))
            .unwrap()
            .is_empty()
    );
    let mut changed = expected[0][0].decision.clone();
    changed.policy_version = "new-policy".into();
    let second = admin.record_decision(&variants[0], changed, 121).unwrap();
    assert_eq!(second.sequence, 2);
    for (index, one) in variants.iter().enumerate().skip(1) {
        assert_eq!(admin.decisions(one).unwrap(), expected[index]);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_shapes_foreign_jobs_scopes_references_and_stale_revisions_never_write() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 3);
    let (_, candidate) = complete(&admin, &source, 100);
    let valid = decision(&candidate, source.revision);
    let running_source = memory(scope("running-session", "owner"), 1);
    let running = admin
        .reserve(&running_source, 100, "extractor-v1", &options())
        .unwrap()
        .unwrap();
    let failed_source = memory(scope("failed-session", "owner"), 1);
    let failed = admin
        .reserve(&failed_source, 100, "extractor-v1", &options())
        .unwrap()
        .unwrap();
    admin
        .finish(
            &failed,
            110,
            LearningOutcome::Failed(LearningFailure::Provider),
        )
        .unwrap();
    let mut invalid = vec![];
    for candidate_id in [String::new(), "unknown-candidate".into()] {
        invalid.push(LearningDecision {
            candidate_id,
            ..valid.clone()
        });
    }
    for batch_id in [String::new(), "unknown-batch".into(), running.id, failed.id] {
        invalid.push(LearningDecision {
            batch_id,
            ..valid.clone()
        });
    }
    for evidence_ids in [
        vec![],
        vec!["unknown-evidence".into()],
        vec!["e-1".into()],
        vec!["e-1".into(), "e-1".into()],
        vec!["e-3".into(), "e-2".into(), "e-1".into()],
    ] {
        invalid.push(LearningDecision {
            evidence_ids,
            ..valid.clone()
        });
    }
    for memory_revision in [0, source.revision - 1] {
        invalid.push(LearningDecision {
            memory_revision,
            ..valid.clone()
        });
    }
    invalid.push(LearningDecision {
        policy_version: " ".into(),
        ..valid.clone()
    });
    invalid.push(LearningDecision {
        reason: DecisionReason::PolicyDenied,
        ..valid.clone()
    });
    invalid.push(LearningDecision {
        action: LearningDecisionAction::Update {
            preference_id: "preference".into(),
            expected_revision: 0,
        },
        reason: DecisionReason::ExplicitRevisionUpdate,
        ..valid.clone()
    });
    let jobs = bytes(store.as_ref(), LEARNING_STATE_KEY);
    let writes = store.writes();
    for intended in invalid {
        assert!(matches!(
            admin.record_decision(&source.scope, intended, 120),
            Err(LearningError::InvalidInput | LearningError::Conflict)
        ));
    }
    for one in [
        &scope("other-session", "owner"),
        &scope("session", "other-user"),
        &running_source.scope,
        &failed_source.scope,
    ] {
        assert!(matches!(
            admin.record_decision(one, valid.clone(), 120),
            Err(LearningError::InvalidInput | LearningError::Conflict)
        ));
    }
    assert!(matches!(
        admin.record_decision(&source.scope, valid.clone(), candidate.created_at_ms - 1),
        Err(LearningError::InvalidInput | LearningError::Conflict)
    ));
    assert!(matches!(
        admin.decisions(&MemoryScope {
            user_id: "".into(),
            ..source.scope.clone()
        }),
        Err(LearningError::InvalidInput)
    ));
    assert_eq!(store.writes(), writes);
    assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), jobs);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), None);
    assert!(admin.decisions(&source.scope).unwrap().is_empty());
    assert_eq!(
        admin
            .record_decision(&source.scope, valid, 120)
            .unwrap()
            .sequence,
        1
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_commit_closes_every_learning_operation_and_restart_recovers_actual_durable_result()
{
    for after_write in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        let (kernel, admin) = open(store.clone()).await;
        let source = memory(scope("session", "owner"), 2);
        let (batch, candidate) = complete(&admin, &source, 100);
        let first = admin
            .record_decision(&source.scope, decision(&candidate, source.revision), 120)
            .unwrap();
        let next = LearningDecision {
            policy_version: "new-policy".into(),
            ..first.decision.clone()
        };
        let original_jobs = bytes(store.as_ref(), LEARNING_STATE_KEY);
        let original_ledger = bytes(store.as_ref(), DECISIONS_KEY);
        store.fail_before(!after_write);
        store.fail_after(after_write);
        let error = admin
            .record_decision(&source.scope, next.clone(), 130)
            .unwrap_err();
        assert_eq!(error, LearningError::Storage);
        assert!(!format!("{error:?} {error}").contains("private-storage-detail"));
        let committed = bytes(store.as_ref(), DECISIONS_KEY);
        assert_eq!(committed == original_ledger, !after_write);
        assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), original_jobs);
        let writes = store.writes();
        store.fail_before(false);
        store.fail_after(false);
        for one in [&source.scope, &scope("other", "other")] {
            assert_eq!(admin.decisions(one), Err(LearningError::Unavailable));
            assert_eq!(admin.snapshot(one), Err(LearningError::Unavailable));
        }
        assert_eq!(
            admin.record_decision(&source.scope, next.clone(), 131),
            Err(LearningError::Unavailable)
        );
        assert_eq!(
            admin.reserve(&source, 140, "another-extractor", &options()),
            Err(LearningError::Unavailable)
        );
        assert_eq!(
            admin.finish(
                &batch,
                140,
                LearningOutcome::Completed(vec![candidate.draft.clone()])
            ),
            Err(LearningError::Unavailable)
        );
        assert_eq!(store.writes(), writes);
        kernel.stop_all().await.unwrap();
        drop(kernel);
        drop(store);

        let store = RecordingStore::open(directory.path());
        let (kernel, restored) = open(store.clone()).await;
        let records = restored.decisions(&source.scope).unwrap();
        assert_eq!(records.len(), if after_write { 2 } else { 1 });
        assert_eq!(records[0], first);
        assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), committed);
        assert_eq!(store.writes(), 0);
        let recovered = restored
            .record_decision(&source.scope, next.clone(), 999)
            .unwrap();
        assert_eq!(recovered.sequence, 2);
        assert_eq!(recovered.decision, next);
        assert_eq!(recovered.at_ms, if after_write { 130 } else { 999 });
        assert_eq!(store.writes(), usize::from(!after_write));
        assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), original_jobs);
        assert_eq!(
            restored.snapshot(&source.scope).unwrap().jobs[0].status,
            JobStatus::Completed
        );
        kernel.stop_all().await.unwrap();
    }
}

async fn rejects_bytes(jobs: &[u8], ledger: Vec<u8>) {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    store
        .set(&owner(), LEARNING_STATE_KEY.into(), jobs.to_vec())
        .unwrap();
    store
        .set(&owner(), DECISIONS_KEY.into(), ledger.clone())
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
        admin.snapshot(&scope("session", "owner")),
        Err(LearningError::Unavailable)
    );
    assert_eq!(
        admin.decisions(&scope("session", "owner")),
        Err(LearningError::Unavailable)
    );
    assert_eq!(store.writes(), writes);
    assert_eq!(
        bytes(store.as_ref(), LEARNING_STATE_KEY),
        Some(jobs.to_vec())
    );
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), Some(ledger));
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
}

#[tokio::test]
async fn unknown_duplicate_malformed_versions_sequences_and_cross_references_refuse_recovery_without_clearing()
 {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 2);
    let (_, candidate) = complete(&admin, &source, 100);
    let first = decision(&candidate, source.revision);
    admin
        .record_decision(&source.scope, first.clone(), 120)
        .unwrap();
    admin
        .record_decision(
            &source.scope,
            LearningDecision {
                policy_version: "new-policy".into(),
                ..first
            },
            121,
        )
        .unwrap();
    let jobs = bytes(store.as_ref(), LEARNING_STATE_KEY).unwrap();
    let valid = bytes(store.as_ref(), DECISIONS_KEY).unwrap();
    kernel.stop_all().await.unwrap();
    let valid: Value = serde_json::from_slice(&valid).unwrap();
    let cases: &[fn(&mut Value)] = &[
        |value| value["format_version"] = json!(2),
        |value| value["unknown"] = json!(true),
        |value| value["records"][0]["unknown"] = json!(true),
        |value| value["records"][0]["scope"]["unknown"] = json!(true),
        |value| value["records"][0]["record"]["unknown"] = json!(true),
        |value| value["records"][0]["record"]["decision"]["unknown"] = json!(true),
        |value| value["records"][0]["record"]["sequence"] = json!(0),
        |value| value["records"][0]["record"]["sequence"] = json!(2),
        |value| value["records"][1]["record"]["sequence"] = json!(1),
        |value| value["records"][1]["record"]["sequence"] = json!(3),
        |value| value["records"][0]["record"]["at_ms"] = json!(109),
        |value| value["records"][0]["record"]["decision"]["memory_revision"] = json!(0),
        |value| value["records"][0]["record"]["decision"]["memory_revision"] = json!(1),
        |value| value["records"][0]["record"]["decision"]["candidate_id"] = json!("unknown"),
        |value| value["records"][0]["record"]["decision"]["batch_id"] = json!("unknown"),
        |value| value["records"][0]["record"]["decision"]["evidence_ids"] = json!(["unknown"]),
        |value| value["records"][0]["record"]["decision"]["evidence_ids"] = json!(["e-1"]),
        |value| value["records"][0]["record"]["decision"]["evidence_ids"] = json!(["e-1", "e-1"]),
        |value| value["records"][0]["record"]["decision"]["reason"] = json!("PolicyDenied"),
        |value| value["records"][0]["scope"]["user_id"] = json!("other-user"),
        |value| value["records"][0]["scope"]["session_id"] = json!("other-session"),
        |value| value["records"][0]["scope"]["channel"] = json!("other-channel"),
        |value| {
            value["records"][1]["record"]["decision"] =
                value["records"][0]["record"]["decision"].clone();
        },
        |value| {
            value["records"].as_array_mut().unwrap().swap(0, 1);
        },
    ];
    for mutate in cases {
        let mut corrupt = valid.clone();
        mutate(&mut corrupt);
        rejects_bytes(&jobs, serde_json::to_vec(&corrupt).unwrap()).await;
    }
    rejects_bytes(
        br#"{"format_version":1,"jobs":[]}"#,
        serde_json::to_vec(&valid).unwrap(),
    )
    .await;
    let mut non_completed: Value = serde_json::from_slice(&jobs).unwrap();
    non_completed["jobs"][0]["job"]["status"] = json!("Running");
    non_completed["jobs"][0]["job"]["finished_at_ms"] = Value::Null;
    non_completed["jobs"][0]["job"]["candidates"] = json!([]);
    rejects_bytes(
        &serde_json::to_vec(&non_completed).unwrap(),
        serde_json::to_vec(&valid).unwrap(),
    )
    .await;
    let mut over_capacity = valid.clone();
    let template = valid["records"][0].clone();
    over_capacity["records"] = json!(
        (1..=MAX_DECISIONS + 1)
            .map(|sequence| {
                let mut entry = template.clone();
                entry["record"]["sequence"] = json!(sequence);
                entry["record"]["decision"]["policy_version"] = json!(format!("policy-{sequence}"));
                entry
            })
            .collect::<Vec<_>>()
    );
    rejects_bytes(&jobs, serde_json::to_vec(&over_capacity).unwrap()).await;
    let valid = serde_json::to_string(&valid).unwrap();
    for corrupt in [
        "{".to_owned(),
        format!("{valid} null"),
        valid.replacen(
            "\"format_version\":1",
            "\"format_version\":1,\"format_version\":1",
            1,
        ),
        valid.replacen("\"sequence\":1", "\"sequence\":1,\"sequence\":1", 1),
        valid.replacen(
            "\"policy_version\":\"evidence-policy-v1\"",
            "\"policy_version\":\"evidence-policy-v1\",\"policy_version\":\"evidence-policy-v1\"",
            1,
        ),
        " ".repeat(MAX_STATE_BYTES + 1),
    ] {
        rejects_bytes(&jobs, corrupt.into_bytes()).await;
    }
}

#[tokio::test]
async fn damaged_decision_ledger_prevents_running_job_recovery_from_writing_other_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = memory(scope("session", "owner"), 1);
    admin
        .reserve(&source, 100, "extractor-v1", &options())
        .unwrap()
        .unwrap();
    let jobs = bytes(store.as_ref(), LEARNING_STATE_KEY).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&jobs).unwrap()["jobs"][0]["job"]["status"],
        "Running"
    );
    kernel.stop_all().await.unwrap();
    rejects_bytes(
        &jobs,
        br#"{"format_version":1,"records":[],"unknown":true}"#.to_vec(),
    )
    .await;
}

#[tokio::test]
async fn global_record_capacity_preserves_history_and_still_permits_idempotent_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let scopes = [scope("session-a", "alice"), scope("session-b", "bob")];
    for one in &scopes {
        let source = memory(one.clone(), 2);
        let (_, candidate) = complete(&admin, &source, 100);
        admin
            .record_decision(one, decision(&candidate, source.revision), 120)
            .unwrap();
    }
    let jobs = bytes(store.as_ref(), LEARNING_STATE_KEY);
    let mut ledger: Value =
        serde_json::from_slice(&bytes(store.as_ref(), DECISIONS_KEY).unwrap()).unwrap();
    let templates = ledger["records"].as_array().unwrap().clone();
    let count_per_scope = MAX_DECISIONS / 2;
    assert_eq!(count_per_scope * 2, MAX_DECISIONS);
    let mut full = vec![];
    for (index, template) in templates.iter().enumerate() {
        for sequence in 1..=count_per_scope {
            let mut record = template.clone();
            record["record"]["sequence"] = json!(sequence);
            record["record"]["at_ms"] = json!(120 + sequence);
            record["record"]["decision"]["policy_version"] =
                json!(format!("policy-{index}-{sequence}"));
            full.push(record);
        }
    }
    ledger["records"] = json!(full);
    let encoded = serde_json::to_vec(&ledger).unwrap();
    assert!(encoded.len() < MAX_STATE_BYTES);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);
    let store = RecordingStore::open(directory.path());
    store
        .set(&owner(), DECISIONS_KEY.into(), encoded.clone())
        .unwrap();
    let writes = store.writes();
    let (kernel, restored) = open(store.clone()).await;
    for one in &scopes {
        let history = restored.decisions(one).unwrap();
        assert_eq!(history.len(), count_per_scope);
        let last = history.last().unwrap();
        assert_eq!(
            restored
                .record_decision(one, last.decision.clone(), 9999)
                .unwrap(),
            *last
        );
        let mut new = last.decision.clone();
        new.policy_version = "one-more-policy".into();
        assert_eq!(
            restored.record_decision(one, new, 9999),
            Err(LearningError::LimitReached)
        );
        assert_eq!(restored.decisions(one).unwrap(), history);
    }
    assert_eq!(store.writes(), writes);
    assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), jobs);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), Some(encoded));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn byte_capacity_refuses_new_decisions_before_record_count_without_removing_prior_sources() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    // 合法标识中的引号会被 JSON 转义；字节容量必须计实际编码，而不只计记录数。
    let long_id = |suffix: &str| format!("{}{suffix}", "\"".repeat(256 - suffix.len()));
    let mut source = memory(
        MemoryScope {
            channel: long_id("channel"),
            session_id: long_id("session"),
            user_id: long_id("user"),
        },
        MAX_BATCH_EVIDENCE,
    );
    for (index, item) in source.evidence.iter_mut().enumerate() {
        item.id = long_id(&format!("evidence-{index}"));
    }
    let (_, candidate) = complete(&admin, &source, 100);
    let intended = LearningDecision {
        policy_version: long_id("policy-0"),
        action: LearningDecisionAction::Update {
            preference_id: long_id("preference"),
            expected_revision: 1,
        },
        reason: DecisionReason::ExplicitRevisionUpdate,
        ..decision(&candidate, source.revision)
    };
    admin
        .record_decision(&source.scope, intended.clone(), 120)
        .unwrap();
    let jobs = bytes(store.as_ref(), LEARNING_STATE_KEY);
    let mut ledger: Value =
        serde_json::from_slice(&bytes(store.as_ref(), DECISIONS_KEY).unwrap()).unwrap();
    let template = ledger["records"][0].clone();
    ledger["records"] = json!([]);
    let mut encoded_size = serde_json::to_vec(&ledger).unwrap().len();
    let mut full = vec![];
    for sequence in 1..=MAX_DECISIONS {
        let mut entry = template.clone();
        entry["record"]["sequence"] = json!(sequence);
        entry["record"]["at_ms"] = json!(120 + sequence);
        entry["record"]["decision"]["policy_version"] =
            json!(long_id(&format!("policy-{sequence}")));
        let addition = serde_json::to_vec(&entry).unwrap().len() + usize::from(!full.is_empty());
        if encoded_size + addition > MAX_STATE_BYTES {
            break;
        }
        encoded_size += addition;
        full.push(entry);
    }
    assert!(full.len() < MAX_DECISIONS);
    assert!(full.len() > 1);
    let count = full.len();
    ledger["records"] = json!(full);
    let encoded = serde_json::to_vec(&ledger).unwrap();
    assert_eq!(encoded.len(), encoded_size);
    assert!(encoded.len() <= MAX_STATE_BYTES);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(store);

    let store = RecordingStore::open(directory.path());
    store
        .set(&owner(), DECISIONS_KEY.into(), encoded.clone())
        .unwrap();
    let writes = store.writes();
    let (kernel, restored) = open(store.clone()).await;
    let history = restored.decisions(&source.scope).unwrap();
    assert_eq!(history.len(), count);
    let next = LearningDecision {
        policy_version: long_id(&format!("policy-{}", count + 1)),
        ..intended
    };
    assert_eq!(
        restored.record_decision(&source.scope, next, 120 + count as u64 + 1),
        Err(LearningError::LimitReached)
    );
    let last = history.last().unwrap();
    assert_eq!(
        restored
            .record_decision(&source.scope, last.decision.clone(), 9999)
            .unwrap(),
        *last
    );
    assert_eq!(restored.decisions(&source.scope).unwrap(), history);
    assert_eq!(bytes(store.as_ref(), LEARNING_STATE_KEY), jobs);
    assert_eq!(bytes(store.as_ref(), DECISIONS_KEY), Some(encoded));
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}
