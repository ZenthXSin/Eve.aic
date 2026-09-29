use eve_kernel::{InMemoryPluginCatalog, Kernel};
use eve_plugin_api::{
    Plugin, PluginCatalog, PluginError, PluginFactory, PluginFuture, PluginManifest, PluginResult,
};
use std::sync::{Arc, Mutex};

struct TestPlugin {
    manifest: PluginManifest,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(
        &mut self,
        _ctx: eve_plugin_api::PluginContext,
    ) -> PluginFuture<'_, Option<eve_plugin_api::Cleanup>> {
        Box::pin(async { Ok(None) })
    }
}

struct TestFactory {
    manifest: PluginManifest,
    created: Arc<Mutex<usize>>,
    drift: bool,
}

struct ReentrantFactory {
    manifest: PluginManifest,
    catalog: Arc<InMemoryPluginCatalog>,
}

impl PluginFactory for ReentrantFactory {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn create(&self) -> PluginResult<Box<dyn Plugin>> {
        let _ = self.catalog.manifests()?;
        Ok(Box::new(TestPlugin {
            manifest: self.manifest.clone(),
        }))
    }
}

impl PluginFactory for TestFactory {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn create(&self) -> PluginResult<Box<dyn Plugin>> {
        *self.created.lock().unwrap() += 1;
        let mut manifest = self.manifest.clone();
        if self.drift {
            manifest.version = eve_plugin_api::Version::new("9.9.9")?;
        }
        Ok(Box::new(TestPlugin { manifest }))
    }
}

fn manifest(id: &str, version: &str) -> PluginManifest {
    PluginManifest::new(id, version).unwrap()
}

#[test]
fn supports_exact_versions_and_same_id_multiple_versions() {
    let catalog = InMemoryPluginCatalog::new();
    let created = Arc::new(Mutex::new(0));
    for version in ["1.0.0", "2.0.0"] {
        catalog
            .register_factory(Arc::new(TestFactory {
                manifest: manifest("demo.catalog", version),
                created: created.clone(),
                drift: false,
            }))
            .unwrap();
    }
    assert_eq!(catalog.manifests().unwrap().len(), 2);
    assert!(
        catalog
            .find(
                &eve_plugin_api::PluginId::new("demo.catalog").unwrap(),
                &eve_plugin_api::Version::new("1.0.0").unwrap()
            )
            .unwrap()
            .is_some()
    );
    assert!(
        catalog
            .create(
                &eve_plugin_api::PluginId::new("demo.catalog").unwrap(),
                &eve_plugin_api::Version::new("2.0.0").unwrap()
            )
            .is_ok()
    );
    assert_eq!(*created.lock().unwrap(), 1);
}

#[test]
fn rejects_manifest_drift_without_removing_factory() {
    let catalog = InMemoryPluginCatalog::new();
    let id = eve_plugin_api::PluginId::new("demo.drift").unwrap();
    let version = eve_plugin_api::Version::new("1.0.0").unwrap();
    catalog
        .register_factory(Arc::new(TestFactory {
            manifest: manifest("demo.drift", "1.0.0"),
            created: Arc::new(Mutex::new(0)),
            drift: true,
        }))
        .unwrap();
    assert!(matches!(
        catalog.create(&id, &version),
        Err(PluginError::FactoryManifestMismatch { .. })
    ));
    assert!(catalog.find(&id, &version).is_ok());
}

#[tokio::test]
async fn created_plugin_can_be_assembled_into_kernel() {
    let catalog = InMemoryPluginCatalog::new();
    let manifest = manifest("demo.kernel", "1.0.0");
    let id = manifest.id.clone();
    let version = manifest.version.clone();
    catalog
        .register_factory(Arc::new(TestFactory {
            manifest,
            created: Arc::new(Mutex::new(0)),
            drift: false,
        }))
        .unwrap();
    let kernel = Kernel::new();
    kernel
        .register(catalog.create(&id, &version).unwrap())
        .unwrap();
    kernel.start(&id).await.unwrap();
    kernel.stop_all().await.unwrap();
}

#[test]
fn factory_callback_can_reenter_catalog_without_holding_catalog_lock() {
    let catalog = Arc::new(InMemoryPluginCatalog::new());
    let manifest = manifest("demo.reentrant", "1.0.0");
    let id = manifest.id.clone();
    let version = manifest.version.clone();
    catalog
        .register_factory(Arc::new(ReentrantFactory {
            manifest,
            catalog: catalog.clone(),
        }))
        .unwrap();
    assert!(catalog.create(&id, &version).is_ok());
}
