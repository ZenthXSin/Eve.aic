use eve_action_api::*;
use eve_action_plugin::{ACTION_STATE_KEY, ActionController, ActionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginId, StateStore};
use std::sync::{Arc, Barrier};

fn plugin_id() -> PluginId {
    PluginId::new(ACTION_PLUGIN_ID).unwrap()
}

fn proposal(goal_revision: u64) -> DocumentActionProposal {
    DocumentActionProposal {
        schema_version: ACTION_SCHEMA_VERSION,
        action_id: derive_action_id("eve", "alice", "goal", goal_revision, "reflection").unwrap(),
        subject_id: "eve".into(),
        user_id: "alice".into(),
        goal_id: "goal".into(),
        goal_revision,
        reflection_goal_id: "reflection".into(),
        reflection_goal_revision: 3,
        observation_event_id: format!("file-observation:{}", "a".repeat(64)),
        observation_source_id: format!("file-source:{}", "b".repeat(64)),
        input_sha256: "c".repeat(64),
        input_byte_count: 12,
        artifact_source_id: format!("artifact-file:{}", "d".repeat(64)),
        artifact_sha256: "e".repeat(64),
        artifact_byte_count: 24,
        created_at_ms: 10,
        timeout_ms: 1000,
    }
}

fn receipt(proposal: &DocumentActionProposal) -> ArtifactReceipt {
    ArtifactReceipt {
        artifact_source_id: proposal.artifact_source_id.clone(),
        sha256: proposal.artifact_sha256.clone(),
        byte_count: proposal.artifact_byte_count,
        verified_at_ms: 20,
    }
}

async fn open(store: Arc<MemoryStateStore>) -> (Kernel, ActionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = ActionPlugin::new("eve").unwrap();
    let journal = plugin.controller();
    assert_eq!(journal.snapshot(), Err(ActionError::Unavailable));
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, journal)
}

#[tokio::test]
async fn begin_is_persisted_before_return_and_duplicate_cannot_change_target_or_budget() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone()).await;
    let request = proposal(1);
    let begun = journal.begin(request.clone()).unwrap();
    assert!(!begun.duplicate);
    assert_eq!(begun.record.status, ActionStatus::Executing);
    assert_eq!(begun.record.revision, 1);
    let bytes = store.get(&plugin_id(), ACTION_STATE_KEY).unwrap().unwrap();
    let saved = ActionSnapshot::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert_eq!(saved, journal.snapshot().unwrap());
    assert_eq!(saved.records, std::slice::from_ref(&begun.record));

    let mut later_delivery = request.clone();
    later_delivery.created_at_ms = 99;
    let replay = journal.begin(later_delivery).unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.record, begun.record);
    for changed in [
        DocumentActionProposal {
            artifact_source_id: format!("artifact-file:{}", "f".repeat(64)),
            ..request.clone()
        },
        DocumentActionProposal {
            artifact_sha256: "f".repeat(64),
            ..request.clone()
        },
        DocumentActionProposal {
            artifact_byte_count: request.artifact_byte_count + 1,
            ..request.clone()
        },
        DocumentActionProposal {
            observation_event_id: format!("file-observation:{}", "f".repeat(64)),
            ..request.clone()
        },
        DocumentActionProposal {
            observation_source_id: format!("file-source:{}", "f".repeat(64)),
            ..request.clone()
        },
        DocumentActionProposal {
            input_sha256: "f".repeat(64),
            ..request.clone()
        },
        DocumentActionProposal {
            input_byte_count: request.input_byte_count + 1,
            ..request.clone()
        },
        DocumentActionProposal {
            reflection_goal_revision: request.reflection_goal_revision + 1,
            ..request.clone()
        },
        DocumentActionProposal {
            timeout_ms: request.timeout_ms + 1,
            ..request.clone()
        },
    ] {
        assert_eq!(journal.begin(changed), Err(ActionError::Conflict));
    }
    assert_eq!(
        store.get(&plugin_id(), ACTION_STATE_KEY).unwrap(),
        Some(bytes)
    );
    assert_eq!(journal.snapshot().unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn finish_requires_current_record_revision_and_exact_independent_receipt() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone()).await;
    let request = proposal(1);
    journal.begin(request.clone()).unwrap();
    let before = journal.snapshot().unwrap();
    for wrong in [
        ArtifactReceipt {
            artifact_source_id: format!("artifact-file:{}", "f".repeat(64)),
            ..receipt(&request)
        },
        ArtifactReceipt {
            sha256: "f".repeat(64),
            ..receipt(&request)
        },
        ArtifactReceipt {
            byte_count: request.artifact_byte_count + 1,
            ..receipt(&request)
        },
    ] {
        assert_eq!(
            journal.finish(&request.action_id, 1, ActionOutcome::Completed(wrong)),
            Err(ActionError::InvalidInput)
        );
    }
    assert_eq!(
        journal.finish(
            &request.action_id,
            2,
            ActionOutcome::Completed(receipt(&request))
        ),
        Err(ActionError::StaleRevision)
    );
    assert_eq!(journal.snapshot().unwrap(), before);
    let done = journal
        .finish(
            &request.action_id,
            1,
            ActionOutcome::Completed(receipt(&request)),
        )
        .unwrap();
    assert_eq!(done.status, ActionStatus::Completed);
    assert_eq!(done.revision, 2);
    assert_eq!(done.finished_at_ms, Some(20));
    let bytes = store.get(&plugin_id(), ACTION_STATE_KEY).unwrap().unwrap();
    for outcome in [
        ActionOutcome::Completed(receipt(&request)),
        ActionOutcome::Blocked {
            failure: ActionFailure::WriteFailed,
            finished_at_ms: 30,
        },
    ] {
        assert_eq!(
            journal.finish(&request.action_id, 2, outcome),
            Err(ActionError::InvalidTransition)
        );
    }
    assert_eq!(journal.begin(request.clone()).unwrap().record, done);
    assert!(journal.begin(request.clone()).unwrap().duplicate);
    assert_eq!(
        store.get(&plugin_id(), ACTION_STATE_KEY).unwrap(),
        Some(bytes)
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(journal.snapshot(), Err(ActionError::Unavailable));
    assert_eq!(
        journal.begin(request.clone()),
        Err(ActionError::Unavailable)
    );
    assert_eq!(
        journal.finish(
            &request.action_id,
            2,
            ActionOutcome::Completed(receipt(&request))
        ),
        Err(ActionError::Unavailable)
    );
}

