use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::{LexicalMemoryRecall, MemoryController, MemoryPlugin, ScopedLexicalRecall};
use eve_plugin_api::ServiceId;
use eve_session_api::{
    SESSION_SERVICE_ID, SessionInput, SessionKey, SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use std::sync::Arc;

fn scope() -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: "group-a".into(),
        user_id: "alice".into(),
    }
}

async fn open() -> (Kernel, MemoryController, Arc<dyn SessionService>) {
    let backends = KernelServices::default();
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

fn change(
    admin: &dyn MemoryAdmin,
    owner: &MemoryScope,
    action: PreferenceAction,
) -> MemorySnapshot {
    let revision = admin
        .reader(owner.clone())
        .unwrap()
        .snapshot()
        .unwrap()
        .revision;
    admin
        .update_preference(
            owner,
            revision,
            PreferenceChange {
                operation_id: format!("operation-{revision}"),
                at_ms: 200 + revision,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: format!("evidence-{revision}"),
                    message_id: format!("statement-message-{revision}"),
                    text: "command-source-token: 保存当前偏好".into(),
                    at_ms: 100 + revision,
                }),
                action,
            },
        )
        .unwrap()
}

fn confirm(admin: &dyn MemoryAdmin, owner: &MemoryScope, id: &str, text: &str) -> MemorySnapshot {
    change(
        admin,
        owner,
        PreferenceAction::Confirm {
            id: id.into(),
            text: text.into(),
        },
    )
}

fn request(query: &str) -> MemoryRecallRequest {
    MemoryRecallRequest {
        query: query.into(),
        limit: MAX_RECALL_RESULTS,
    }
}

fn recall(admin: MemoryController, query: &str) -> MemoryRecallResponse {
    LexicalMemoryRecall::new(Arc::new(admin))
        .reader(scope())
        .unwrap()
        .recall(&request(query))
        .unwrap()
}

