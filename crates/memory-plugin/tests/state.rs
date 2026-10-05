mod support;

use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatMessage, ChatRole, ToolCall, ToolResult};
use eve_memory_api::*;
use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryController, MemoryPlugin};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_session_api::{
    SESSION_SERVICE_ID, SessionFailure, SessionFailureCode, SessionInput, SessionKey,
    SessionService, SessionServiceHandle, SessionSnapshot, SessionTurnStatus,
};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::sync::{Arc, Barrier};
use support::{RecordingStore, complete_session};

fn owner() -> PluginId {
    PluginId::new(MEMORY_PLUGIN_ID).unwrap()
}

fn scope(session: &str, user: &str) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: session.into(),
        user_id: user.into(),
    }
}

fn key(scope: &MemoryScope) -> SessionKey {
    SessionKey::new(&scope.session_id, &scope.user_id).unwrap()
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
    assert!(matches!(
        admin.reader(scope("s", "u")),
        Err(MemoryError::Unavailable)
    ));
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

fn interaction(id: &str, snapshot: SessionSnapshot, turn_id: u64) -> CompletedInteraction {
    CompletedInteraction {
        evidence_id: id.into(),
        message_id: format!("message-{id}"),
        at_ms: 100,
        snapshot,
        turn_id,
    }
}

fn confirm(operation: &str, evidence: &str, id: &str, text: &str) -> PreferenceChange {
    PreferenceChange {
        operation_id: operation.into(),
        at_ms: 200,
        evidence: PreferenceEvidence::Existing(evidence.into()),
        action: PreferenceAction::Confirm {
            id: id.into(),
            text: text.into(),
        },
    }
}

fn statement(operation: &str, evidence: &str, id: &str, text: &str) -> PreferenceChange {
    PreferenceChange {
        evidence: PreferenceEvidence::Statement(UserStatement {
            evidence_id: evidence.into(),
            message_id: format!("message-{evidence}"),
            text: text.into(),
            at_ms: 100,
        }),
        ..confirm(operation, evidence, id, text)
    }
}

#[tokio::test]
async fn completed_evidence_preserves_exact_user_and_final_reply_with_zero_write_replay() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let scope = scope("group-a", "alice");
    let input = " 请先说结论。\n再给必要细节，保留原文空白。 ";
    let reply = "结论：可以。\n下一步请确认。";
    let turn = sessions
        .begin(SessionInput {
            key: key(&scope),
            text: input.into(),
        })
        .unwrap();
    sessions
        .complete(
            &turn.lease,
            vec![
                ChatMessage::text(ChatRole::User, input),
                ChatMessage::assistant_tool_calls(vec![ToolCall {
                    id: "tool-1".into(),
                    name: "echo".into(),
                    arguments: json!({"text":"tool-private-argument"}),
                }])
                .unwrap(),
                ChatMessage::tool_results(vec![
                    ToolResult::success("tool-1", json!("tool-private-result")).unwrap(),
                ])
                .unwrap(),
                ChatMessage::text(ChatRole::Assistant, reply),
            ],
        )
        .unwrap();
    let completed = sessions.snapshot(&key(&scope)).unwrap().unwrap();
    let evidence = interaction("e-1", completed, turn.lease.turn_id);
    let saved = admin.import_completed(&scope, 0, evidence.clone()).unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.evidence.len(), 1);
    assert!(saved.preferences.is_empty());
    assert_eq!(saved.evidence[0].revision, 1);
    assert_eq!(saved.evidence[0].at_ms, 100);
    assert_eq!(
        saved.evidence[0].source,
        EvidenceSource::CompletedInteraction {
            message_id: "message-e-1".into(),
            session_revision: 2,
            turn_id: 1,
            user_text: input.into(),
            assistant_text: reply.into(),
        }
    );
    let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let memory_json = String::from_utf8(bytes.clone()).unwrap();
    assert!(!memory_json.contains("tool-private-argument"));
    assert!(!memory_json.contains("tool-private-result"));
    let writes = store.writes();
    let mut replay = evidence.clone();
    replay.at_ms = 999;
    assert_eq!(admin.import_completed(&scope, 0, replay).unwrap(), saved);
    assert_eq!(store.writes(), writes);
    let mut conflict = evidence.clone();
    conflict.message_id = "another-source-message".into();
    assert_eq!(
        admin.import_completed(&scope, 1, conflict),
        Err(MemoryError::Conflict)
    );
    let mut conflict = evidence;
    conflict.snapshot.turns[0].input = "被改写原文".into();
    let SessionTurnStatus::Completed { messages } = &mut conflict.snapshot.turns[0].status else {
        unreachable!()
    };
    messages[0] = ChatMessage::text(ChatRole::User, "被改写原文");
    assert_eq!(
        admin.import_completed(&scope, 1, conflict),
        Err(MemoryError::Conflict)
    );
    assert_eq!(
        admin.reader(scope.clone()).unwrap().snapshot().unwrap(),
        saved
    );
    assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), Some(bytes));
    assert_eq!(store.writes(), writes);
    let debug = format!("{:?} {:?}", saved, saved.evidence[0].source);
    for private in [input, reply, "alice", "group-a"] {
        assert!(!debug.contains(private));
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn non_completed_or_mismatched_session_inputs_never_become_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("session", "alice");
    let pending = sessions
        .begin(SessionInput {
            key: key(&one),
            text: "尚未完成".into(),
        })
        .unwrap();
    let pending_snapshot = sessions.snapshot(&key(&one)).unwrap().unwrap();
    assert_eq!(
        admin.import_completed(&one, 0, interaction("pending", pending_snapshot.clone(), 1)),
        Err(MemoryError::InvalidInput)
    );
    sessions
        .fail(
            &pending.lease,
            SessionFailure {
                code: SessionFailureCode::Cancelled,
                started_tools: Some(0),
            },
        )
        .unwrap();
    let failed = sessions.snapshot(&key(&one)).unwrap().unwrap();
    assert_eq!(
        admin.import_completed(&one, 0, interaction("failed", failed.clone(), 1)),
        Err(MemoryError::InvalidInput)
    );
    let mut interrupted = failed;
    interrupted.turns[0].status = SessionTurnStatus::Interrupted;
    assert_eq!(
        admin.import_completed(&one, 0, interaction("interrupted", interrupted, 1)),
        Err(MemoryError::InvalidInput)
    );
    let complete = complete_session(&*sessions, &key(&one), "完成原文", "完成回复");
    for wrong in [scope("different-session", "alice"), scope("session", "bob")] {
        assert_eq!(
            admin.import_completed(&wrong, 0, interaction("wrong-owner", complete.clone(), 2)),
            Err(MemoryError::InvalidInput)
        );
    }
    let mut corrupt = complete.clone();
    corrupt.revision += 1;
    assert_eq!(
        admin.import_completed(&one, 0, interaction("bad-revision", corrupt, 2)),
        Err(MemoryError::InvalidInput)
    );
    assert_eq!(
        admin.import_completed(&one, 0, interaction("missing-turn", complete, 3)),
        Err(MemoryError::InvalidInput)
    );
    assert!(store.get(&owner(), MEMORY_STATE_KEY).unwrap().is_none());
    let snapshot = admin.reader(one).unwrap().snapshot().unwrap();
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.evidence.is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn bound_readers_and_evidence_references_isolate_channel_session_and_user() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let alice = scope("group-a", "alice");
    let saved = admin
        .update_preference(
            &alice,
            0,
            statement("op-alice", "alice-evidence", "tone", "请称呼我 Alice"),
        )
        .unwrap();
    let reader = admin.reader(alice.clone()).unwrap();
    for other in [
        scope("group-b", "alice"),
        scope("group-a", "bob"),
        MemoryScope {
            channel: "terminal".into(),
            ..alice.clone()
        },
    ] {
        let other_reader = admin.reader(other.clone()).unwrap();
        let empty = other_reader.snapshot().unwrap();
        assert_eq!(empty.revision, 0);
        assert!(empty.evidence.is_empty() && empty.preferences.is_empty());
        assert_eq!(
            admin.update_preference(
                &other,
                0,
                confirm("foreign-ref", "alice-evidence", "leak", "禁止跨范围引用")
            ),
            Err(MemoryError::NotFound)
        );
        let independent = admin
            .update_preference(
                &other,
                0,
                statement("op-alice", "own-evidence", "tone", "自己的偏好"),
            )
            .unwrap();
        assert_eq!(independent.revision, 1);
        assert_eq!(independent.evidence.len(), 1);
        assert_eq!(reader.snapshot().unwrap(), saved);
    }
    kernel.stop_all().await.unwrap();
    assert_eq!(reader.snapshot(), Err(MemoryError::Unavailable));
    assert!(matches!(admin.reader(alice), Err(MemoryError::Unavailable)));
}

