//! 可信静态插件的 Tokio 宿主，能力后端由组合层注入。

pub mod backends;
mod context;
mod lifecycle;
mod operations;
mod panic_boundary;
mod scope;

use context::KernelHooks;
use eve_plugin_api::{
    EventBus, Logger, PermissionChecker, Plugin, PluginContext, PluginError, PluginId, PluginInfo,
    PluginManifest, PluginRegistry, PluginResult, PluginStatus, RuntimeInspector, ServiceRegistry,
    StateStore, TaskManager,
};
use scope::PluginScope;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

// 保留原导出路径，生命周期定义归入契约层。
pub use eve_plugin_api::PluginState;

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
    pub logger: Arc<dyn Logger>,
}

impl Default for KernelServices {
    fn default() -> Self {
        Self {
            events: Arc::new(backends::SyncEventBus::default()),
            registry: Arc::new(backends::MemoryServiceRegistry::default()),
            state: Arc::new(backends::MemoryStateStore::default()),
            permissions: Arc::new(backends::DeclaredPermissionChecker),
            tasks: Arc::new(backends::TokioTaskManager::default()),
            logger: Arc::new(backends::StderrLogger::default()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelConfig {
    pub task_shutdown_timeout: Duration,
    pub task_abort_timeout: Duration,
    /// 未确认的生命周期记录上限；满时拒绝新操作，不淘汰旧错误。
    pub lifecycle_report_capacity: usize,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            task_shutdown_timeout: Duration::from_secs(5),
            task_abort_timeout: Duration::from_millis(100),
            lifecycle_report_capacity: 128,
        }
    }
}

#[derive(Default)]
struct KernelInner {
    lifecycle: Arc<AsyncMutex<()>>,
    operations: Mutex<operations::OperationRegistry>,
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
        let _registration = self.registration_guard()?;
        let manifest = plugin.manifest().clone();
        manifest.validate()?;
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

    /// 移除已注册但没有运行中资源的插件；不会删除该插件的 StateStore 数据。
    pub fn unregister(&self, id: &PluginId) -> PluginResult<()> {
        let _registration = self.registration_guard()?;
        let slot = self.slot(id)?;
        let state = *slot.state.lock().expect("state lock poisoned");
        if !matches!(
            state,
            PluginState::Registered | PluginState::Stopped | PluginState::Failed
        ) {
            return Err(PluginError::InvalidLifecycle {
                plugin: id.clone(),
                state: format!("cannot unregister plugin in {state:?} state"),
            });
        }
        if slot.scope.lock().expect("scope lock poisoned").is_some() {
            return Err(PluginError::InvalidLifecycle {
                plugin: id.clone(),
                state: "plugin scope is still present".into(),
            });
        }
        let tasks = self.inner.services.tasks.list(id)?;
        if let Some(task) = tasks.iter().find(|task| !task.exited) {
            return Err(PluginError::Task(format!(
                "cannot unregister plugin with an unconfirmed task: {}",
                task.id
            )));
        }
        self.inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .remove(id);
        Ok(())
    }

    /// 注册表变更与生命周期快照串行化，避免运行中的 start/stop 漏掉新插件。
    fn registration_guard(&self) -> PluginResult<AsyncMutexGuard<'_, ()>> {
        self.ensure_lifecycle_healthy()?;
        self.inner
            .lifecycle
            .try_lock()
            .map_err(|_| PluginError::Lifecycle("生命周期操作正在执行，不能注册插件".into()))
    }

    pub fn state(&self, id: &PluginId) -> Option<PluginState> {
        self.slot(id)
            .ok()
            .map(|slot| *slot.state.lock().expect("state lock poisoned"))
    }

    /// 获取已注册插件的状态快照；不等待生命周期锁。
    pub fn plugins(&self) -> PluginResult<Vec<PluginStatus>> {
        let slots = self
            .inner
            .plugins
            .lock()
            .map_err(|_| PluginError::Lifecycle("插件注册表锁中毒".into()))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut statuses = Vec::with_capacity(slots.len());
        for slot in slots {
            statuses.push(PluginStatus {
                info: PluginInfo::from(&slot.manifest),
                state: *slot
                    .state
                    .lock()
                    .map_err(|_| PluginError::Lifecycle("插件状态锁中毒".into()))?,
            });
        }
        statuses.sort_by(|a, b| a.info.id.cmp(&b.info.id));
        Ok(statuses)
    }

    /// 宿主查询任务，包括停止后尚未确认退出的任务。
    pub fn tasks(&self, id: &PluginId) -> PluginResult<Vec<eve_plugin_api::TaskInfo>> {
        self.slot(id)?;
        self.inner.services.tasks.list(id)
    }

    /// 宿主停止日志生产后显式刷新；即使插件停止失败，也可尝试排出已有日志。
    pub fn flush_logs(&self) -> PluginResult<()> {
        self.inner.services.logger.flush()
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

impl PluginRegistry for Kernel {
    fn register(&self, plugin: Box<dyn Plugin>) -> PluginResult<()> {
        Kernel::register(self, plugin)
    }

    fn unregister(&self, id: &PluginId) -> PluginResult<()> {
        Kernel::unregister(self, id)
    }
}

impl RuntimeInspector for Kernel {
    fn plugins(&self) -> PluginResult<Vec<PluginStatus>> {
        Kernel::plugins(self)
    }

    fn plugin_tasks(&self, id: &PluginId) -> PluginResult<Vec<eve_plugin_api::TaskInfo>> {
        self.tasks(id)
    }
}

#[cfg(test)]
mod tests;