#[tokio::test]
async fn blocked_attempt_is_terminal_and_subject_is_bound_to_plugin() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store).await;
    let request = proposal(1);
    let mut foreign = request.clone();
    foreign.subject_id = "another-subject".into();
    foreign.action_id = derive_action_id(
        &foreign.subject_id,
        &foreign.user_id,
        &foreign.goal_id,
        foreign.goal_revision,
        &foreign.reflection_goal_id,
    )
    .unwrap();
    assert_eq!(journal.begin(foreign), Err(ActionError::SubjectMismatch));
    assert_eq!(journal.snapshot().unwrap().revision, 0);
    journal.begin(request.clone()).unwrap();
    let blocked = journal
        .finish(
            &request.action_id,
            1,
            ActionOutcome::Blocked {
                failure: ActionFailure::PreconditionChanged,
                finished_at_ms: 20,
            },
        )
        .unwrap();
    assert_eq!(blocked.status, ActionStatus::Blocked);
    assert_eq!(journal.begin(request.clone()).unwrap().record, blocked);
    assert!(journal.begin(request.clone()).unwrap().duplicate);
    assert_eq!(
        journal.finish(
            &request.action_id,
            2,
            ActionOutcome::Completed(receipt(&request)),
        ),
        Err(ActionError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn concurrent_begin_reserves_one_attempt_and_finish_commits_once() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store).await;
    let gate = Arc::new(Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let journal = journal.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                journal.begin(proposal(1)).unwrap()
            })
        })
        .collect();
    gate.wait();
    let results: Vec<_> = threads.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|report| !report.duplicate).count(), 1);
    assert_eq!(journal.snapshot().unwrap().revision, 1);
    let gate = Arc::new(Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let journal = journal.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                let request = proposal(1);
                gate.wait();
                journal.finish(
                    &request.action_id,
                    1,
                    ActionOutcome::Completed(receipt(&request)),
                )
            })
        })
        .collect();
    gate.wait();
    let results: Vec<_> = threads.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(ActionError::StaleRevision))
            .count(),
        1
    );
    assert_eq!(journal.snapshot().unwrap().revision, 2);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_journal_refuses_new_attempt_without_pruning_idempotency_records() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone()).await;
    for revision in 1..=MAX_ACTION_RECORDS as u64 {
        journal.begin(proposal(revision)).unwrap();
    }
    let saved = journal.snapshot().unwrap();
    let bytes = store.get(&plugin_id(), ACTION_STATE_KEY).unwrap();
    assert_eq!(
        journal.begin(proposal(MAX_ACTION_RECORDS as u64 + 1)),
        Err(ActionError::LimitReached)
    );
    assert!(journal.begin(proposal(1)).unwrap().duplicate);
    assert_eq!(journal.snapshot().unwrap(), saved);
    assert_eq!(store.get(&plugin_id(), ACTION_STATE_KEY).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
}