#[tokio::test]
async fn explicit_preferences_keep_evidence_and_history_through_cas_correction_and_revoke() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let scope = scope("s", "u");
    let completed = complete_session(&*sessions, &key(&scope), "先结论，后细节。", "记住了。");
    let imported = admin
        .import_completed(&scope, 0, interaction("original", completed, 1))
        .unwrap();
    assert_eq!(
        admin.update_preference(&scope, 1, confirm("missing", "absent", "style", "没有来源")),
        Err(MemoryError::NotFound)
    );
    let operation = confirm("confirm", "original", "style", "先给结论");
    let confirmed = admin
        .update_preference(&scope, 1, operation.clone())
        .unwrap();
    assert_eq!(confirmed.revision, 2);
    assert_eq!(confirmed.evidence, imported.evidence);
    assert_eq!(confirmed.preferences[0].history.len(), 1);
    assert_eq!(confirmed.preferences[0].history[0].evidence_id, "original");
    let writes = store.writes();
    let mut replay = operation.clone();
    replay.at_ms = 999;
    assert_eq!(
        admin.update_preference(&scope, 0, replay).unwrap(),
        confirmed
    );
    assert_eq!(store.writes(), writes);
    let mut conflict = operation;
    conflict.action = PreferenceAction::Confirm {
        id: "style".into(),
        text: "冲突改写".into(),
    };
    assert_eq!(
        admin.update_preference(&scope, 2, conflict),
        Err(MemoryError::Conflict)
    );
    let mut correction = statement("correction", "correction-evidence", "style", "请简短一点");
    correction.action = PreferenceAction::Correct {
        id: "style".into(),
        text: "简短并先给结论".into(),
    };
    assert_eq!(
        admin.update_preference(&scope, 1, correction.clone()),
        Err(MemoryError::StaleRevision)
    );
    assert_eq!(
        admin.reader(scope.clone()).unwrap().snapshot().unwrap(),
        confirmed
    );
    let corrected = admin.update_preference(&scope, 2, correction).unwrap();
    assert_eq!(corrected.revision, 3);
    assert_eq!(corrected.evidence.len(), 2);
    assert_eq!(corrected.preferences[0].revision, 2);
    assert_eq!(
        corrected.preferences[0].history[0],
        confirmed.preferences[0].history[0]
    );
    assert_eq!(
        corrected.preferences[0].history[1].evidence_id,
        "correction-evidence"
    );
    let mut revoke = statement("revoke", "revoke-evidence", "style", "撤销刚才的偏好");
    revoke.action = PreferenceAction::Revoke { id: "style".into() };
    let revoked = admin.update_preference(&scope, 3, revoke.clone()).unwrap();
    assert_eq!(revoked.revision, 4);
    assert_eq!(revoked.evidence.len(), 3);
    assert_eq!(revoked.preferences[0].status, PreferenceStatus::Revoked);
    assert_eq!(revoked.preferences[0].revision, 3);
    assert_eq!(revoked.preferences[0].text, corrected.preferences[0].text);
    assert_eq!(
        &revoked.preferences[0].history[..2],
        corrected.preferences[0].history.as_slice()
    );
    assert_eq!(
        revoked.preferences[0].history[2].status,
        PreferenceStatus::Revoked
    );
    assert_eq!(revoked.evidence[0], imported.evidence[0]);
    let writes = store.writes();
    revoke.at_ms = 9999;
    if let PreferenceEvidence::Statement(value) = &mut revoke.evidence {
        value.at_ms = 9999;
    }
    assert_eq!(admin.update_preference(&scope, 0, revoke).unwrap(), revoked);
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn concurrent_preference_cas_commits_once_without_orphan_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let one = scope("s", "u");
    let first = admin
        .update_preference(
            &one,
            0,
            statement("original", "original", "style", "原偏好"),
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let jobs: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            let admin = admin.clone();
            let scope = one.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut change = statement(name, name, "style", name);
                change.action = PreferenceAction::Correct {
                    id: "style".into(),
                    text: name.into(),
                };
                barrier.wait();
                admin.update_preference(&scope, 1, change)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(MemoryError::StaleRevision))
            .count(),
        1
    );
    let saved = admin.reader(one).unwrap().snapshot().unwrap();
    assert_eq!(saved.revision, 2);
    assert_eq!(saved.evidence.len(), 2);
    assert_eq!(saved.preferences[0].history.len(), 2);
    assert_eq!(saved.evidence[0], first.evidence[0]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn real_file_restart_is_read_only_and_old_handles_stay_closed() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let completed = complete_session(&*sessions, &key(&one), "可恢复原文", "已保存回复");
    let source = interaction("completed", completed, 1);
    admin.import_completed(&one, 0, source.clone()).unwrap();
    let operation = confirm("confirm", "completed", "style", "简短一点");
    let saved = admin.update_preference(&one, 1, operation.clone()).unwrap();
    let old_reader = admin.reader(one.clone()).unwrap();
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(sessions);
    drop(store);
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, restored, _) = open(store.clone()).await;
    assert_eq!(
        restored.reader(one.clone()).unwrap().snapshot().unwrap(),
        saved
    );
    assert_eq!(restored.import_completed(&one, 0, source).unwrap(), saved);
    assert_eq!(
        restored.update_preference(&one, 0, operation).unwrap(),
        saved
    );
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    assert_eq!(old_reader.snapshot(), Err(MemoryError::Unavailable));
    assert!(matches!(admin.reader(one), Err(MemoryError::Unavailable)));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_file_commit_closes_instance_and_reopen_keeps_only_confirmed_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let saved = admin
        .update_preference(&one, 0, statement("first", "first", "style", "原偏好"))
        .unwrap();
    let old_reader = admin.reader(one.clone()).unwrap();
    let memory_bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    store.fail_writes(true);
    let error = admin
        .update_preference(
            &one,
            1,
            statement("second", "second", "another", "未保存新偏好"),
        )
        .unwrap_err();
    assert_eq!(error, MemoryError::Storage);
    assert!(!format!("{error:?} {error}").contains("test-storage-private-detail"));
    assert_eq!(
        store.get(&owner(), MEMORY_STATE_KEY).unwrap(),
        Some(memory_bytes)
    );
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    assert_eq!(old_reader.snapshot(), Err(MemoryError::Unavailable));
    assert!(matches!(
        admin.reader(one.clone()),
        Err(MemoryError::Unavailable)
    ));
    assert_eq!(
        admin.update_preference(
            &one,
            1,
            statement("retry", "retry", "another", "不能直接重试")
        ),
        Err(MemoryError::Unavailable)
    );
    store.fail_writes(false);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(sessions);
    drop(store);
    let store = RecordingStore::open(directory.path());
    let (kernel, restored, _) = open(store.clone()).await;
    assert_eq!(restored.reader(one).unwrap().snapshot().unwrap(), saved);
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn committed_but_unacknowledged_write_closes_all_handles_until_disk_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let old = admin
        .update_preference(&one, 0, statement("first", "first", "style", "原偏好"))
        .unwrap();
    let reader = admin.reader(one.clone()).unwrap();
    let other_reader = admin.reader(scope("other", "other-user")).unwrap();
    let before = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let operation = statement("second", "second", "new-preference", "已经落盘但确认丢失");
    store.fail_after_writes(true);
    assert_eq!(
        admin.update_preference(&one, 1, operation.clone()),
        Err(MemoryError::Storage)
    );
    let committed = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    assert_ne!(committed, before);
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    assert_eq!(reader.snapshot(), Err(MemoryError::Unavailable));
    assert_eq!(other_reader.snapshot(), Err(MemoryError::Unavailable));
    assert!(matches!(
        admin.reader(one.clone()),
        Err(MemoryError::Unavailable)
    ));
    store.fail_after_writes(false);
    let writes = store.writes();
    assert_eq!(
        admin.update_preference(&one, 1, operation.clone()),
        Err(MemoryError::Unavailable)
    );
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(sessions);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let (kernel, restored, _) = open(store.clone()).await;
    let recovered = restored.reader(one.clone()).unwrap().snapshot().unwrap();
    assert_eq!(recovered.revision, 2);
    assert_eq!(recovered.evidence.len(), 2);
    assert_eq!(recovered.preferences.len(), 2);
    assert_eq!(recovered.evidence[0], old.evidence[0]);
    assert!(
        recovered
            .preferences
            .iter()
            .any(|value| value.id == "new-preference" && value.text == "已经落盘但确认丢失")
    );
    assert_eq!(
        restored.update_preference(&one, 1, operation).unwrap(),
        recovered
    );
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    assert_eq!(reader.snapshot(), Err(MemoryError::Unavailable));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn completed_replay_keeps_original_provenance_when_later_turns_exist() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let first = complete_session(&*sessions, &key(&one), "第一轮原文", "第一轮回复");
    let first_source = interaction("first", first, 1);
    let saved = admin
        .import_completed(&one, 0, first_source.clone())
        .unwrap();
    let later = complete_session(&*sessions, &key(&one), "第二轮原文", "第二轮回复");
    let writes = store.writes();
    assert_eq!(
        admin
            .import_completed(&one, 0, interaction("first", later, 1))
            .unwrap(),
        saved
    );
    assert_eq!(store.writes(), writes);
    let mut alias = first_source;
    alias.evidence_id = "new-id-same-origin".into();
    assert_eq!(
        admin.import_completed(&one, 1, alias),
        Err(MemoryError::Conflict)
    );
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_preferences_never_leave_orphan_statements_or_replace_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let one = scope("s", "u");
    let saved = admin
        .update_preference(&one, 0, statement("first", "first", "style", "已确认偏好"))
        .unwrap();
    let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let writes = store.writes();
    let mut correction = statement("missing", "missing", "absent", "不能凭空修订");
    correction.action = PreferenceAction::Correct {
        id: "absent".into(),
        text: "不存在".into(),
    };
    assert_eq!(
        admin.update_preference(&one, 1, correction),
        Err(MemoryError::NotFound)
    );
    let duplicate = statement("duplicate", "duplicate", "style", "不能覆盖已存在偏好");
    assert_eq!(
        admin.update_preference(&one, 1, duplicate),
        Err(MemoryError::Conflict)
    );
    let mut conflicting_statement =
        statement("different-operation", "first", "new", "改写已有原文");
    conflicting_statement.action = PreferenceAction::Correct {
        id: "style".into(),
        text: "修改偏好".into(),
    };
    assert_eq!(
        admin.update_preference(&one, 1, conflicting_statement),
        Err(MemoryError::Conflict)
    );
    assert_eq!(
        admin.reader(one.clone()).unwrap().snapshot().unwrap(),
        saved
    );
    assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), Some(bytes));
    assert_eq!(store.writes(), writes);
    let mut revoke = statement("revoke", "revoke", "style", "撤销偏好");
    revoke.action = PreferenceAction::Revoke { id: "style".into() };
    let revoked = admin.update_preference(&one, 1, revoke).unwrap();
    let writes = store.writes();
    let mut revive = statement("revive", "revive", "style", "不能隐式恢复");
    revive.action = PreferenceAction::Correct {
        id: "style".into(),
        text: "新文本".into(),
    };
    assert_eq!(
        admin.update_preference(&one, 2, revive),
        Err(MemoryError::Conflict)
    );
    assert_eq!(admin.reader(one).unwrap().snapshot().unwrap(), revoked);
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

