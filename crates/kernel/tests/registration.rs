use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginFuture, PluginId, PluginManifest, PluginRegistry,
    StateStore,
};
use std::sync::Arc;
use tokio::sync::Notify;

fn plugin(id: &str) -> Box<dyn Plugin> {
    Box::new(SimplePlugin {
        manifest: PluginManifest::new(id, "0.1.0").unwrap(),
    })
}

struct SimplePlugin {
    manifest: PluginManifest,
}

impl Plugin for SimplePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, _: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async { Ok(None) })
    }
}

struct StatefulPlugin {
    manifest: PluginManifest,
}

impl Plugin for StatefulPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.state_set("保留", "状态".as_bytes().to_vec())?;
            Ok(None)
        })
    }
}

struct GatedPlugin {
    manifest: PluginManifest,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Plugin for GatedPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, _: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok(None)
        })
    }
}

#[tokio::test]
async fn registration_is_rejected_while_a_lifecycle_operation_holds_the_admission_lock() {
    let kernel = Kernel::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    kernel
        .register(Box::new(GatedPlugin {
            manifest: PluginManifest::new("gated", "0.1.0").unwrap(),
            entered: entered.clone(),
            release: release.clone(),
        }))
        .unwrap();

    let operation = kernel
        .submit_lifecycle(eve_plugin_api::LifecycleRequest::Start(
            PluginId::new("gated").unwrap(),
        ))
        .await
        .unwrap();
    entered.notified().await;

    let error = kernel.register(plugin("late")).unwrap_err();
    assert!(error.to_string().contains("不能注册插件"));
    assert_eq!(kernel.state(&PluginId::new("late").unwrap()), None);

    release.notify_one();
    let report = kernel.wait_lifecycle(operation).await.unwrap();
    assert!(matches!(
        report.state,
        eve_plugin_api::LifecycleOperationState::Completed(Ok(()))
    ));
    kernel.acknowledge_lifecycle(operation).unwrap();

    kernel.register(plugin("late")).unwrap();
    assert_eq!(
        kernel.state(&PluginId::new("late").unwrap()),
        Some(eve_plugin_api::PluginState::Registered)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unregister_preserves_state_and_allows_reusing_the_plugin_id() {
    let state = Arc::new(MemoryStateStore::default());
    let kernel = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..Default::default()
    });
    let id = PluginId::new("reusable").unwrap();
    kernel
        .register(Box::new(StatefulPlugin {
            manifest: PluginManifest::new("reusable", "0.1.0").unwrap(),
        }))
        .unwrap();
    kernel.start(&id).await.unwrap();
    kernel.stop(&id).await.unwrap();

    let registry: &dyn PluginRegistry = &kernel;
    registry.unregister(&id).unwrap();
    assert_eq!(kernel.state(&id), None);
    assert_eq!(
        state.get(&id, "保留").unwrap(),
        Some("状态".as_bytes().to_vec())
    );

    registry.register(plugin("reusable")).unwrap();
    assert_eq!(
        kernel.state(&id),
        Some(eve_plugin_api::PluginState::Registered)
    );
}

#[tokio::test]
async fn unregister_rejects_an_active_plugin_until_it_is_stopped() {
    let kernel = Kernel::new();
    let id = PluginId::new("active").unwrap();
    kernel.register(plugin("active")).unwrap();
    kernel.start(&id).await.unwrap();

    let error = kernel.unregister(&id).unwrap_err();
    assert!(matches!(
        error,
        eve_plugin_api::PluginError::InvalidLifecycle { .. }
    ));
    assert_eq!(kernel.state(&id), Some(eve_plugin_api::PluginState::Active));

    kernel.stop(&id).await.unwrap();
    kernel.unregister(&id).unwrap();
    assert_eq!(kernel.state(&id), None);
}
