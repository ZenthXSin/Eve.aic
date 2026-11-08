use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::ContextScope;
use eve_plugin_api::*;
use eve_training_api::*;
use eve_training_plugin::TrainingPlugin;
use eve_training_sqlite::SqliteExpressions;
use std::{path::Path, sync::Arc};

fn scope() -> ContextScope {
    ContextScope {
        session_id: "claim-integrity".into(),
        user_id: "human".into(),
    }
}

async fn start(
    state: Arc<MemoryStateStore>,
    store: Arc<SqliteExpressions>,
) -> (Kernel, PluginResult<()>, Arc<dyn ServiceRegistry>) {
    let services = KernelServices {
        state,
        ..Default::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(
            TrainingPlugin::new(true)
                .unwrap()
                .with_conversation_store(store),
        ))
        .unwrap();
    let result = kernel
        .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await;
    (kernel, result, registry)
}

fn service(registry: &Arc<dyn ServiceRegistry>) -> Arc<dyn TrainingService> {
    registry
        .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<TrainingServiceHandle>()
        .unwrap()
        .0
        .clone()
}

fn archive_count(path: &Path) -> u64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM conversation_batches", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[tokio::test]
async fn restart_rejects_forged_claims_without_archiving_or_changing_checkpoint() {
    for corruption in ["body", "metadata", "skip", "utf8", "after"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("training.sqlite3");
        let state = Arc::new(MemoryStateStore::default());
        let store = Arc::new(SqliteExpressions::open(&path).unwrap());
        let (kernel, result, registry) = start(state.clone(), store.clone()).await;
        result.unwrap();
        let training = service(&registry);
        for index in 0..4 {
            training
                .observe_conversation(
                    &scope(),
                    ConversationMessage {
                        id: format!("event-{index}"),
                        speaker: "human".into(),
                        role: ConversationRole::Human,
                        text: format!("原消息 {index}"),
                        metadata: Some(ConversationMetadata {
                            context: Some(String::new()),
                            source_kind: Some("qq_private".into()),
                            ..Default::default()
                        }),
                    },
                )
                .unwrap();
        }
        training.take_conversation_batch(1).unwrap().unwrap();
        kernel.stop_all().await.unwrap();
        let row = store.scope(&scope()).unwrap().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(row.checkpoint.as_ref().unwrap()).unwrap();
        match corruption {
            "body" => value["inflight"]["batch"]["messages"][0]["text"] = "伪造原消息".into(),
            "metadata" => {
                value["inflight"]["batch"]["messages"][0]["metadata"]["source_kind"] = "web".into()
            }
            "skip" => {
                value["inflight"]["batch"]["messages"]
                    .as_array_mut()
                    .unwrap()
                    .remove(0);
            }
            "utf8" => value["cursor"]["text_offset"] = 1.into(),
            "after" => value["inflight"]["after"]["sequence"] = 4.into(),
            _ => unreachable!(),
        }
        let forged = serde_json::to_vec(&value).unwrap();
        assert!(
            store
                .commit(
                    &scope(),
                    &ConversationCommit {
                        expected_revision: row.revision,
                        checkpoint: forged.clone(),
                        revisions: vec![],
                        batch: None,
                    }
                )
                .unwrap()
        );
        assert_eq!(archive_count(&path), 0);
        let (kernel, result, _) =
            start(state, Arc::new(SqliteExpressions::open(&path).unwrap())).await;
        assert!(result.is_err(), "{corruption}");
        assert_eq!(
            store.scope(&scope()).unwrap().unwrap().checkpoint.unwrap(),
            forged,
            "{corruption}"
        );
        assert_eq!(archive_count(&path), 0, "{corruption}");
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_claim_preserves_explicit_empty_context_when_recovering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let state = Arc::new(MemoryStateStore::default());
    let messages: Vec<_> = (0..4)
        .map(|index| ConversationMessage {
            id: format!("legacy-{index}"),
            speaker: "human".into(),
            role: ConversationRole::Human,
            text: format!("旧消息 {index}"),
            metadata: Some(ConversationMetadata {
                context: Some(String::new()),
                ..Default::default()
            }),
        })
        .collect();
    let batch = ConversationBatch {
        id: "conversation-2-1".into(),
        scope: scope(),
        messages: messages.clone(),
        lessons: vec![],
        lesson_history: vec![],
        attempt: 1,
    };
    let legacy = serde_json::to_vec(&serde_json::json!({"version":1,"next":3,"scopes":[{
        "scope":scope(),"messages":messages,"seen":[],"pending":0,"generation":1,"ready_at":0,
        "inflight":batch,"lessons":[]
    }]}))
    .unwrap();
    state
        .set(
            &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
            "conversation.v1".into(),
            legacy.clone(),
        )
        .unwrap();
    let store = Arc::new(SqliteExpressions::open(&path).unwrap());
    let (kernel, result, registry) = start(state.clone(), store.clone()).await;
    result.unwrap();
    assert!(
        service(&registry)
            .take_conversation_batch(1)
            .unwrap()
            .is_none()
    );
    assert_eq!(archive_count(&path), 1);
    assert_eq!(
        state
            .get(
                &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
                "conversation.v1"
            )
            .unwrap()
            .unwrap(),
        legacy
    );
    assert!(
        store
            .events(&scope(), 1, 4)
            .unwrap()
            .iter()
            .all(|event| event.message.metadata.as_ref().unwrap().context.as_deref() == Some(""))
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_rejects_current_lesson_changed_outside_its_saved_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let state = Arc::new(MemoryStateStore::default());
    let store = Arc::new(SqliteExpressions::open(&path).unwrap());
    let (kernel, result, registry) = start(state.clone(), store.clone()).await;
    result.unwrap();
    let training = service(&registry);
    for index in 0..4 {
        training
            .observe_conversation(
                &scope(),
                ConversationMessage {
                    id: format!("source-{index}"),
                    speaker: "human".into(),
                    role: ConversationRole::Human,
                    text: format!("先接话再查资料 {index}"),
                    metadata: None,
                },
            )
            .unwrap();
    }
    let batch = training.take_conversation_batch(1).unwrap().unwrap();
    training
        .finish_conversation_batch(
            &batch,
            vec![ConversationLessonDraft {
                lesson_id: None,
                situation: "准备查资料时".into(),
                response: "先自然接话再查询".into(),
                avoid: "没有内容就不重复说".into(),
                assessment: ConversationAssessment::Observation,
                evidence: batch.messages[..2]
                    .iter()
                    .map(|message| ConversationEvidence {
                        message_id: message.id.clone(),
                        quote: "先接话再查资料".into(),
                    })
                    .collect(),
            }],
            2,
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    let row = store.scope(&scope()).unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(row.checkpoint.as_ref().unwrap()).unwrap();
    value["lessons"][0]["response"] = "无来源的新规则".into();
    let forged = serde_json::to_vec(&value).unwrap();
    assert!(
        store
            .commit(
                &scope(),
                &ConversationCommit {
                    expected_revision: row.revision,
                    checkpoint: forged.clone(),
                    revisions: vec![],
                    batch: None,
                }
            )
            .unwrap()
    );
    let (kernel, result, _) = start(state, Arc::new(SqliteExpressions::open(&path).unwrap())).await;
    assert!(result.is_err());
    assert_eq!(
        store.scope(&scope()).unwrap().unwrap().checkpoint.unwrap(),
        forged
    );
    assert_eq!(archive_count(&path), 1);
    assert_eq!(
        store
            .latest_revision(&scope(), value["lessons"][0]["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .lesson
            .response,
        "先自然接话再查询"
    );
    kernel.stop_all().await.unwrap();
}
