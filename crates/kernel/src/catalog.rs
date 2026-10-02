use eve_plugin_api::{
    Plugin, PluginCatalog, PluginError, PluginFactory, PluginId, PluginManifest, PluginResult,
    Version,
};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

type CatalogKey = (PluginId, Version);

struct CatalogEntry {
    factory: Arc<dyn PluginFactory>,
    manifest: PluginManifest,
}

/// 默认的进程内静态插件目录。
///
/// 目录只保存可信工厂和清单快照；工厂不持久化，实例的启动、依赖解析和状态
/// 恢复仍由组合层与 Kernel 显式负责。目录锁不会跨越用户工厂回调。
#[derive(Default)]
pub struct InMemoryPluginCatalog {
    factories: Mutex<BTreeMap<CatalogKey, CatalogEntry>>,
}

impl InMemoryPluginCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    fn snapshot(factory: &Arc<dyn PluginFactory>) -> PluginResult<PluginManifest> {
        let manifest =
            catch_unwind(AssertUnwindSafe(|| factory.manifest().clone())).map_err(factory_panic)?;
        manifest.validate()?;
        Ok(manifest)
    }
}

impl PluginCatalog for InMemoryPluginCatalog {
    fn register_factory(&self, factory: Arc<dyn PluginFactory>) -> PluginResult<()> {
        let manifest = Self::snapshot(&factory)?;
        let key = (manifest.id.clone(), manifest.version.clone());
        let mut factories = self
            .factories
            .lock()
            .map_err(|_| PluginError::Lifecycle("插件目录锁中毒".into()))?;
        if factories.contains_key(&key) {
            return Err(PluginError::DuplicatePlugin(manifest.id));
        }
        factories.insert(key, CatalogEntry { factory, manifest });
        Ok(())
    }

    fn manifests(&self) -> PluginResult<Vec<PluginManifest>> {
        let factories = self
            .factories
            .lock()
            .map_err(|_| PluginError::Lifecycle("插件目录锁中毒".into()))?;
        Ok(factories
            .values()
            .map(|entry| entry.manifest.clone())
            .collect())
    }

    fn find(&self, id: &PluginId, version: &Version) -> PluginResult<Option<PluginManifest>> {
        let factories = self
            .factories
            .lock()
            .map_err(|_| PluginError::Lifecycle("插件目录锁中毒".into()))?;
        let entry = factories
            .get(&(id.clone(), version.clone()))
            .map(|entry| (entry.factory.clone(), entry.manifest.clone()));
        drop(factories);
        entry
            .map(|(factory, manifest)| {
                let current = Self::snapshot(&factory)?;
                if current != manifest {
                    return Err(PluginError::FactoryManifestMismatch {
                        expected: Box::new(manifest),
                        found: Box::new(current),
                    });
                }
                Ok(manifest)
            })
            .transpose()
    }

    fn create(&self, id: &PluginId, version: &Version) -> PluginResult<Box<dyn Plugin>> {
        let (factory, expected) = {
            let factories = self
                .factories
                .lock()
                .map_err(|_| PluginError::Lifecycle("插件目录锁中毒".into()))?;
            factories
                .get(&(id.clone(), version.clone()))
                .map(|entry| (entry.factory.clone(), entry.manifest.clone()))
        }
        .ok_or_else(|| PluginError::FactoryNotFound {
            id: id.clone(),
            version: version.clone(),
        })?;

        let current = Self::snapshot(&factory)?;
        if current != expected {
            return Err(PluginError::FactoryManifestMismatch {
                expected: Box::new(expected),
                found: Box::new(current),
            });
        }
        let plugin =
            catch_unwind(AssertUnwindSafe(|| factory.create())).map_err(factory_panic)??;
        let found =
            catch_unwind(AssertUnwindSafe(|| plugin.manifest().clone())).map_err(factory_panic)?;
        found.validate()?;
        if found != expected {
            return Err(PluginError::FactoryManifestMismatch {
                expected: Box::new(expected),
                found: Box::new(found),
            });
        }
        Ok(plugin)
    }
}

fn factory_panic(payload: Box<dyn std::any::Any + Send>) -> PluginError {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("非字符串 panic 载荷");
    PluginError::FactoryCreate(format!("工厂回调发生 panic：{message}"))
}
