use super::restore_saved_receipts;
use crate::{QQBOT_PLUGIN_ID, QqConversations, state::Ledger};
use eve_kernel::{
    Kernel, KernelServices,
    backends::{MemoryLogger, MemoryStateStore},
};
use eve_llm_api::ContextScope;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginFuture, PluginId, PluginManifest,
    ServiceId, StateStore,
};
use eve_training_api::{
    PHRASE_BATCH, PhraseDraft, PhraseFailure, PhraseFailureOutcome, PhraseFinish, PhraseKind,
    TRAINING_PLUGIN_ID, TRAINING_SERVICE_ID, TrainingService, TrainingServiceHandle,
};
use eve_training_plugin::TrainingPlugin;
use serde_json::{Value, json};
use std::sync::Arc;

/// 只运行真实恢复入口和训练插件；不装配桥、模型或工具执行器。
struct RestorePlugin(PluginManifest);
impl Plugin for RestorePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.0
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let training = ctx
                .service::<TrainingServiceHandle>(&ServiceId::new(TRAINING_SERVICE_ID)?)?
                .unwrap();
            restore_saved_receipts(
                "app",
                &ctx,
                &Ledger::load(&ctx)?,
                &QqConversations::default(),
                None,
                Some(&training.0),
            )?;
            Ok(None)
        })
    }
}

struct Host {
    kernel: Kernel,
    state: Arc<MemoryStateStore>,
    logger: Arc<MemoryLogger>,
    training: Arc<dyn TrainingService>,
}
impl Host {
    async fn open(state: Arc<MemoryStateStore>) -> Self {
        let logger = Arc::new(MemoryLogger::default());
        let services = KernelServices {
            state: state.clone(),
            logger: logger.clone(),
            ..KernelServices::default()
        };
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        kernel
            .register(Box::new(
                TrainingPlugin::new(true).unwrap().with_phrase_learning(),
            ))
            .unwrap();
        kernel
            .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let training = registry
            .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<TrainingServiceHandle>()
            .unwrap()
            .0
            .clone();
        let mut manifest = PluginManifest::new(QQBOT_PLUGIN_ID, "0.1.0").unwrap();
        manifest.dependencies.push(PluginDependency {
            id: PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
            requirement: Some("^0.1".into()),
        });
        kernel.register(Box::new(RestorePlugin(manifest))).unwrap();
        Self {
            kernel,
            state,
            logger,
            training,
        }
    }
    async fn restore(&self) {
        self.kernel
            .start(&PluginId::new(QQBOT_PLUGIN_ID).unwrap())
            .await
            .unwrap();
    }
    fn raw(&self, owner: &str, key: &str) -> Vec<u8> {
        self.state
            .get(&PluginId::new(owner).unwrap(), key)
            .unwrap()
            .unwrap()
    }
    fn document(&self, owner: &str, key: &str) -> Value {
        serde_json::from_slice(&self.raw(owner, key)).unwrap()
    }
    fn save_receipts(&self, entries: &[Value]) -> Vec<u8> {
        let bytes = serde_json::to_vec(&json!({"version":10,"entries":entries})).unwrap();
        Ledger::decode(&bytes).unwrap();
        self.state
            .set(
                &PluginId::new(QQBOT_PLUGIN_ID).unwrap(),
                "receipts.v1".into(),
                bytes.clone(),
            )
            .unwrap();
        bytes
    }
}

fn receipt(id: &str, text: &str, content_type: Option<&str>) -> Value {
    let mut message = json!({
        "id":id,"scope":"c2c","target_id":"user","user_id":"user","text":text
    });
    if let Some(content_type) = content_type {
        message["attachments"] = json!([{
            "content_type":content_type,"url":"https://example.invalid/attachment"
        }]);
    }
    json!({"app_id":"app","message":message,"state":"Failed","reply":null})
}
fn scope() -> ContextScope {
    let message: crate::Message =
        serde_json::from_value(receipt("scope", "范围", None)["message"].clone()).unwrap();
    let session = message.session_key("app").unwrap();
    ContextScope {
        session_id: session.session_id,
        user_id: session.user_id,
    }
}

