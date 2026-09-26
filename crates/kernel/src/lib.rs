//! An embeddable Tokio host for trusted, statically linked plugins.

mod lifecycle;
mod scope;

use eve_plugin_api::{
    Event, EventHandler, EventId, Plugin, PluginContext, PluginError, PluginId, PluginInfo,
    PluginManifest, PluginResult, RuntimeHooks, ServiceId, cleanup,
};
use scope::PluginScope;
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
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

struct Listener {
    id: u64,
    scope: Weak<PluginScope>,
    handler: EventHandler,
}

struct ServiceEntry {
    owner: PluginId,
    value: Arc<dyn Any + Send + Sync>,
}

#[derive(Default)]
struct KernelInner {
    lifecycle: AsyncMutex<()>,
    plugins: Mutex<HashMap<PluginId, Arc<PluginSlot>>>,
    listeners: Mutex<HashMap<EventId, Vec<Listener>>>,
    services: Mutex<HashMap<ServiceId, ServiceEntry>>,
    state: Mutex<HashMap<(PluginId, String), Vec<u8>>>,
    next_listener: AtomicU64,
}

#[derive(Clone, Default)]
pub struct Kernel {
    inner: Arc<KernelInner>,
}

impl Kernel {
    pub fn new() -> Self {
        Self::default()
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
        });
        PluginContext::new(PluginInfo::from(&slot.manifest), hooks)
    }
}

struct KernelHooks {
    kernel: Weak<KernelInner>,
    scope: Arc<PluginScope>,
}

impl KernelHooks {
    fn kernel(&self) -> PluginResult<Kernel> {
        self.kernel
            .upgrade()
            .map(|inner| Kernel { inner })
            .ok_or_else(|| PluginError::Lifecycle("runtime has been dropped".into()))
    }
}

impl RuntimeHooks for KernelHooks {
    fn emit(&self, _owner: &PluginId, event: Event) -> PluginResult<()> {
        let kernel = self.kernel()?;
        let handlers: Vec<_> = self.scope.access(|| {
            kernel
                .inner
                .listeners
                .lock()
                .expect("listener lock poisoned")
                .get(&event.id)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| (entry.scope.clone(), entry.handler.clone()))
                        .collect()
                })
                .unwrap_or_default()
        })?;
        let mut first_error = None;
        for (scope, handler) in handlers {
            if scope.upgrade().is_some_and(|scope| scope.is_open())
                && let Err(error) = handler(&event)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn subscribe(
        &self,
        _owner: &PluginId,
        event: EventId,
        handler: EventHandler,
    ) -> PluginResult<()> {
        let kernel = self.kernel()?;
        self.scope.resource(|| {
            let listener_id = kernel.inner.next_listener.fetch_add(1, Ordering::Relaxed);
            kernel
                .inner
                .listeners
                .lock()
                .expect("listener lock poisoned")
                .entry(event.clone())
                .or_default()
                .push(Listener {
                    id: listener_id,
                    scope: Arc::downgrade(&self.scope),
                    handler,
                });
            let weak = self.kernel.clone();
            Ok(cleanup(move || async move {
                if let Some(inner) = weak.upgrade() {
                    let mut listeners = inner.listeners.lock().expect("listener lock poisoned");
                    if let Some(entries) = listeners.get_mut(&event) {
                        entries.retain(|entry| entry.id != listener_id);
                        if entries.is_empty() {
                            listeners.remove(&event);
                        }
                    }
                }
                Ok(())
            }))
        })
    }

    fn provide_service(
        &self,
        owner: &PluginId,
        id: ServiceId,
        service: Arc<dyn Any + Send + Sync>,
    ) -> PluginResult<()> {
        let kernel = self.kernel()?;
        self.scope.resource(|| {
            let mut services = kernel.inner.services.lock().expect("service lock poisoned");
            if services.contains_key(&id) {
                return Err(PluginError::ServiceConflict(id));
            }
            services.insert(
                id.clone(),
                ServiceEntry {
                    owner: owner.clone(),
                    value: service,
                },
            );
            let weak = self.kernel.clone();
            Ok(cleanup(move || async move {
                if let Some(inner) = weak.upgrade() {
                    inner
                        .services
                        .lock()
                        .expect("service lock poisoned")
                        .remove(&id);
                }
                Ok(())
            }))
        })
    }

    fn get_service(&self, id: &ServiceId) -> Option<Arc<dyn Any + Send + Sync>> {
        let kernel = self.kernel().ok()?;
        self.scope
            .access(|| {
                let entry = kernel
                    .inner
                    .services
                    .lock()
                    .expect("service lock poisoned")
                    .get(id)
                    .map(|entry| (entry.owner.clone(), entry.value.clone()));
                entry.and_then(|(owner, value)| {
                    matches!(
                        kernel.state(&owner),
                        Some(PluginState::Active | PluginState::Stopping)
                    )
                    .then_some(value)
                })
            })
            .ok()
            .flatten()
    }

    fn state_get(&self, owner: &PluginId, key: &str) -> Option<Vec<u8>> {
        let kernel = self.kernel().ok()?;
        self.scope
            .access(|| {
                kernel
                    .inner
                    .state
                    .lock()
                    .expect("state lock poisoned")
                    .get(&(owner.clone(), key.to_string()))
                    .cloned()
            })
            .ok()
            .flatten()
    }

    fn state_set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let kernel = self.kernel()?;
        self.scope.access(|| {
            kernel
                .inner
                .state
                .lock()
                .expect("state lock poisoned")
                .insert((owner.clone(), key), value);
        })
    }

    fn register_cleanup(
        &self,
        _owner: &PluginId,
        cleanup: eve_plugin_api::Cleanup,
    ) -> PluginResult<()> {
        self.scope.resource(|| Ok(cleanup))
    }
}

#[cfg(test)]
mod tests;
