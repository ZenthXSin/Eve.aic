use super::*;
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginManifest,
};
use eve_segment_api::{SegmentChange, SegmentPreference, SegmentPreferences, SegmentScope};
use eve_segment_plugin::{SEGMENT_PREFERENCES_PLUGIN_ID, SegmentPreferencePlugin};

fn manager(kernel: &Kernel, registry: Arc<dyn ServiceRegistry>, managed: &[&str]) -> PanelPlugins {
    PanelPlugins::new(
        Arc::new(kernel.clone()),
        Arc::new(kernel.clone()),
        registry,
        PageWritePermit::default(),
        managed.iter().map(|id| (*id).into()).collect(),
    )
    .unwrap()
}
fn action(manager: &PanelPlugins, plugin_id: &str, action: PluginActionKind) -> PluginAction {
    let list = manager.plugins().unwrap();
    PluginAction {
        instance: list.instance,
        plugin_id: plugin_id.into(),
        expected_state: list
            .items
            .iter()
            .find(|p| p.id == plugin_id)
            .unwrap()
            .state
            .clone(),
        action,
    }
}
#[tokio::test]
async fn actual_lifecycle_stop_disables_segments_and_restart_restores_saved_preferences() {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = SegmentPreferencePlugin::new().unwrap();
    let inner = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    let id = PluginId::new(SEGMENT_PREFERENCES_PLUGIN_ID).unwrap();
    kernel.start(&id).await.unwrap();
    let scope = SegmentScope {
        channel: "qq".into(),
        session_id: "c2c:u1".into(),
        user_id: "u1".into(),
    };
    let saved = inner
        .update(
            &scope,
            SegmentChange::Patch(SegmentPreference {
                enabled: Some(true),
                max_segments: Some(3),
                pause_percent: Some(0),
            }),
        )
        .unwrap();
    let inspector: Arc<dyn RuntimeInspector> = Arc::new(kernel.clone());
    let preferences = ManagedSegmentPreferences {
        inspector: Arc::downgrade(&inspector),
        inner,
    };
    let panel = manager(&kernel, registry, &[SEGMENT_PREFERENCES_PLUGIN_ID]);
    let stop = action(
        &panel,
        SEGMENT_PREFERENCES_PLUGIN_ID,
        PluginActionKind::Stop,
    );
    let receipt = panel.action(stop.clone()).await.unwrap();
    kernel
        .wait(LifecycleOperationId::new(receipt.id))
        .await
        .unwrap();
    assert_eq!(panel.operations().unwrap()[0].state, "completed");
    assert_eq!(preferences.get(&scope).unwrap().enabled, Some(false));
    assert!(matches!(panel.action(stop).await, Err(PanelError::Stale)));
    assert!(matches!(
        panel.acknowledge(u64::MAX).await,
        Err(PanelError::NotFound)
    ));
    assert!(panel.acknowledge(receipt.id).await.unwrap());
    assert!(panel.operations().unwrap().is_empty());
    let receipt = panel
        .action(action(
            &panel,
            SEGMENT_PREFERENCES_PLUGIN_ID,
            PluginActionKind::Start,
        ))
        .await
        .unwrap();
    kernel
        .wait(LifecycleOperationId::new(receipt.id))
        .await
        .unwrap();
    assert_eq!(preferences.get(&scope).unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

struct TestPlugin {
    manifest: PluginManifest,
    fail: bool,
}
impl Plugin for TestPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, _: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            if self.fail {
                Err(PluginError::State("do-not-expose-provider-details".into()))
            } else {
                Ok(None)
            }
        })
    }
}
#[tokio::test]
async fn cascade_cannot_stop_host_bound_dependents_and_errors_remain_until_acknowledged() {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TestPlugin {
            manifest: PluginManifest::new("demo.base", "0.1.0").unwrap(),
            fail: false,
        }))
        .unwrap();
    let mut dependent = PluginManifest::new("demo.host", "0.1.0").unwrap();
    dependent.dependencies.push(PluginDependency {
        id: PluginId::new("demo.base").unwrap(),
        requirement: None,
    });
    kernel
        .register(Box::new(TestPlugin {
            manifest: dependent,
            fail: false,
        }))
        .unwrap();
    kernel
        .register(Box::new(TestPlugin {
            manifest: PluginManifest::new("demo.failure", "0.1.0").unwrap(),
            fail: true,
        }))
        .unwrap();
    kernel
        .start(&PluginId::new("demo.host").unwrap())
        .await
        .unwrap();
    let panel = manager(&kernel, registry, &["demo.base", "demo.failure"]);
    assert!(matches!(
        panel
            .action(action(&panel, "demo.base", PluginActionKind::Stop))
            .await,
        Err(PanelError::Forbidden)
    ));
    assert_eq!(
        kernel.state(&PluginId::new("demo.host").unwrap()),
        Some(PluginState::Active)
    );
    let receipt = panel
        .action(action(&panel, "demo.failure", PluginActionKind::Start))
        .await
        .unwrap();
    kernel
        .wait(LifecycleOperationId::new(receipt.id))
        .await
        .unwrap();
    for _ in 0..2 {
        let records = panel.operations().unwrap();
        assert_eq!(records[0].state, "failed");
        assert!(
            !serde_json::to_string(&records)
                .unwrap()
                .contains("do-not-expose")
        );
    }
    assert!(panel.acknowledge(receipt.id).await.unwrap());
    kernel.stop_all().await.unwrap();
}
