//! 可信静态插件的 Tokio 宿主，能力后端由组合层注入。

pub mod backends;
mod context;
mod lifecycle;
mod scope;

use context::KernelHooks;
use eve_plugin_api::{
    EventBus, PermissionChecker, Plugin, PluginContext, PluginError, PluginId, PluginInfo,
    PluginManifest, PluginResult, ServiceRegistry, StateStore, TaskManager,
};
use scope::PluginScope;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginState {
    Registered,
    WaitingDependencies,
    Starting,
    Active,
    Stopping,
    Stopped,
    Failed,
}

struct PluginSlot {
    manifest: PluginManifest,
    plugin: AsyncMutex<Box<dyn Plugin>>,
    state: Mutex<PluginState>,
    scope: Mutex<Option<Arc<PluginScope>>>,
}

/// 组合层按能力选择后端；每个 Kernel 默认拥有独立的内存后端。
/// Event Bus / Service Registry 应属于同一个 Runtime，避免插件 ID 跨 Runtime 冲突。
pub struct KernelServices {
    pub events: Arc<dyn EventBus>,
    pub registry: Arc<dyn ServiceRegistry>,
    pub state: Arc<dyn StateStore>,
    pub permissions: Arc<dyn PermissionChecker>,
    pub tasks: Arc<dyn TaskManager>,
}

impl Default for KernelServices {
    fn default() -> Self {
        Self {
            events: Arc::new(backends::SyncEventBus::default()),
            registry: Arc::new(backends::MemoryServiceRegistry::default()),
            state: Arc::new(backends::MemoryStateStore::default()),
            permissions: Arc::new(backends::DeclaredPermissionChecker),
            tasks: Arc::new(backends::TokioTaskManager::default()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelConfig {
    pub task_shutdown_timeout: Duration,
    pub task_abort_timeout: Duration,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            task_shutdown_timeout: Duration::from_secs(5),
            task_abort_timeout: Duration::from_millis(100),
        }
    }
}

#[derive(Default)]
struct KernelInner {
    lifecycle: AsyncMutex<()>,
    plugins: Mutex<HashMap<PluginId, Arc<PluginSlot>>>,
    services: KernelServices,
    config: KernelConfig,
}

#[derive(Clone, Default)]
pub struct Kernel {
    inner: Arc<KernelInner>,
}

impl Kernel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_services(services: KernelServices) -> Self {
        Self::with_services_and_config(services, KernelConfig::default())
    }

    pub fn with_services_and_config(services: KernelServices, config: KernelConfig) -> Self {
        Self {
            inner: Arc::new(KernelInner {
                services,
                config,
                ..KernelInner::default()
            }),
        }
    }

    pub fn register(&self, plugin: Box<dyn Plugin>) -> PluginResult<()> {
        let manifest = plugin.manifest().clone();
        let id = manifest.id.clone();
        let mut plugins = self.inner.plugins.lock().expect("plugin lock poisoned");
        if plugins.contains_key(&id) {
            return Err(PluginError::DuplicatePlugin(id));
        }
        plugins.insert(
            id,
            Arc::new(PluginSlot {
                manifest,
                plugin: AsyncMutex::new(plugin),
                state: Mutex::new(PluginState::Registered),
                scope: Mutex::new(None),
            }),
        );
        Ok(())
    }

    pub fn state(&self, id: &PluginId) -> Option<PluginState> {
        self.slot(id)
            .ok()
            .map(|slot| *slot.state.lock().expect("state lock poisoned"))
    }

    /// 宿主查询任务，包括停止后尚未确认退出的任务。
    pub fn tasks(&self, id: &PluginId) -> PluginResult<Vec<eve_plugin_api::TaskInfo>> {
        self.slot(id)?;
        self.inner.services.tasks.list(id)
    }

    fn has(&self, id: &PluginId) -> bool {
        self.inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .contains_key(id)
    }

    fn plugin_ids(&self) -> Vec<PluginId> {
        let mut ids: Vec<_> = self
            .inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    fn slot(&self, id: &PluginId) -> PluginResult<Arc<PluginSlot>> {
        self.inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| PluginError::InvalidLifecycle {
                plugin: id.clone(),
                state: "not registered".into(),
            })
    }

    fn context(&self, slot: &PluginSlot, scope: Arc<PluginScope>) -> PluginContext {
        let hooks = Arc::new(KernelHooks {
            kernel: Arc::downgrade(&self.inner),
            scope,
            manifest: slot.manifest.clone(),
        });
        PluginContext::new(PluginInfo::from(&slot.manifest), hooks)
    }
}

#[cfg(test)]
mod tests;