#[tokio::test]
async fn restore_without_expression_index_skips_attachments_but_recovers_failed_text_once() {
    let state = Arc::new(MemoryStateStore::default());
    let host = Host::open(state.clone()).await;
    let mut other_app = receipt("other-app", "其他应用原话", None);
    other_app["app_id"] = "other".into();
    let mut unknown = receipt("unknown-image", "未确认发送的图片说明", Some("image/png"));
    unknown["state"] = "Unknown".into();
    unknown["reply"] = "尚未确认送达的旧回复".into();
    let entries = vec![
        receipt("image", "这张图真好看", Some("image/png")),
        receipt("image-only", "", Some("image/jpeg")),
        receipt("file", "这里是附件说明", Some("application/pdf")),
        unknown,
        receipt("command", "先等等\n /train start", None),
        other_app,
        receipt("text", "今天有空就聊一会儿", None),
    ];
    let receipts = host.save_receipts(&entries);
    assert!(
        state
            .get(&PluginId::new(TRAINING_PLUGIN_ID).unwrap(), "expression.v1")
            .unwrap()
            .is_none()
    );
    host.restore().await;
    assert_eq!(
        host.training
            .expression_snapshot(&scope())
            .unwrap()
            .unwrap()
            .samples,
        1
    );
    assert!(
        host.training
            .is_expression_source(&scope(), "text", "今天有空就聊一会儿")
            .unwrap()
    );
    assert_eq!(
        host.document(TRAINING_PLUGIN_ID, "phrases.v1")["pending"],
        json!([{"scope":scope(),"text":"今天有空就聊一会儿"}])
    );
    assert_eq!(host.raw(QQBOT_PLUGIN_ID, "receipts.v1"), receipts);
    let expressions = host.raw(TRAINING_PLUGIN_ID, "expression.v1");
    let phrases = host.raw(TRAINING_PLUGIN_ID, "phrases.v1");
    host.kernel.stop_all().await.unwrap();

    let restored = Host::open(state).await;
    restored.restore().await;
    assert_eq!(
        restored.raw(TRAINING_PLUGIN_ID, "expression.v1"),
        expressions
    );
    assert_eq!(restored.raw(TRAINING_PLUGIN_ID, "phrases.v1"), phrases);
    assert_eq!(restored.raw(QQBOT_PLUGIN_ID, "receipts.v1"), receipts);
    restored.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn existing_attachment_samples_learned_phrases_and_retry_failures_survive_restore() {
    let state = Arc::new(MemoryStateStore::default());
    let host = Host::open(state.clone()).await;
    let mut entries = Vec::new();
    // 模拟已被旧版采集的附件；只用公开训练服务生成真实去重、已学和失败状态。
    for prefix in ["learned", "retry"] {
        for index in 0..PHRASE_BATCH {
            let id = format!("{prefix}-{index}");
            let text = format!("这游戏yyds，历史图片{index}");
            host.training
                .observe_user_message(&scope(), &id, &text)
                .unwrap();
            entries.push(receipt(&id, &text, Some("image/png")));
        }
        let batch = host.training.take_phrase_batch(1).unwrap().unwrap();
        if prefix == "learned" {
            assert_eq!(
                host.training
                    .finish_phrase_batch(
                        &batch,
                        vec![PhraseDraft {
                            kind: PhraseKind::Term,
                            text: "yyds".into(),
                            meaning: "夸东西特别好".into(),
                            example: "这游戏yyds".into(),
                        }],
                        2,
                    )
                    .unwrap(),
                PhraseFinish::Saved(1)
            );
        } else {
            assert!(matches!(
                host.training
                    .fail_phrase_batch(&batch, PhraseFailure::ServerError, 2)
                    .unwrap(),
                PhraseFailureOutcome::Retry { .. }
            ));
        }
    }
    host.training
        .observe_user_message(&scope(), "pending", "已经采集过的图片说明")
        .unwrap();
    entries.push(receipt(
        "pending",
        "已经采集过的图片说明",
        Some("image/png"),
    ));
    let receipts = host.save_receipts(&entries);
    let expressions = host.raw(TRAINING_PLUGIN_ID, "expression.v1");
    let phrases = host.raw(TRAINING_PLUGIN_ID, "phrases.v1");
    host.restore().await;
    assert_eq!(host.raw(TRAINING_PLUGIN_ID, "expression.v1"), expressions);
    assert_eq!(host.raw(TRAINING_PLUGIN_ID, "phrases.v1"), phrases);
    host.kernel.stop_all().await.unwrap();

    let restored = Host::open(state).await;
    restored.restore().await;
    assert_eq!(
        restored.raw(TRAINING_PLUGIN_ID, "expression.v1"),
        expressions
    );
    assert_eq!(restored.raw(TRAINING_PLUGIN_ID, "phrases.v1"), phrases);
    assert_eq!(restored.raw(QQBOT_PLUGIN_ID, "receipts.v1"), receipts);
    restored.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn ordinary_source_conflict_still_logs_and_preserves_history_without_learning_later_rows() {
    let host = Host::open(Arc::new(MemoryStateStore::default())).await;
    host.training
        .observe_user_message(&scope(), "conflict", "先前已保存的原话")
        .unwrap();
    let receipts = host.save_receipts(&[
        receipt("image", "未采集的图片说明", Some("image/png")),
        receipt("conflict", "同一编号但不同原话", None),
        receipt("later", "此次应停止补采", None),
    ]);
    let expressions = host.raw(TRAINING_PLUGIN_ID, "expression.v1");
    let phrases = host.raw(TRAINING_PLUGIN_ID, "phrases.v1");
    host.restore().await;
    assert_eq!(host.raw(TRAINING_PLUGIN_ID, "expression.v1"), expressions);
    assert_eq!(host.raw(TRAINING_PLUGIN_ID, "phrases.v1"), phrases);
    assert_eq!(host.raw(QQBOT_PLUGIN_ID, "receipts.v1"), receipts);
    assert!(
        host.logger
            .snapshot()
            .unwrap()
            .records
            .iter()
            .any(|record| { record.entry.message == "expression_history_observation_failed" })
    );
    host.kernel.stop_all().await.unwrap();
}