async fn rejects_persisted_bytes_without_writing(bytes: Vec<u8>) {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    store
        .set(&owner(), MEMORY_STATE_KEY.into(), bytes.clone())
        .unwrap();
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let writes = store.writes();
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert!(matches!(
        admin.reader(scope("s", "u")),
        Err(MemoryError::Unavailable)
    ));
    assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), Some(bytes));
    assert_eq!(store.writes(), writes);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
}

#[tokio::test]
async fn corrupt_unknown_and_inconsistent_persistent_states_are_not_repaired_or_cleared() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let one = scope("s", "u");
    admin
        .update_preference(
            &one,
            0,
            statement("first", "first", "style", "保留原始证据"),
        )
        .unwrap();
    let valid: Value =
        serde_json::from_slice(&store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap()).unwrap();
    kernel.stop_all().await.unwrap();
    let cases: [fn(&mut Value); 9] = [
        |value| value["format_version"] = json!(MEMORY_FORMAT_VERSION + 1),
        |value| value["unknown_field"] = json!("拒绝未来字段"),
        |value| value["scopes"][0]["snapshot"]["revision"] = json!(9),
        |value| value["scopes"][0]["snapshot"]["evidence"][0]["source"]["text"] = json!("篡改原文"),
        |value| {
            value["scopes"][0]["snapshot"]["preferences"][0]["history"][0]["evidence_id"] =
                json!("absent")
        },
        |value| {
            value["scopes"][0]["snapshot"]["preferences"][0]["text"] = json!("和最新历史不一致")
        },
        |value| value["scopes"][0]["operations"][0]["revision"] = json!(2),
        |value| {
            let duplicate = value["scopes"][0].clone();
            value["scopes"].as_array_mut().unwrap().push(duplicate);
        },
        |value| {
            let duplicate = value["scopes"][0]["snapshot"]["evidence"][0].clone();
            value["scopes"][0]["snapshot"]["evidence"]
                .as_array_mut()
                .unwrap()
                .push(duplicate);
        },
    ];
    for mutate in cases {
        let mut damaged = valid.clone();
        mutate(&mut damaged);
        rejects_persisted_bytes_without_writing(serde_json::to_vec(&damaged).unwrap()).await;
    }
    for bytes in [
        b"{truncated".to_vec(),
        b"null".to_vec(),
        vec![b' '; MAX_STATE_BYTES + 1],
    ] {
        rejects_persisted_bytes_without_writing(bytes).await;
    }
}