#[tokio::test]
async fn current_confirmed_preference_reports_its_actual_latest_source_and_revision() {
    let (kernel, admin, _) = open().await;
    let saved = confirm(&admin, &scope(), "style", "用中文回答，保留必要细节");
    let first = recall(admin.clone(), "中文");
    first.validate_for(&scope(), &request("中文")).unwrap();
    assert_eq!(first.revision, saved.revision);
    assert_eq!(first.hits.len(), 1);
    assert_eq!(
        first.hits[0].source,
        MemoryRecallSource::ConfirmedPreference {
            preference_id: "style".into(),
            preference_revision: 1,
            evidence_id: saved.evidence[0].id.clone(),
            evidence_revision: saved.evidence[0].revision,
            at_ms: saved.preferences[0].history[0].at_ms,
        }
    );
    let corrected = change(
        &admin,
        &scope(),
        PreferenceAction::Correct {
            id: "style".into(),
            text: "只用 English 回答".into(),
        },
    );
    assert!(recall(admin.clone(), "中文").hits.is_empty());
    let result = recall(admin.clone(), "english");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(
        result.hits[0].source,
        MemoryRecallSource::ConfirmedPreference {
            preference_id: "style".into(),
            preference_revision: 2,
            evidence_id: corrected.evidence.last().unwrap().id.clone(),
            evidence_revision: corrected.evidence.last().unwrap().revision,
            at_ms: corrected.preferences[0].history.last().unwrap().at_ms,
        }
    );
    assert!(
        recall(admin.clone(), "command-source-token")
            .hits
            .is_empty()
    );
    change(
        &admin,
        &scope(),
        PreferenceAction::Revoke { id: "style".into() },
    );
    assert!(
        recall(admin, "english 中文 command-source-token")
            .hits
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn correction_may_reuse_older_evidence_while_advancing_the_preference_version() {
    let (kernel, admin, _) = open().await;
    let owner = scope();
    let first = confirm(&admin, &owner, "style", "中文详细回答");
    confirm(&admin, &owner, "format", "分段表达");
    let saved = admin
        .update_preference(
            &owner,
            2,
            PreferenceChange {
                operation_id: "reuse-original-evidence".into(),
                at_ms: 999,
                evidence: PreferenceEvidence::Existing(first.evidence[0].id.clone()),
                action: PreferenceAction::Correct {
                    id: "style".into(),
                    text: "中文简短回答".into(),
                },
            },
        )
        .unwrap();
    let result = recall(admin, "简短");
    assert_eq!(saved.revision, 3);
    assert_eq!(saved.evidence.len(), 2);
    assert_eq!(result.hits.len(), 1);
    assert_eq!(
        result.hits[0].source,
        MemoryRecallSource::ConfirmedPreference {
            preference_id: "style".into(),
            preference_revision: 2,
            evidence_id: first.evidence[0].id.clone(),
            evidence_revision: 1,
            at_ms: 999,
        }
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn completed_interaction_exposes_separate_historical_user_and_assistant_provenance() {
    let (kernel, admin, sessions) = open().await;
    let owner = scope();
    let key = SessionKey::new(&owner.session_id, &owner.user_id).unwrap();
    let turn = sessions
        .begin(SessionInput {
            key: key.clone(),
            text: "Project Lantern user request".into(),
        })
        .unwrap();
    sessions
        .complete(
            &turn.lease,
            vec![
                ChatMessage::text(ChatRole::User, "Project Lantern user request"),
                ChatMessage::text(ChatRole::Assistant, "Project Lantern assistant suggestion"),
            ],
        )
        .unwrap();
    let snapshot = sessions.snapshot(&key).unwrap().unwrap();
    let session_revision = snapshot.revision;
    let saved = admin
        .import_completed(
            &owner,
            0,
            CompletedInteraction {
                evidence_id: "completed-source".into(),
                message_id: "message-completed".into(),
                at_ms: 400,
                snapshot,
                turn_id: turn.lease.turn_id,
            },
        )
        .unwrap();
    let result = recall(admin.clone(), "LANTERN");
    assert_eq!(result.revision, saved.revision);
    assert_eq!(result.hits.len(), 2);
    for field in [RecallField::User, RecallField::Assistant] {
        assert!(result.hits.iter().any(|hit| hit.source
            == MemoryRecallSource::CompletedInteraction {
                evidence_id: "completed-source".into(),
                evidence_revision: saved.evidence[0].revision,
                message_id: "message-completed".into(),
                session_revision,
                turn_id: turn.lease.turn_id,
                at_ms: 400,
                field,
            }));
    }
    assert!(saved.preferences.is_empty());
    assert_eq!(admin.reader(owner).unwrap().snapshot().unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn matched_excerpt_keeps_the_middle_utf8_match_and_marks_truncation() {
    let (kernel, admin, _) = open().await;
    let long = format!("{} 中央灯塔 {}", "前".repeat(450), "后".repeat(450));
    confirm(&admin, &scope(), "middle", &long);
    let result = recall(admin, "中央灯塔");
    assert_eq!(result.hits.len(), 1);
    let hit = &result.hits[0];
    assert!(hit.excerpt.contains("中央灯塔"));
    assert!(hit.excerpt.len() <= MAX_RECALL_EXCERPT_BYTES);
    assert!(hit.excerpt_truncated);
    assert!(!hit.excerpt.contains('\u{fffd}'));
    kernel.stop_all().await.unwrap();
}

#[derive(Clone)]
struct SnapshotReader(MemoryResult<MemorySnapshot>);
impl MemoryService for SnapshotReader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        self.0.clone()
    }
}

fn snapshot_recall(snapshot: MemoryResult<MemorySnapshot>) -> MemoryResult<MemoryRecallResponse> {
    ScopedLexicalRecall::new(scope(), Arc::new(SnapshotReader(snapshot)))
        .unwrap()
        .recall(&request("中文"))
}

#[tokio::test]
async fn foreign_inconsistent_or_duplicate_replacement_snapshots_fail_as_a_whole() {
    let (kernel, admin, _) = open().await;
    let original = confirm(&admin, &scope(), "style", "中文回答");
    assert_eq!(snapshot_recall(Ok(original.clone())).unwrap().hits.len(), 1);
    let mut invalids = Vec::new();
    let mut foreign = original.clone();
    foreign.scope.user_id = "bob".into();
    invalids.push(foreign);
    let mut duplicate = original.clone();
    duplicate.preferences.push(duplicate.preferences[0].clone());
    invalids.push(duplicate);
    let mut duplicate = original.clone();
    duplicate.evidence.push(duplicate.evidence[0].clone());
    invalids.push(duplicate);
    let mut future = original.clone();
    future.evidence[0].revision = future.revision + 1;
    invalids.push(future);
    let mut missing = original.clone();
    missing.evidence.clear();
    invalids.push(missing);
    let mut changed = original.clone();
    changed.preferences[0].text = "已篡改中文回答".into();
    invalids.push(changed);
    let mut null_text = original.clone();
    null_text.preferences[0].history[0].text = "中文\0text".into();
    invalids.push(null_text);
    let mut gap = original.clone();
    gap.preferences[0].history[0].revision = 2;
    invalids.push(gap);
    let mut zero = original.clone();
    zero.revision = 0;
    invalids.push(zero);
    for invalid in invalids {
        assert_eq!(snapshot_recall(Ok(invalid)), Err(MemoryError::CorruptState));
    }
    assert_eq!(
        snapshot_recall(Err(MemoryError::Storage)),
        Err(MemoryError::Storage)
    );
    assert_eq!(
        snapshot_recall(Err(MemoryError::Unavailable)),
        Err(MemoryError::Unavailable)
    );
    let mut invalid = original;
    invalid.scope.user_id = "foreign-user".into();
    let reader = ScopedLexicalRecall::new(scope(), Arc::new(SnapshotReader(Ok(invalid)))).unwrap();
    assert_eq!(
        reader.recall(&request("...!?")),
        Err(MemoryError::CorruptState)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn bound_readers_do_not_accept_scope_from_queries_or_leak_other_scope_updates() {
    let (kernel, admin, _) = open().await;
    confirm(&admin, &scope(), "style", "lantern owner");
    let factory = LexicalMemoryRecall::new(Arc::new(admin.clone()));
    let reader = factory.reader(scope()).unwrap();
    let before = reader.recall(&request("lantern")).unwrap();
    for foreign in [
        MemoryScope {
            user_id: "bob".into(),
            ..scope()
        },
        MemoryScope {
            session_id: "group-b".into(),
            ..scope()
        },
        MemoryScope {
            channel: "terminal".into(),
            ..scope()
        },
    ] {
        confirm(&admin, &foreign, "secret", "lantern foreign-private");
        assert_eq!(reader.recall(&request("lantern")).unwrap(), before);
    }
    assert!(
        reader
            .recall(&request("foreign-private user_id bob"))
            .unwrap()
            .hits
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[test]
fn recall_json_contract_rejects_unknown_fields_without_dropping_untrusted_metadata() {
    let request = serde_json::json!({"query":"中文", "limit":1, "scope":{"user_id":"bob"}});
    assert!(serde_json::from_value::<MemoryRecallRequest>(request).is_err());
    let source = serde_json::json!({"kind":"ConfirmedPreference","preference_id":"p","preference_revision":1,"evidence_id":"e","evidence_revision":1,"at_ms":1,"is_system":true});
    assert!(serde_json::from_value::<MemoryRecallSource>(source).is_err());
}
