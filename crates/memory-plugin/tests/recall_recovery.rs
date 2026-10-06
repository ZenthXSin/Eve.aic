mod support;

use eve_kernel::{Kernel, KernelServices};
use eve_memory_api::*;
use eve_memory_plugin::{LexicalMemoryRecall, MEMORY_STATE_KEY, MemoryController, MemoryPlugin};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_session_api::{SESSION_SERVICE_ID, SessionKey, SessionService, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::sync::Arc;
use support::{RecordingStore, complete_session};

fn owner() -> PluginId {
    PluginId::new(MEMORY_PLUGIN_ID).unwrap()
}

fn scope() -> MemoryScope {
    MemoryScope {
        channel: "recall-recovery".into(),
        session_id: "session".into(),
        user_id: "alice".into(),
    }
}

fn request() -> MemoryRecallRequest {
    MemoryRecallRequest {
        query: "anchor".into(),
        limit: 8,
    }
}

async fn open(state: Arc<dyn StateStore>) -> (Kernel, MemoryController, Arc<dyn SessionService>) {
    let backends = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start_all().await.unwrap();
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    (kernel, admin, sessions)
}

fn statement(operation: &str, id: &str, text: &str) -> PreferenceChange {
    PreferenceChange {
        operation_id: operation.into(),
        at_ms: 100,
        evidence: PreferenceEvidence::Statement(UserStatement {
            evidence_id: format!("evidence-{operation}"),
            message_id: format!("message-{operation}"),
            text: text.into(),
            at_ms: 100,
        }),
        action: PreferenceAction::Confirm {
            id: id.into(),
            text: text.into(),
        },
    }
}

fn recall(factory: &LexicalMemoryRecall) -> Arc<dyn MemoryRecallService> {
    factory.reader(scope()).unwrap()
}

#[tokio::test]
async fn real_file_restart_preserves_recall_content_provenance_order_and_zero_write_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let scoped = scope();
    let key = SessionKey::new(&scoped.session_id, &scoped.user_id).unwrap();
    let completed = complete_session(
        &*sessions,
        &key,
        "anchor user evidence",
        "anchor assistant reply",
    );
    let imported = admin
        .import_completed(
            &scoped,
            0,
            CompletedInteraction {
                evidence_id: "completed-evidence".into(),
                message_id: "completed-message".into(),
                at_ms: 20,
                snapshot: completed.clone(),
                turn_id: 1,
            },
        )
        .unwrap();
    let saved = admin
        .update_preference(
            &scoped,
            imported.revision,
            PreferenceChange {
                operation_id: "confirmed-operation".into(),
                at_ms: 30,
                evidence: PreferenceEvidence::Existing("completed-evidence".into()),
                action: PreferenceAction::Confirm {
                    id: "style".into(),
                    text: "anchor confirmed preference".into(),
                },
            },
        )
        .unwrap();
    let factory = LexicalMemoryRecall::new(Arc::new(admin.clone()));
    let old_reader = recall(&factory);
    let before_bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let before_disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let writes = store.writes();
    let expected = old_reader.recall(&request()).unwrap();
    expected.validate_for(&scoped, &request()).unwrap();
    assert_eq!(expected.revision, saved.revision);
    assert_eq!(expected.hits.len(), 3);
    let preference = expected
        .hits
        .iter()
        .find(|hit| matches!(hit.source, MemoryRecallSource::ConfirmedPreference { .. }))
        .unwrap();
    assert_eq!(preference.excerpt, "anchor confirmed preference");
    assert!(matches!(
        &preference.source,
        MemoryRecallSource::ConfirmedPreference {
            preference_id,
            preference_revision: 1,
            evidence_id,
            evidence_revision: 1,
            ..
        } if preference_id == "style" && evidence_id == "completed-evidence"
    ));
    for (field, text) in [
        (RecallField::User, "anchor user evidence"),
        (RecallField::Assistant, "anchor assistant reply"),
    ] {
        let hit = expected
            .hits
            .iter()
            .find(|hit| {
                matches!(
                    &hit.source,
                    MemoryRecallSource::CompletedInteraction { field: hit_field, .. }
                        if *hit_field == field
                )
            })
            .unwrap();
        assert_eq!(hit.excerpt, text);
        assert!(!hit.excerpt_truncated);
        assert!(matches!(
            &hit.source,
            MemoryRecallSource::CompletedInteraction {
                evidence_id,
                evidence_revision: 1,
                message_id,
                session_revision: 2,
                turn_id: 1,
                at_ms: 20,
                ..
            } if evidence_id == "completed-evidence" && message_id == "completed-message"
        ));
    }
    for _ in 0..3 {
        assert_eq!(old_reader.recall(&request()).unwrap(), expected);
    }
    assert_eq!(
        admin.reader(scoped.clone()).unwrap().snapshot().unwrap(),
        saved
    );
    assert_eq!(store.writes(), writes);
    assert_eq!(
        store.get(&owner(), MEMORY_STATE_KEY).unwrap(),
        Some(before_bytes)
    );
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before_disk
    );

    kernel.stop_all().await.unwrap();
    assert_eq!(old_reader.recall(&request()), Err(MemoryError::Unavailable));
    drop(kernel);
    drop(sessions);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let (kernel, restored, sessions) = open(store.clone()).await;
    let factory = LexicalMemoryRecall::new(Arc::new(restored.clone()));
    let reader = recall(&factory);
    for _ in 0..3 {
        assert_eq!(reader.recall(&request()).unwrap(), expected);
    }
    assert_eq!(restored.reader(scoped).unwrap().snapshot().unwrap(), saved);
    assert_eq!(sessions.snapshot(&key).unwrap(), Some(completed));
    assert_eq!(old_reader.recall(&request()), Err(MemoryError::Unavailable));
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before_disk
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrected_and_revoked_preference_history_never_reappears_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let scoped = scope();
    admin
        .update_preference(
            &scoped,
            0,
            statement("first", "style", "anchor obsolete text"),
        )
        .unwrap();
    admin
        .update_preference(
            &scoped,
            1,
            statement("second", "removed", "anchor withdrawn text"),
        )
        .unwrap();
    let factory = LexicalMemoryRecall::new(Arc::new(admin.clone()));
    let old_reader = recall(&factory);
    assert_eq!(old_reader.recall(&request()).unwrap().hits.len(), 2);
    let mut correction = statement("correct", "style", "anchor corrected text");
    correction.action = PreferenceAction::Correct {
        id: "style".into(),
        text: "anchor corrected text".into(),
    };
    admin.update_preference(&scoped, 2, correction).unwrap();
    let mut revocation = statement("revoke", "removed", "anchor revocation statement");
    revocation.action = PreferenceAction::Revoke {
        id: "removed".into(),
    };
    let saved = admin.update_preference(&scoped, 3, revocation).unwrap();
    let writes = store.writes();
    let expected = old_reader.recall(&request()).unwrap();
    assert_eq!(expected.revision, 4);
    assert_eq!(expected.hits.len(), 1);
    assert_eq!(expected.hits[0].excerpt, "anchor corrected text");
    assert!(matches!(
        &expected.hits[0].source,
        MemoryRecallSource::ConfirmedPreference {
            preference_id,
            preference_revision: 2,
            evidence_id,
            evidence_revision: 3,
            ..
        } if preference_id == "style" && evidence_id == "evidence-correct"
    ));
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(sessions);
    drop(store);

    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, restored, _) = open(store.clone()).await;
    let factory = LexicalMemoryRecall::new(Arc::new(restored.clone()));
    assert_eq!(recall(&factory).recall(&request()).unwrap(), expected);
    let recovered = restored.reader(scoped).unwrap().snapshot().unwrap();
    assert_eq!(recovered, saved);
    assert!(recovered.preferences.iter().any(|preference| {
        preference.id == "style" && preference.history[0].text == "anchor obsolete text"
    }));
    assert!(recovered.preferences.iter().any(|preference| {
        preference.id == "removed" && preference.status == PreferenceStatus::Revoked
    }));
    assert_eq!(old_reader.recall(&request()), Err(MemoryError::Unavailable));
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn same_kernel_restart_keeps_old_recall_handles_closed_and_binds_new_readers() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    admin
        .update_preference(
            &scope(),
            0,
            statement("first", "style", "anchor durable text"),
        )
        .unwrap();
    let factory = LexicalMemoryRecall::new(Arc::new(admin));
    let old_reader = recall(&factory);
    let expected = old_reader.recall(&request()).unwrap();
    let writes = store.writes();
    kernel.stop(&owner()).await.unwrap();
    assert_eq!(old_reader.recall(&request()), Err(MemoryError::Unavailable));
    assert!(matches!(
        factory.reader(scope()),
        Err(MemoryError::Unavailable)
    ));
    kernel.start(&owner()).await.unwrap();
    let reader = recall(&factory);
    assert_eq!(reader.recall(&request()).unwrap(), expected);
    assert_eq!(old_reader.recall(&request()), Err(MemoryError::Unavailable));
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
    assert_eq!(reader.recall(&request()), Err(MemoryError::Unavailable));
}