#[tokio::test]
async fn evidence_capacity_is_global_and_full_replay_is_still_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let mut first_source = None;
    for index in 0..MAX_EVIDENCE {
        let completed = complete_session(&*sessions, &key(&one), &format!("原文 {index}"), "回复");
        let source = interaction(&format!("e-{index}"), completed, index as u64 + 1);
        if index == 0 {
            first_source = Some(source.clone());
        }
        let saved = admin.import_completed(&one, index as u64, source).unwrap();
        assert_eq!(saved.evidence.len(), index + 1);
    }
    let saved = admin.reader(one.clone()).unwrap().snapshot().unwrap();
    let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let writes = store.writes();
    assert_eq!(
        admin
            .import_completed(&one, 0, first_source.unwrap())
            .unwrap(),
        saved
    );
    let another = scope("other-session", "other-user");
    assert_eq!(
        admin.update_preference(
            &another,
            0,
            statement("overflow", "overflow", "style", "不能挤掉其他用户证据")
        ),
        Err(MemoryError::LimitReached)
    );
    assert!(
        admin
            .reader(another)
            .unwrap()
            .snapshot()
            .unwrap()
            .evidence
            .is_empty()
    );
    assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), Some(bytes));
    assert_eq!(store.writes(), writes);
    let confirmed = admin
        .update_preference(
            &one,
            saved.revision,
            confirm("uses-existing", "e-0", "style", "证据已满仍可引用已有证据"),
        )
        .unwrap();
    assert_eq!(confirmed.evidence, saved.evidence);
    assert_eq!(confirmed.preferences.len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn history_capacity_never_discards_old_versions_or_half_commits_new_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let one = scope("s", "u");
    let original = admin
        .update_preference(&one, 0, statement("first", "first", "style", "原始偏好"))
        .unwrap();
    let mut last_operation = None;
    for index in 1..MAX_HISTORY {
        let mut change = confirm(&format!("correction-{index}"), "first", "style", "占位");
        change.action = PreferenceAction::Correct {
            id: "style".into(),
            text: format!("修订 {index}"),
        };
        last_operation = Some(change.clone());
        let saved = admin.update_preference(&one, index as u64, change).unwrap();
        assert_eq!(saved.preferences[0].history.len(), index + 1);
    }
    let saved = admin.reader(one.clone()).unwrap().snapshot().unwrap();
    assert_eq!(saved.evidence, original.evidence);
    assert_eq!(
        saved.preferences[0].history[0],
        original.preferences[0].history[0]
    );
    let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap().unwrap();
    let writes = store.writes();
    assert_eq!(
        admin
            .update_preference(&one, 0, last_operation.unwrap())
            .unwrap(),
        saved
    );
    let mut revoke = statement(
        "overflow-revoke",
        "overflow",
        "style",
        "容量满时也不能丢失旧证据",
    );
    revoke.action = PreferenceAction::Revoke { id: "style".into() };
    assert_eq!(
        admin.update_preference(&one, saved.revision, revoke),
        Err(MemoryError::LimitReached)
    );
    assert_eq!(
        admin.update_preference(
            &scope("other", "user"),
            0,
            statement("other", "other", "other", "全局历史已满")
        ),
        Err(MemoryError::LimitReached)
    );
    assert_eq!(admin.reader(one).unwrap().snapshot().unwrap(), saved);
    assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), Some(bytes));
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn preference_capacity_preserves_all_ids_without_eviction() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let one = scope("s", "u");
    admin
        .update_preference(
            &one,
            0,
            statement("first", "first", "preference-0", "第一条偏好"),
        )
        .unwrap();
    for index in 1..MAX_PREFERENCES {
        let id = format!("preference-{index}");
        admin
            .update_preference(&one, index as u64, confirm(&id, "first", &id, "明确偏好"))
            .unwrap();
    }
    let saved = admin.reader(one.clone()).unwrap().snapshot().unwrap();
    assert_eq!(saved.preferences.len(), MAX_PREFERENCES);
    let writes = store.writes();
    assert_eq!(
        admin.update_preference(
            &one,
            saved.revision,
            confirm("overflow", "first", "overflow", "超过偏好数量")
        ),
        Err(MemoryError::LimitReached)
    );
    assert_eq!(admin.reader(one).unwrap().snapshot().unwrap(), saved);
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn utf8_text_byte_limits_reject_without_writes_and_accept_exact_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let exact_preference = format!("{}x", "好".repeat(MAX_PREFERENCE_BYTES / 3));
    assert_eq!(exact_preference.len(), MAX_PREFERENCE_BYTES);
    let saved = admin
        .update_preference(
            &one,
            0,
            statement("exact", "exact", "style", &exact_preference),
        )
        .unwrap();
    let writes = store.writes();
    for text in [
        format!("{exact_preference}x"),
        " \n ".into(),
        "不能\0含空字节".into(),
    ] {
        assert_eq!(
            admin.update_preference(&one, 1, statement("invalid", "invalid", "new", &text)),
            Err(MemoryError::InvalidInput)
        );
    }
    assert_eq!(store.writes(), writes);
    let exact_text = format!("{}xx", "原".repeat(MAX_TEXT_BYTES / 3));
    assert_eq!(exact_text.len(), MAX_TEXT_BYTES);
    let completed = complete_session(&*sessions, &key(&one), &exact_text, &exact_text);
    let saved = admin
        .import_completed(
            &one,
            saved.revision,
            interaction("exact-interaction", completed, 1),
        )
        .unwrap();
    let too_long = format!("{exact_text}x");
    let completed = complete_session(&*sessions, &key(&one), &too_long, "合法回复");
    let writes = store.writes();
    assert_eq!(
        admin.import_completed(&one, saved.revision, interaction("too-long", completed, 2)),
        Err(MemoryError::InvalidInput)
    );
    assert_eq!(store.writes(), writes);
    assert_eq!(admin.reader(one).unwrap().snapshot().unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn new_ids_cannot_duplicate_completed_turns_or_source_messages() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let completed = complete_session(&*sessions, &key(&one), "唯一原始交互", "唯一回复");
    let original = interaction("original", completed, 1);
    let saved = admin.import_completed(&one, 0, original.clone()).unwrap();
    let writes = store.writes();
    let mut duplicate_turn = original.clone();
    duplicate_turn.evidence_id = "other-evidence".into();
    duplicate_turn.message_id = "other-message".into();
    assert_eq!(
        admin.import_completed(&one, 1, duplicate_turn),
        Err(MemoryError::Conflict)
    );
    let mut duplicate_message = statement("other-op", "other-evidence", "style", "明确偏好");
    if let PreferenceEvidence::Statement(value) = &mut duplicate_message.evidence {
        value.message_id = original.message_id.clone();
    }
    assert_eq!(
        admin.update_preference(&one, 1, duplicate_message),
        Err(MemoryError::Conflict)
    );
    assert_eq!(store.writes(), writes);
    let later = complete_session(&*sessions, &key(&one), "新的交互", "新的回复");
    let mut duplicate_message = interaction("later", later, 2);
    duplicate_message.message_id = original.message_id;
    let writes = store.writes();
    assert_eq!(
        admin.import_completed(&one, 1, duplicate_message),
        Err(MemoryError::Conflict)
    );
    assert_eq!(admin.reader(one).unwrap().snapshot().unwrap(), saved);
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn same_kernel_restart_does_not_reactivate_old_readers() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, _) = open(store.clone()).await;
    let one = scope("s", "u");
    let saved = admin
        .update_preference(&one, 0, statement("first", "first", "style", "偏好原文"))
        .unwrap();
    let old_reader = admin.reader(one.clone()).unwrap();
    let writes = store.writes();
    kernel.stop(&owner()).await.unwrap();
    assert_eq!(old_reader.snapshot(), Err(MemoryError::Unavailable));
    assert!(matches!(
        admin.reader(one.clone()),
        Err(MemoryError::Unavailable)
    ));
    kernel.start(&owner()).await.unwrap();
    let new_reader = admin.reader(one).unwrap();
    assert_eq!(new_reader.snapshot().unwrap(), saved);
    assert_eq!(old_reader.snapshot(), Err(MemoryError::Unavailable));
    assert_eq!(store.writes(), writes);
    kernel.stop_all().await.unwrap();
    assert_eq!(new_reader.snapshot(), Err(MemoryError::Unavailable));
}

