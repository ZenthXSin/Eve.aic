use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ContextAssembler, ContextScope, TurnInput};
use eve_plugin_api::*;
use eve_training_api::*;
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct FallibleState {
    storage: MemoryStateStore,
    fail: AtomicBool,
}
impl StateStore for FallibleState {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.storage.get(owner, key)
    }
    fn set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("test failure".into()));
        }
        self.storage.set(owner, key, value)
    }
}
fn scope(session: &str, user: &str) -> ContextScope {
    ContextScope {
        session_id: session.into(),
        user_id: user.into(),
    }
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

#[tokio::test]
async fn storage_failure_preserves_mode_and_stopped_handles_fail() {
    let state = Arc::new(FallibleState {
        storage: MemoryStateStore::default(),
        fail: AtomicBool::new(false),
    });
    let backends = KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TrainingPlugin::new(false).unwrap()))
        .unwrap();
    let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    kernel.start(&id).await.unwrap();
    let old = service(&registry);
    let one = scope("group-1", "user-1");
    state.fail.store(true, Ordering::SeqCst);
    assert!(old.set_enabled(&one, true).is_err());
    assert!(!old.enabled(&one).unwrap());
    state.fail.store(false, Ordering::SeqCst);
    old.set_enabled(&one, true).unwrap();
    assert!(!old.enabled(&scope("group-1", "user-2")).unwrap());
    let context = TrainingContext(old.clone());
    assert!(
        context
            .assemble(TurnInput {
                text: "/train start user-1".into()
            })
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    assert!(
        !context
            .assemble_scoped(
                TurnInput {
                    text: "答案".into()
                },
                Some(one.clone())
            )
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    kernel.stop(&id).await.unwrap();
    assert!(old.enabled(&one).is_err());
    assert!(
        context
            .assemble_scoped(
                TurnInput {
                    text: "答案".into()
                },
                Some(one.clone())
            )
            .await
            .is_err()
    );
    kernel.start(&id).await.unwrap();
    assert!(service(&registry).enabled(&one).unwrap());
    assert!(old.set_enabled(&one, false).is_err());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn capacity_rejects_new_scope_without_evicting_existing_stop() {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TrainingPlugin::new(true).unwrap()))
        .unwrap();
    kernel
        .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let modes = service(&registry);
    for i in 0..256 {
        modes
            .set_enabled(&scope(&format!("s-{i}"), "u"), false)
            .unwrap();
    }
    assert!(modes.set_enabled(&scope("overflow", "u"), false).is_err());
    assert!(!modes.enabled(&scope("s-0", "u")).unwrap());
    modes.set_enabled(&scope("s-0", "u"), true).unwrap();
    assert!(modes.enabled(&scope("s-0", "u")).unwrap());
    kernel.stop_all().await.unwrap();
}