#[tokio::test]
async fn corrupted_persistence_cannot_be_bypassed_by_cached_recall_or_repaired_by_a_read() {
    for truncated_json in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        let (kernel, admin, _) = open(store.clone()).await;
        admin
            .update_preference(
                &scope(),
                0,
                statement("first", "style", "anchor durable text"),
            )
            .unwrap();
        let factory = LexicalMemoryRecall::new(Arc::new(admin));
        let old_reader = recall(&factory);
        assert_eq!(old_reader.recall(&request()).unwrap().hits.len(), 1);
        let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
        kernel.stop(&owner()).await.unwrap();
        let damaged = if truncated_json {
            b"{truncated-recall-state".to_vec()
        } else {
            let mut document: Value = serde_json::from_slice(&bytes).unwrap();
            document["scopes"][0]["snapshot"]["preferences"][0]["history"][0]["evidence_id"] =
                json!("missing-source");
            serde_json::to_vec(&document).unwrap()
        };
        store
            .set(&owner(), MEMORY_STATE_KEY.into(), damaged.clone())
            .unwrap();
        let disk = std::fs::read(directory.path().join("state.json")).unwrap();
        let writes = store.writes();
        assert!(kernel.start(&owner()).await.is_err());
        assert_eq!(old_reader.recall(&request()), Err(MemoryError::Unavailable));
        assert!(matches!(
            factory.reader(scope()),
            Err(MemoryError::Unavailable)
        ));
        assert_eq!(
            store.get(&owner(), MEMORY_STATE_KEY).unwrap(),
            Some(damaged)
        );
        assert_eq!(store.writes(), writes);
        assert_eq!(
            std::fs::read(directory.path().join("state.json")).unwrap(),
            disk
        );
        kernel.stop_all().await.unwrap();
    }
}

