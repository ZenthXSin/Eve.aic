use crate::{Kernel, KernelInner, PluginState, scope::PluginScope};
use eve_plugin_api::{
    Cleanup, Event, EventHandler, EventId, Permission, PluginError, PluginFuture, PluginId,
    PluginManifest, PluginResult, RuntimeHooks, ServiceId, ServiceValue, TaskId, TaskInfo,
    TaskRunReport, TaskSpec,
};
use std::sync::{Arc, Weak};

pub(crate) struct KernelHooks {
    pub(crate) kernel: Weak<KernelInner>,
    pub(crate) scope: Arc<PluginScope>,
    pub(crate) manifest: PluginManifest,
}

impl KernelHooks {
    fn kernel(&self) -> PluginResult<Kernel> {
        self.kernel
            .upgrade()
            .map(|inner| Kernel { inner })
            .ok_or_else(|| PluginError::Lifecycle("Runtime 已释放".into()))
    }
}

impl RuntimeHooks for KernelHooks {
    fn emit(&self, _owner: &PluginId, event: Event) -> PluginResult<()> {
        let kernel = self.kernel()?;
        // 先检查准入，释放 Scope 锁后再分发，避免回调使用 Context 时死锁。
        // 停止不会强制中断已经进入的同步回调。
        self.scope.access(|| ())?;
        kernel.inner.services.events.emit(event)
    }

    fn subscribe(
        &self,
        _owner: &PluginId,
        event: EventId,
        handler: EventHandler,
    ) -> PluginResult<()> {
        let kernel = self.kernel()?;
        let scope = Arc::downgrade(&self.scope);
        let guarded: EventHandler = Arc::new(move |event| {
            if scope.upgrade().is_some_and(|scope| scope.is_open()) {
                handler(event)
            } else {
                Ok(())
            }
        });
        self.scope.resource(|| {
            kernel
                .inner
                .services
                .events
                .clone()
                .subscribe(event, guarded)
        })
    }

    fn provide_service(
        &self,
        _owner: &PluginId,
        id: ServiceId,
        service: ServiceValue,
    ) -> PluginResult<()> {
        let kernel = self.kernel()?;
        self.scope.resource(|| {
            kernel
                .inner
                .services
                .registry
                .clone()
                .provide(self.manifest.id.clone(), id, service)
        })
    }

    fn get_service(&self, id: &ServiceId) -> PluginResult<Option<ServiceValue>> {
        let kernel = self.kernel()?;
        self.scope.access(|| {
            Ok(kernel.inner.services.registry.get(id)?.and_then(|entry| {
                matches!(kernel.state(&entry.owner), Some(PluginState::Active))
                    .then_some(entry.value)
            }))
        })?
    }

    fn state_get(&self, _owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        let kernel = self.kernel()?;
        self.scope
            .access(|| kernel.inner.services.state.get(&self.manifest.id, key))?
    }

    fn state_set(&self, _owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let kernel = self.kernel()?;
        self.scope.access(|| {
            kernel
                .inner
                .services
                .state
                .set(&self.manifest.id, key, value)
        })?
    }

    fn check_permission(&self, permission: &Permission) -> PluginResult<()> {
        let kernel = self.kernel()?;
        self.scope.access(|| {
            kernel
                .inner
                .services
                .permissions
                .check(&self.manifest, permission)
        })?
    }

    fn spawn_task(&self, _owner: &PluginId, spec: TaskSpec) -> PluginResult<TaskId> {
        let kernel = self.kernel()?;
        self.scope.access(|| {
            kernel
                .inner
                .services
                .tasks
                .spawn(self.manifest.id.clone(), spec)
        })?
    }

    fn run_foreground(
        &self,
        _owner: &PluginId,
        spec: TaskSpec,
    ) -> PluginFuture<'static, TaskRunReport> {
        if let Err(error) = self.scope.access(|| ()) {
            return Box::pin(async move { Err(error) });
        }
        match self.kernel() {
            Ok(kernel) => kernel
                .inner
                .services
                .tasks
                .clone()
                .run_foreground(self.manifest.id.clone(), spec),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn list_tasks(&self, _owner: &PluginId) -> PluginResult<Vec<TaskInfo>> {
        let kernel = self.kernel()?;
        self.scope
            .access(|| kernel.inner.services.tasks.list(&self.manifest.id))?
    }

    fn register_cleanup(&self, _owner: &PluginId, cleanup: Cleanup) -> PluginResult<()> {
        self.kernel()?;
        self.scope.register(cleanup)
    }
}
