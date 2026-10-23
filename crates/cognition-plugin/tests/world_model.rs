use eve_cognition_api::*;
use eve_cognition_plugin::{WorldModelController, WorldModelPlugin, evaluate_world_receipt};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn scope() -> WorldScope {
    WorldScope {
        environment_sha256: "a".repeat(64),
        capability: "code-check".into(),
        visibility: Visibility::User("alice".into()),
    }
}
fn prediction() -> WorldPrediction {
    WorldPrediction {
        id: "prediction-1".into(),
        execution_id: "task-1".into(),
        scope: scope(),
        observation_ids: vec![],
        check: WorldCheck {
            tool: "check".into(),
            arguments_sha256: "b".repeat(64),
            output_pointer: "/passed".into(),
            expected: json!(true),
        },
        success_probability_percent: 80,
        created_at_ms: 10,
        deadline_ms: 100,
    }
}
fn receipt() -> WorldReceipt {
    WorldReceipt {
        execution_id: "task-1".into(),
        environment_sha256: "a".repeat(64),
        source: Source {
            kind: SourceKind::Tool,
            channel: "host".into(),
            reference: "call-1".into(),
        },
        tool: "check".into(),
        arguments_sha256: "b".repeat(64),
        output: json!({"passed":true,"unrelated_private":"not-for-reader"}),
        succeeded: true,
        final_batch: true,
        at_ms: 20,
    }
}
async fn open(store: Arc<dyn StateStore>) -> (Kernel, WorldModelController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = WorldModelPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}

#[test]
fn independent_receipt_rejects_self_report_and_preserves_unknown() {
    let prediction = prediction();
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&receipt())),
        Ok(WorldEffect::Verified)
    );
    let mut value = receipt();
    value.environment_sha256 = "f".repeat(64);
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Ok(WorldEffect::Inconclusive)
    );
    value = receipt();
    value.output = json!({"passed":false});
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Ok(WorldEffect::Failed)
    );
    value.final_batch = false;
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Ok(WorldEffect::Inconclusive)
    );
    value = receipt();
    value.succeeded = false;
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Ok(WorldEffect::Inconclusive)
    );
    value = receipt();
    value.at_ms = 101;
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Ok(WorldEffect::Inconclusive)
    );
    value = receipt();
    value.output = json!({"description":"I succeeded"});
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Ok(WorldEffect::Inconclusive)
    );
    value = receipt();
    value.source.kind = SourceKind::Inference;
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Err(CognitionError::InvalidInput)
    );
    value = receipt();
    value.arguments_sha256 = "c".repeat(64);
    assert_eq!(
        evaluate_world_receipt(&prediction, Some(&value)),
        Err(CognitionError::InvalidInput)
    );
    assert_eq!(
        evaluate_world_receipt(&prediction, None),
        Ok(WorldEffect::Inconclusive)
    );
}

