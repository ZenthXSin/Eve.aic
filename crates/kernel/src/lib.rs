//! The first Eve.aic runtime kernel.

use eve_plugin_api::{
    Cleanup, Event, EventHandler, EventId, Plugin, PluginContext, PluginError, PluginId,
    PluginInfo, PluginManifest, PluginResult, RuntimeHooks, ServiceId,
};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The lifecycle state of a registered plugin.
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

/// A resource collection owned by one plugin.
#[derive(Default)]
struct PluginScope {
    cleanups: Mutex<Vec<Cleanup>>,
}

impl PluginScope {
    fn register(&self, cleanup: Cleanup) {
        self.cleanups
            .lock()
            .expect("scope lock poisoned")
            .push(cleanup);
    }

    fn cleanup(&self) -> PluginResult<()> {
        let cleanups = std::mem::take(&mut *self.cleanups.lock().expect("scope lock poisoned"));
        let mut errors = Vec::new();
        for cleanup in cleanups.into_iter().rev() {
            if let Err(error) = cleanup() {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(PluginError::Cleanup(errors.join("; ")))
        }
    }
}

struct PluginSlot {
    manifest: PluginManifest,
    plugin: Mutex<Box<dyn Plugin>>,
    state: Mutex<PluginState>,
    scope: Arc<PluginScope>,
}

struct Listener {
    id: u64,
    handler: EventHandler,
}

struct ServiceEntry {
    owner: PluginId,
    value: Arc<dyn Any + Send + Sync>,
}

struct KernelInner {
    plugins: Mutex<HashMap<PluginId, Arc<PluginSlot>>>,
    listeners: Mutex<HashMap<EventId, Vec<Listener>>>,
    services: Mutex<HashMap<ServiceId, ServiceEntry>>,
    state: Mutex<HashMap<(PluginId, String), Vec<u8>>>,
    next_listener: AtomicU64,
}

impl Default for KernelInner {
    fn default() -> Self {
        Self {
            plugins: Mutex::new(HashMap::new()),
            listeners: Mutex::new(HashMap::new()),
            services: Mutex::new(HashMap::new()),
            state: Mutex::new(HashMap::new()),
            next_listener: AtomicU64::new(1),
        }
    }
}

/// A small, embeddable plugin runtime.
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
                plugin: Mutex::new(plugin),
                state: Mutex::new(PluginState::Registered),
                scope: Arc::new(PluginScope::default()),
            }),
        );
        Ok(())
    }

    pub fn state(&self, id: &PluginId) -> Option<PluginState> {
        self.inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .get(id)
            .map(|slot| *slot.state.lock().expect("state lock poisoned"))
    }

    pub fn start_all(&self) -> PluginResult<()> {
        let ids: Vec<_> = self
            .inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.start(&id)?;
        }
        Ok(())
    }

    pub fn start(&self, id: &PluginId) -> PluginResult<()> {
        let mut path = HashSet::new();
        let mut started = Vec::new();
        match self.start_with_path(id, &mut path, &mut started) {
            Ok(()) => Ok(()),
            Err(error) => {
                for started_id in started.into_iter().rev() {
                    let _ = self.stop(&started_id);
                }
                Err(error)
            }
        }
    }

    fn start_with_path(
        &self,
        id: &PluginId,
        path: &mut HashSet<PluginId>,
        started: &mut Vec<PluginId>,
    ) -> PluginResult<()> {
        if !path.insert(id.clone()) {
            return Err(PluginError::DependencyCycle(id.clone()));
        }

        let slot = self.slot(id)?;
        let dependencies = slot.manifest.dependencies.clone();
        for dependency in dependencies {
            if !self.has(&dependency.id) {
                path.remove(id);
                return Err(PluginError::MissingDependency {
                    plugin: id.clone(),
                    dependency: dependency.id,
                });
            }
            self.start_with_path(&dependency.id, path, started)?;
        }
        path.remove(id);

        let mut state = slot.state.lock().expect("state lock poisoned");
        match *state {
            PluginState::Active => return Ok(()),
            PluginState::Starting | PluginState::Stopping => {
                return Err(PluginError::InvalidLifecycle {
                    plugin: id.clone(),
                    state: format!("{:?}", *state),
                });
            }
            PluginState::Registered | PluginState::WaitingDependencies | PluginState::Stopped => {}
            PluginState::Failed => {
                return Err(PluginError::InvalidLifecycle {
                    plugin: id.clone(),
                    state: "Failed; restart is not automatic".to_string(),
                });
            }
        }
        *state = PluginState::Starting;
        drop(state);

        let info = PluginInfo::from(&slot.manifest);
        let hooks: Arc<dyn RuntimeHooks> = Arc::new(KernelHooks {
            kernel: self.clone(),
            scope: slot.scope.clone(),
        });
        let result = slot
            .plugin
            .lock()
            .expect("plugin lock poisoned")
            .start(PluginContext::new(info, hooks));

        let mut state = slot.state.lock().expect("state lock poisoned");
        match result {
            Ok(explicit_cleanup) => {
                if let Some(cleanup) = explicit_cleanup {
                    slot.scope.register(cleanup);
                }
                *state = PluginState::Active;
                started.push(id.clone());
                Ok(())
            }
            Err(error) => {
                *state = PluginState::Failed;
                drop(state);
                let cleanup_result = slot.scope.cleanup();
                if let Err(cleanup_error) = cleanup_result {
                    return Err(PluginError::PluginFailed {
                        plugin: id.clone(),
                        message: format!("{error}; {cleanup_error}"),
                    });
                }
                Err(PluginError::PluginFailed {
                    plugin: id.clone(),
                    message: error.to_string(),
                })
            }
        }
    }

    pub fn stop_all(&self) -> PluginResult<()> {
        let ids: Vec<_> = self
            .inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .keys()
            .cloned()
            .collect();
        let mut first_error = None;
        for id in ids {
            if let Err(error) = self.stop(&id) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn stop(&self, id: &PluginId) -> PluginResult<()> {
        let slot = self.slot(id)?;
        let consumers: Vec<_> = self
            .inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .iter()
            .filter_map(|(candidate_id, candidate)| {
                let active =
                    *candidate.state.lock().expect("state lock poisoned") == PluginState::Active;
                let depends = candidate
                    .manifest
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.id == *id);
                active
                    .then_some(depends.then(|| candidate_id.clone()))
                    .flatten()
            })
            .collect();

        let mut first_error = None;
        for consumer in consumers {
            if let Err(error) = self.stop(&consumer) {
                first_error.get_or_insert(error);
            }
        }

        let mut state = slot.state.lock().expect("state lock poisoned");
        if *state != PluginState::Active {
            return first_error.map_or(Ok(()), Err);
        }
        *state = PluginState::Stopping;
        drop(state);

        let cleanup_result = slot.scope.cleanup();
        self.remove_owned_services(id);
        *slot.state.lock().expect("state lock poisoned") = PluginState::Stopped;
        if let Err(error) = cleanup_result {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn has(&self, id: &PluginId) -> bool {
        self.inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .contains_key(id)
    }

    fn slot(&self, id: &PluginId) -> PluginResult<Arc<PluginSlot>> {
        self.inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| PluginError::MissingDependency {
                plugin: id.clone(),
                dependency: id.clone(),
            })
    }

    fn remove_owned_services(&self, owner: &PluginId) {
        self.inner
            .services
            .lock()
            .expect("service lock poisoned")
            .retain(|_, service| service.owner != *owner);
    }

    fn remove_listener(&self, event: &EventId, listener_id: u64) {
        let mut listeners = self.inner.listeners.lock().expect("listener lock poisoned");
        if let Some(entries) = listeners.get_mut(event) {
            entries.retain(|entry| entry.id != listener_id);
            if entries.is_empty() {
                listeners.remove(event);
            }
        }
    }
}

struct KernelHooks {
    kernel: Kernel,
    scope: Arc<PluginScope>,
}

impl RuntimeHooks for KernelHooks {
    fn emit(&self, _owner: &PluginId, event: Event) -> PluginResult<()> {
        let handlers: Vec<_> = self
            .kernel
            .inner
            .listeners
            .lock()
            .expect("listener lock poisoned")
            .get(&event.id)
            .map(|entries| entries.iter().map(|entry| entry.handler.clone()).collect())
            .unwrap_or_default();
        let mut first_error = None;
        for handler in handlers {
            if let Err(error) = handler(&event) {
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
        let listener_id = self
            .kernel
            .inner
            .next_listener
            .fetch_add(1, Ordering::Relaxed);
        self.kernel
            .inner
            .listeners
            .lock()
            .expect("listener lock poisoned")
            .entry(event.clone())
            .or_default()
            .push(Listener {
                id: listener_id,
                handler,
            });
        let kernel = self.kernel.clone();
        self.scope.register(Box::new(move || {
            kernel.remove_listener(&event, listener_id);
            Ok(())
        }));
        Ok(())
    }

    fn provide_service(
        &self,
        owner: &PluginId,
        id: ServiceId,
        service: Arc<dyn Any + Send + Sync>,
    ) -> PluginResult<()> {
        let mut services = self
            .kernel
            .inner
            .services
            .lock()
            .expect("service lock poisoned");
        if services.contains_key(&id) {
            return Err(PluginError::ServiceConflict(id));
        }
        services.insert(
            id,
            ServiceEntry {
                owner: owner.clone(),
                value: service,
            },
        );
        Ok(())
    }

    fn get_service(&self, id: &ServiceId) -> Option<Arc<dyn Any + Send + Sync>> {
        self.kernel
            .inner
            .services
            .lock()
            .expect("service lock poisoned")
            .get(id)
            .map(|entry| entry.value.clone())
    }

    fn state_get(&self, owner: &PluginId, key: &str) -> Option<Vec<u8>> {
        self.kernel
            .inner
            .state
            .lock()
            .expect("state lock poisoned")
            .get(&(owner.clone(), key.to_string()))
            .cloned()
    }

    fn state_set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.kernel
            .inner
            .state
            .lock()
            .expect("state lock poisoned")
            .insert((owner.clone(), key), value);
        Ok(())
    }

    fn register_cleanup(&self, _owner: &PluginId, cleanup: Cleanup) -> PluginResult<()> {
        self.scope.register(cleanup);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_plugin_api::{Event, EventHandler, PluginDependency};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Producer {
        manifest: PluginManifest,
    }

    impl Producer {
        fn new() -> Self {
            Self {
                manifest: PluginManifest::new("producer", "0.1.0").unwrap(),
            }
        }
    }

    impl Plugin for Producer {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn start(&mut self, ctx: PluginContext) -> PluginResult<Option<Cleanup>> {
            ctx.provide_service(ServiceId::new("number")?, 42_u32)?;
            ctx.state_set("started", b"yes".to_vec())?;
            ctx.emit(Event::new("ready", b"producer".to_vec())?)?;
            Ok(Some(Box::new(|| Ok(()))))
        }
    }

    struct Consumer {
        manifest: PluginManifest,
        seen: Arc<AtomicUsize>,
    }

    impl Consumer {
        fn new(seen: Arc<AtomicUsize>) -> Self {
            let mut manifest = PluginManifest::new("consumer", "0.1.0").unwrap();
            manifest.dependencies.push(PluginDependency {
                id: PluginId::new("producer").unwrap(),
                requirement: None,
            });
            Self { manifest, seen }
        }
    }

    impl Plugin for Consumer {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn start(&mut self, ctx: PluginContext) -> PluginResult<Option<Cleanup>> {
            let number = ctx
                .service::<u32>(&ServiceId::new("number")?)
                .ok_or_else(|| PluginError::ServiceNotFound(ServiceId::new("number").unwrap()))?;
            assert_eq!(*number, 42);
            let seen = self.seen.clone();
            let handler: EventHandler = Arc::new(move |event| {
                assert_eq!(event.payload, b"consumer".to_vec());
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            ctx.on(EventId::new("consumer-ready")?, handler)?;
            Ok(None)
        }
    }

    #[test]
    fn starts_dependencies_and_restores_plugin_state() {
        let kernel = Kernel::new();
        let seen = Arc::new(AtomicUsize::new(0));
        kernel.register(Box::new(Producer::new())).unwrap();
        kernel
            .register(Box::new(Consumer::new(seen.clone())))
            .unwrap();
        let consumer = PluginId::new("consumer").unwrap();
        let producer = PluginId::new("producer").unwrap();

        kernel.start(&consumer).unwrap();
        assert_eq!(kernel.state(&producer), Some(PluginState::Active));
        assert_eq!(kernel.state(&consumer), Some(PluginState::Active));

        let hooks = KernelHooks {
            kernel: kernel.clone(),
            scope: kernel
                .inner
                .plugins
                .lock()
                .unwrap()
                .get(&consumer)
                .unwrap()
                .scope
                .clone(),
        };
        hooks
            .emit(
                &consumer,
                Event::new("consumer-ready", b"consumer".to_vec()).unwrap(),
            )
            .unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        assert_eq!(hooks.state_get(&producer, "started"), Some(b"yes".to_vec()));
        kernel.stop_all().unwrap();
        assert_eq!(kernel.state(&consumer), Some(PluginState::Stopped));
        assert_eq!(kernel.state(&producer), Some(PluginState::Stopped));
    }

    #[test]
    fn cleanup_runs_in_reverse_order_and_continues_after_errors() {
        let kernel = Kernel::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        struct CleanupPlugin {
            manifest: PluginManifest,
            order: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Plugin for CleanupPlugin {
            fn manifest(&self) -> &PluginManifest {
                &self.manifest
            }
            fn start(&mut self, ctx: PluginContext) -> PluginResult<Option<Cleanup>> {
                let first = self.order.clone();
                ctx.cleanup(Box::new(move || {
                    first.lock().unwrap().push("first");
                    Err(PluginError::Cleanup("expected".to_string()))
                }))?;
                let second = self.order.clone();
                ctx.cleanup(Box::new(move || {
                    second.lock().unwrap().push("second");
                    Ok(())
                }))?;
                Ok(None)
            }
        }
        let manifest = PluginManifest::new("cleanup", "0.1.0").unwrap();
        kernel
            .register(Box::new(CleanupPlugin {
                manifest,
                order: order.clone(),
            }))
            .unwrap();
        let id = PluginId::new("cleanup").unwrap();
        kernel.start(&id).unwrap();
        assert!(kernel.stop(&id).is_err());
        assert_eq!(*order.lock().unwrap(), vec!["second", "first"]);
    }

    #[test]
    fn failed_start_rolls_back_dependencies_started_for_the_request() {
        struct FailingConsumer {
            manifest: PluginManifest,
        }
        impl Plugin for FailingConsumer {
            fn manifest(&self) -> &PluginManifest {
                &self.manifest
            }

            fn start(&mut self, _ctx: PluginContext) -> PluginResult<Option<Cleanup>> {
                Err(PluginError::PluginFailed {
                    plugin: self.manifest.id.clone(),
                    message: "expected failure".to_string(),
                })
            }
        }

        let kernel = Kernel::new();
        kernel.register(Box::new(Producer::new())).unwrap();
        let mut manifest = PluginManifest::new("failing", "0.1.0").unwrap();
        manifest.dependencies.push(PluginDependency {
            id: PluginId::new("producer").unwrap(),
            requirement: None,
        });
        kernel
            .register(Box::new(FailingConsumer { manifest }))
            .unwrap();

        let failing = PluginId::new("failing").unwrap();
        let producer = PluginId::new("producer").unwrap();
        assert!(kernel.start(&failing).is_err());
        assert_eq!(kernel.state(&failing), Some(PluginState::Failed));
        assert_eq!(kernel.state(&producer), Some(PluginState::Stopped));
    }
}