#[tokio::test]
async fn unacknowledged_completed_import_requires_reopen_without_repeating_the_turn() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    let completed = complete_session(&*sessions, &key(&one), "已经完成且投递的原文", "完整回复");
    let source = interaction("completed", completed.clone(), 1);
    let reader = admin.reader(one.clone()).unwrap();
    store.fail_after_writes(true);
    assert_eq!(
        admin.import_completed(&one, 0, source.clone()),
        Err(MemoryError::Storage)
    );
    assert_eq!(reader.snapshot(), Err(MemoryError::Unavailable));
    let writes = store.writes();
    assert_eq!(
        admin.import_completed(&one, 0, source.clone()),
        Err(MemoryError::Unavailable)
    );
    assert_eq!(store.writes(), writes);
    store.fail_after_writes(false);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(sessions);
    drop(store);

    let store = RecordingStore::open(directory.path());
    let disk = std::fs::read(directory.path().join("state.json")).unwrap();
    let (kernel, restored, sessions) = open(store.clone()).await;
    let saved = restored.reader(one.clone()).unwrap().snapshot().unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.evidence.len(), 1);
    assert_eq!(restored.import_completed(&one, 0, source).unwrap(), saved);
    assert_eq!(sessions.snapshot(&key(&one)).unwrap(), Some(completed));
    assert_eq!(store.writes(), 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        disk
    );
    assert_eq!(reader.snapshot(), Err(MemoryError::Unavailable));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn serialized_byte_capacity_rejects_atomically_and_keeps_existing_evidence_readable() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin, sessions) = open(store.clone()).await;
    let one = scope("s", "u");
    // JSON 转义会放大字节；每条原文仍合法，且远未达到证据数量上限。
    let text = format!("x{}", "\u{0001}".repeat(MAX_TEXT_BYTES - 1));
    let mut first = None;
    let mut saved = admin.reader(one.clone()).unwrap().snapshot().unwrap();
    let mut reached_capacity = false;
    for index in 0..MAX_EVIDENCE {
        let completed = complete_session(&*sessions, &key(&one), &text, &text);
        let source = interaction(&format!("large-{index}"), completed, index as u64 + 1);
        if first.is_none() {
            first = Some(source.clone());
        }
        let bytes = store.get(&owner(), MEMORY_STATE_KEY).unwrap();
        let writes = store.writes();
        match admin.import_completed(&one, saved.revision, source) {
            Ok(next) => saved = next,
            Err(MemoryError::LimitReached) => {
                assert_eq!(store.writes(), writes);
                assert_eq!(store.get(&owner(), MEMORY_STATE_KEY).unwrap(), bytes);
                reached_capacity = true;
                break;
            }
            result => panic!("unexpected import result: {result:?}"),
        }
    }
    assert!(reached_capacity);
    assert!(saved.evidence.len() < MAX_EVIDENCE);
    assert!(
        store
            .get(&owner(), MEMORY_STATE_KEY)
            .unwrap()
            .unwrap()
            .len()
            <= MAX_STATE_BYTES
    );
    let writes = store.writes();
    assert_eq!(
        admin.reader(one.clone()).unwrap().snapshot().unwrap(),
        saved
    );
    assert_eq!(
        admin.import_completed(&one, 0, first.unwrap()).unwrap(),
        saved
    );
    assert_eq!(store.writes(), writes);
    let confirmed = admin
        .update_preference(
            &one,
            saved.revision,
            confirm("small", "large-0", "style", "仍能保存较小变更"),
        )
        .unwrap();
    assert_eq!(confirmed.evidence, saved.evidence);
    assert_eq!(confirmed.preferences.len(), 1);
    kernel.stop_all().await.unwrap();
}