#[tokio::test]
async fn persistence_is_idempotent_and_isolates_user_and_environment() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    admin.predict(prediction()).unwrap();
    admin.predict(prediction()).unwrap();
    assert_eq!(admin.snapshot().unwrap().revision, 1);
    let mut changed = prediction();
    changed.check.expected = json!(false);
    assert_eq!(admin.predict(changed), Err(CognitionError::InvalidInput));
    assert_eq!(
        admin.resolve("prediction-1", Some(receipt()), 21),
        Ok(WorldEffect::Verified)
    );
    assert_eq!(
        admin.resolve("prediction-1", Some(receipt()), 22),
        Ok(WorldEffect::Verified)
    );
    assert_eq!(admin.snapshot().unwrap().revision, 2);
    let reader = admin.reader(ReadAccess::User("alice".into())).unwrap();
    let view = reader.snapshot(&"a".repeat(64), 22).unwrap();
    assert_eq!(view.trials[0].effect, Some(WorldEffect::Verified));
    assert_eq!(
        view.trials[0].receipt_sha256,
        Some(world_digest(&receipt()).unwrap())
    );
    assert!(!serde_json::to_string(&view).unwrap().contains("\"output\""));
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("not-for-reader")
    );
    let estimates = reader.capabilities(&"a".repeat(64)).unwrap();
    assert_eq!(estimates.len(), 1);
    assert_eq!(estimates[0].verified, 1);
    assert_eq!(estimates[0].success_probability_percent, 66);
    assert_eq!(estimates[0].forecast_error_percent, Some(20));
    assert!(reader.capabilities(&"c".repeat(64)).unwrap().is_empty());
    assert!(
        admin
            .reader(ReadAccess::Public)
            .unwrap()
            .capabilities(&"a".repeat(64))
            .unwrap()
            .is_empty()
    );
    assert!(
        admin
            .reader(ReadAccess::User("bob".into()))
            .unwrap()
            .snapshot(&"a".repeat(64), 22)
            .unwrap()
            .trials
            .is_empty()
    );
    let saved = admin.snapshot().unwrap();
    kernel.stop_all().await.unwrap();
    assert_eq!(admin.snapshot(), Err(CognitionError::Unavailable));
    assert_eq!(
        reader.snapshot(&"a".repeat(64), 22),
        Err(CognitionError::Unavailable)
    );
    let (kernel, admin) = open(store).await;
    assert_eq!(admin.snapshot().unwrap(), saved);
    assert_eq!(
        admin.resolve("prediction-1", None, 23),
        Err(CognitionError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn observations_require_fresh_and_non_broadening_lineage() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    let observation = WorldObservation {
        id: "observation".into(),
        scope: scope(),
        source: Source {
            kind: SourceKind::Environment,
            channel: "host".into(),
            reference: "observation".into(),
        },
        summary: "工具可用".into(),
        evidence_sha256: "c".repeat(64),
        at_ms: 1,
        expires_at_ms: 50,
    };
    admin.observe(observation.clone()).unwrap();
    admin.observe(observation).unwrap();
    let mut prediction = prediction();
    prediction.observation_ids.push("observation".into());
    prediction.scope.visibility = Visibility::Public;
    assert_eq!(
        admin.predict(prediction.clone()),
        Err(CognitionError::AccessDenied)
    );
    prediction.scope = scope();
    prediction.created_at_ms = 50;
    assert_eq!(
        admin.predict(prediction.clone()),
        Err(CognitionError::AccessDenied)
    );
    prediction.created_at_ms = 10;
    admin.predict(prediction).unwrap();
    let reader = admin.reader(ReadAccess::User("alice".into())).unwrap();
    assert_eq!(
        reader
            .snapshot(&"a".repeat(64), 10)
            .unwrap()
            .observations
            .len(),
        1
    );
    assert!(
        reader
            .snapshot(&"a".repeat(64), 50)
            .unwrap()
            .observations
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_preserves_pending_and_unknown_does_not_improve_capability() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    admin.predict(prediction()).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(store).await;
    assert!(admin.snapshot().unwrap().trials[0].resolution.is_none());
    assert_eq!(
        admin.resolve("prediction-1", None, 200),
        Ok(WorldEffect::Inconclusive)
    );
    let estimate = &admin
        .reader(ReadAccess::Internal)
        .unwrap()
        .capabilities(&"a".repeat(64))
        .unwrap()[0];
    assert_eq!(
        (estimate.verified, estimate.failed, estimate.inconclusive),
        (0, 0, 1)
    );
    assert_eq!(estimate.success_probability_percent, 50);
    assert_eq!(estimate.forecast_error_percent, None);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_persisted_effect_is_not_trusted_or_reset() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = open(store.clone()).await;
    admin.predict(prediction()).unwrap();
    admin.resolve("prediction-1", None, 20).unwrap();
    kernel.stop_all().await.unwrap();
    let namespace = PluginId::new(WORLD_MODEL_PLUGIN_ID).unwrap();
    let bytes = store
        .get(&namespace, WORLD_MODEL_STATE_KEY)
        .unwrap()
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["snapshot"]["trials"][0]["resolution"]["effect"] = json!("Verified");
    let corrupt = serde_json::to_vec(&value).unwrap();
    store
        .set(&namespace, WORLD_MODEL_STATE_KEY.into(), corrupt.clone())
        .unwrap();
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = WorldModelPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(admin.snapshot(), Err(CognitionError::Unavailable));
    assert_eq!(
        store.get(&namespace, WORLD_MODEL_STATE_KEY).unwrap(),
        Some(corrupt)
    );
}

#[derive(Default)]
struct AmbiguousStore {
    memory: MemoryStateStore,
    fail: AtomicBool,
}
impl StateStore for AmbiguousStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        self.memory.set(namespace, key, bytes)?;
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("ambiguous".into()));
        }
        Ok(())
    }
}
#[tokio::test]
async fn ambiguous_write_closes_handle_until_reload() {
    let store = Arc::new(AmbiguousStore::default());
    let (kernel, admin) = open(store.clone()).await;
    store.fail.store(true, Ordering::SeqCst);
    assert_eq!(admin.predict(prediction()), Err(CognitionError::Storage));
    assert_eq!(admin.snapshot(), Err(CognitionError::Unavailable));
    assert_eq!(
        admin.predict(prediction()),
        Err(CognitionError::Unavailable)
    );
    kernel.stop_all().await.unwrap();
    store.fail.store(false, Ordering::SeqCst);
    let (kernel, admin) = open(store).await;
    assert_eq!(admin.snapshot().unwrap().trials.len(), 1);
    admin.predict(prediction()).unwrap();
    assert_eq!(admin.snapshot().unwrap().revision, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_ledger_rejects_new_prediction_but_can_resolve_existing() {
    let (kernel, admin) = open(Arc::new(MemoryStateStore::default())).await;
    for index in 0..MAX_WORLD_RECORDS {
        let mut value = prediction();
        value.id = format!("prediction-{index}");
        value.execution_id = format!("task-{index}");
        admin.predict(value).unwrap();
    }
    let before = admin.snapshot().unwrap();
    let mut extra = prediction();
    extra.id = "extra".into();
    extra.execution_id = "extra-task".into();
    assert_eq!(admin.predict(extra), Err(CognitionError::LimitReached));
    assert_eq!(admin.snapshot().unwrap(), before);
    assert_eq!(
        admin.resolve("prediction-1", Some(receipt()), 21),
        Ok(WorldEffect::Verified)
    );
    assert_eq!(admin.snapshot().unwrap().trials.len(), MAX_WORLD_RECORDS);
    kernel.stop_all().await.unwrap();
}