async fn failed_write_requires_reopening_recall(committed: bool) {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let scoped = scope();
    admin
        .update_preference(
            &scoped,
            0,
            statement("first", "style", "anchor original text"),
        )
        .unwrap();
    let factory = LexicalMemoryRecall::new(Arc::new(admin.clone()));
    let reader = recall(&factory);
    let other_reader = factory
        .reader(MemoryScope {
            user_id: "bob".into(),
            ..scoped.clone()
        })
        .unwrap();
    let before = reader.recall(&request()).unwrap();
    assert!(other_reader.recall(&request()).unwrap().hits.is_empty());
    let before_bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let change = statement("second", "another", "anchor newly committed text");
    if committed {
        store.fail_after_writes(true);
    } else {
        store.fail_writes(true);
    }
    assert_eq!(
        admin.update_preference(&scoped, 1, change.clone()),
        Err(MemoryError::Storage)
    );
    let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    if committed {
        assert_ne!(bytes, before_bytes);
    } else {
        assert_eq!(bytes, before_bytes);
    }
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let writes = store.writes();
    // Even a previously empty scope must fail instead of hiding a closed service as no results.
    assert_eq!(reader.recall(&request()), Err(MemoryError::Unavailable));
    assert_eq!(
        other_reader.recall(&request()),
        Err(MemoryError::Unavailable)
    );
    assert!(matches!(
        factory.reader(scoped.clone()),
        Err(MemoryError::Unavailable)
    ));
    assert_eq!(
        admin.update_preference(&scoped, 1, change.clone()),
        Err(MemoryError::Unavailable)
    );
    assert_eq!(store.writes(), writes);
    store.fail_writes(false);
    store.fail_after_writes(false);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(sessions);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let (kernel, restored, _) = open(store.clone()).await;
    let factory = LexicalMemoryRecall::new(Arc::new(restored.clone()));
    let recovered = recall(&factory).recall(&request()).unwrap();
    let saved = restored.reader(scoped.clone()).unwrap().snapshot().unwrap();
    if committed {
        assert_eq!(recovered.revision, 2);
        assert_eq!(recovered.hits.len(), 2);
        assert!(
            recovered
                .hits
                .iter()
                .any(|hit| hit.excerpt == "anchor newly committed text")
        );
        assert_eq!(
            restored.update_preference(&scoped, 1, change).unwrap(),
            saved
        );
    } else {
        assert_eq!(recovered, before);
    }
    assert_eq!(recovered.revision, saved.revision);
    assert_eq!(reader.recall(&request()), Err(MemoryError::Unavailable));
    assert_eq!(
        other_reader.recall(&request()),
        Err(MemoryError::Unavailable)
    );
    assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), Some(bytes));
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_file_commit_closes_all_recall_handles_and_reopens_only_confirmed_state() {
    failed_write_requires_reopening_recall(false).await;
}

#[tokio::test]
async fn unacknowledged_file_commit_closes_all_recall_handles_until_disk_recovery() {
    failed_write_requires_reopening_recall(true).await;
}
