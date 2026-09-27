use eve_kernel::{
    Kernel, KernelServices, PluginState,
    backends::{DeclaredPermissionChecker, MemoryServiceRegistry, MemoryStateStore, SyncEventBus},
};
use eve_plugin_api::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct TestPlugin<F> {
    manifest: PluginManifest,
    start: F,
}

impl<F> Plugin for TestPlugin<F>
where
    F: Fn(PluginContext) -> PluginResult<Option<Cleanup>> + Send,
{
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let result = (self.start)(ctx);
        Box::pin(async move { result })
    }
}

fn register(
    kernel: &Kernel,
    id: &str,
    dependencies: &[&str],
    start: impl Fn(PluginContext) -> PluginResult<Option<Cleanup>> + Send + 'static,
) {
    let mut manifest = PluginManifest::new(id, "0.1.0").unwrap();
    manifest.dependencies = dependencies
        .iter()
        .map(|id| PluginDependency {
            id: pid(id),
            requirement: None,
        })
        .collect();
    manifest
        .permissions
        .push(Permission::new("test.allowed").unwrap());
    kernel
        .register(Box::new(TestPlugin { manifest, start }))
        .unwrap();
}

fn pid(id: &str) -> PluginId {
    PluginId::new(id).unwrap()
}

#[tokio::test]
async fn rollback_reports_cleanup_errors_and_still_stops_remaining_dependencies() {
    for start_all in [false, true] {
        let kernel = Kernel::new();
        for id in ["a-provider", "b-provider"] {
            register(&kernel, id, &[], move |_| {
                Ok(Some(cleanup(move || async move {
                    Err(PluginError::Cleanup(id.into()))
                })))
            });
        }
        register(&kernel, "z-failure", &["a-provider", "b-provider"], |_| {
            Err(PluginError::Lifecycle("原始启动错误".into()))
        });
        let result = if start_all {
            kernel.start_all().await
        } else {
            kernel.start(&pid("z-failure")).await
        };
        let PluginError::Rollback { cause, errors } = result.unwrap_err() else {
            panic!("必须返回回滚错误")
        };
        assert!(cause.to_string().contains("原始启动错误"));
        assert_eq!(errors.len(), 2);
        assert!(errors[0].to_string().contains("b-provider"));
        assert!(errors[1].to_string().contains("a-provider"));
        for id in ["a-provider", "b-provider"] {
            assert_eq!(kernel.state(&pid(id)), Some(PluginState::Stopped));
        }
    }
}
fn sid(id: &str) -> ServiceId {
    ServiceId::new(id).unwrap()
}

#[tokio::test]
async fn context_rejects_all_operations_after_stop_and_after_restart() {
    let kernel = Kernel::new();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let contexts = captured.clone();
    register(&kernel, "owner", &[], move |ctx| {
        ctx.state_set("saved", b"value".to_vec())?;
        contexts.lock().unwrap().push(ctx);
        Ok(None)
    });
    kernel.start(&pid("owner")).await.unwrap();
    kernel.stop_all().await.unwrap();
    kernel.start(&pid("owner")).await.unwrap();
    let (old, current) = {
        let contexts = captured.lock().unwrap();
        (contexts[0].clone(), contexts[1].clone())
    };
    assert!(old.emit(Event::new("event", Vec::new()).unwrap()).is_err());
    assert!(
        old.on(EventId::new("event").unwrap(), Arc::new(|_| Ok(())))
            .is_err()
    );
    assert!(old.provide_service(sid("late"), 1_u32).is_err());
    assert!(old.service::<u32>(&sid("late")).is_err());
    assert!(old.state_get("saved").is_err());
    assert!(old.state_set("late", Vec::new()).is_err());
    assert!(
        old.check_permission(&Permission::new("test.allowed").unwrap())
            .is_err()
    );
    assert!(old.cleanup(cleanup(|| async { Ok(()) })).is_err());
    let late_foreground = TaskSpec::new(
        "late",
        TaskMode::Foreground,
        TaskSchedule::Immediate,
        Arc::new(|_| Box::pin(async { Ok(()) })),
    )
    .unwrap();
    assert!(old.run_foreground(late_foreground).await.is_err());
    assert_eq!(current.state_get("saved").unwrap(), Some(b"value".to_vec()));
    current.provide_service(sid("late"), 2_u32).unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn services_check_type_conflicts_and_provider_readiness() {
    let kernel = Kernel::new();
    register(&kernel, "provider", &[], |ctx| {
        ctx.provide_service(sid("number"), 42_u32)?;
        assert!(ctx.service::<u32>(&sid("number"))?.is_none());
        Ok(None)
    });
    register(&kernel, "consumer", &["provider"], |ctx| {
        assert_eq!(*ctx.service::<u32>(&sid("number"))?.unwrap(), 42);
        assert!(matches!(
            ctx.service::<String>(&sid("number")),
            Err(PluginError::ServiceTypeMismatch(_))
        ));
        assert!(ctx.service::<u32>(&sid("missing"))?.is_none());
        assert!(matches!(
            ctx.provide_service(sid("number"), "conflict"),
            Err(PluginError::ServiceConflict(_))
        ));
        assert_eq!(*ctx.service::<u32>(&sid("number"))?.unwrap(), 42);
        Ok(None)
    });
    kernel.start(&pid("consumer")).await.unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn events_are_ordered_reentrant_and_continue_after_listener_errors() {
    let bus = Arc::new(SyncEventBus::default());
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = order.clone();
    let remove_first = bus
        .clone()
        .subscribe(
            EventId::new("outer").unwrap(),
            Arc::new(move |_| {
                first.lock().unwrap().push(1);
                Err(PluginError::Event("预期监听器错误".into()))
            }),
        )
        .unwrap();
    let nested = order.clone();
    let remove_nested = bus
        .clone()
        .subscribe(
            EventId::new("inner").unwrap(),
            Arc::new(move |_| {
                nested.lock().unwrap().push(3);
                Ok(())
            }),
        )
        .unwrap();
    let publisher = Arc::downgrade(&bus);
    let second = order.clone();
    let remove_second = bus
        .clone()
        .subscribe(
            EventId::new("outer").unwrap(),
            Arc::new(move |_| {
                second.lock().unwrap().push(2);
                publisher
                    .upgrade()
                    .unwrap()
                    .emit(Event::new("inner", Vec::new())?)
            }),
        )
        .unwrap();
    assert!(bus.emit(Event::new("outer", Vec::new()).unwrap()).is_err());
    assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
    remove_first().await.unwrap();
    remove_second().await.unwrap();
    remove_nested().await.unwrap();
    bus.emit(Event::new("outer", Vec::new()).unwrap()).unwrap();
    assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
}

#[tokio::test]
async fn stop_provider_cleans_consumers_first_and_physically_removes_listeners() {
    let services = KernelServices::default();
    let events = services.events.clone();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let order = Arc::new(Mutex::new(Vec::new()));
    let provider_order = order.clone();
    register(&kernel, "a-provider", &[], move |ctx| {
        ctx.provide_service(sid("number"), 7_u32)?;
        let order = provider_order.clone();
        Ok(Some(cleanup(move || async move {
            order.lock().unwrap().push("provider");
            Ok(())
        })))
    });
    let consumer_order = order.clone();
    let weak_resource = Arc::new(Mutex::new(None));
    let observed = weak_resource.clone();
    let cleanup_registry = registry.clone();
    register(&kernel, "z-consumer", &["a-provider"], move |ctx| {
        let resource = Arc::new(());
        *observed.lock().unwrap() = Some(Arc::downgrade(&resource));
        let held_context = ctx.clone();
        ctx.on(
            EventId::new("event")?,
            Arc::new(move |_| {
                let _keep = &resource;
                held_context.state_set("event", b"yes".to_vec())
            }),
        )?;
        let registry = cleanup_registry.clone();
        let order = consumer_order.clone();
        Ok(Some(cleanup(move || async move {
            assert!(registry.get(&sid("number"))?.is_some());
            order.lock().unwrap().push("consumer");
            Ok(())
        })))
    });
    kernel.start(&pid("z-consumer")).await.unwrap();
    kernel.stop(&pid("a-provider")).await.unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["consumer", "provider"]);
    assert!(
        weak_resource
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .is_none()
    );
    assert!(registry.get(&sid("number")).unwrap().is_none());
    events
        .emit(Event::new("event", Vec::new()).unwrap())
        .unwrap();
}

#[tokio::test]
async fn state_is_namespaced_and_survives_plugin_restart() {
    let kernel = Kernel::new();
    for id in ["one", "two"] {
        register(&kernel, id, &[], move |ctx| {
            let expected = id.as_bytes().to_vec();
            if let Some(previous) = ctx.state_get("same-key")? {
                assert_eq!(previous, expected);
            }
            ctx.state_set("same-key", expected)?;
            Ok(None)
        });
    }
    kernel.start_all().await.unwrap();
    kernel.stop_all().await.unwrap();
    kernel.start_all().await.unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn permissions_require_exact_manifest_declaration() {
    let kernel = Kernel::new();
    register(&kernel, "permissions", &[], |ctx| {
        ctx.check_permission(&Permission::new("test.allowed")?)?;
        assert!(matches!(
            ctx.check_permission(&Permission::new("test.denied")?),
            Err(PluginError::PermissionDenied { .. })
        ));
        assert!(ctx.check_permission(&Permission::new("test.*")?).is_err());
        Ok(None)
    });
    kernel.start_all().await.unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_start_releases_resources_but_keeps_previously_active_dependencies() {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let weak_resource = Arc::new(Mutex::new(None));
    let observed = weak_resource.clone();
    register(&kernel, "provider", &[], |_| Ok(None));
    register(&kernel, "failure", &["provider"], move |ctx| {
        ctx.provide_service(sid("transient"), 1_u32)?;
        let resource = Arc::new(());
        *observed.lock().unwrap() = Some(Arc::downgrade(&resource));
        ctx.on(
            EventId::new("event")?,
            Arc::new(move |_| {
                let _keep = &resource;
                Ok(())
            }),
        )?;
        Err(PluginError::Lifecycle("预期失败".into()))
    });
    kernel.start(&pid("provider")).await.unwrap();
    assert!(kernel.start(&pid("failure")).await.is_err());
    assert_eq!(kernel.state(&pid("provider")), Some(PluginState::Active));
    assert!(registry.get(&sid("transient")).unwrap().is_none());
    assert!(
        weak_resource
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

// 测试替代后端：记录调用并可拒绝状态读取或权限检查，不修改任何插件接口。
#[derive(Default)]
struct AlternateBackend {
    calls: AtomicUsize,
    state: MemoryStateStore,
    events: Arc<SyncEventBus>,
    registry: Arc<MemoryServiceRegistry>,
    deny: bool,
}

impl EventBus for AlternateBackend {
    fn emit(&self, event: Event) -> PluginResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.emit(event)
    }
    fn subscribe(self: Arc<Self>, event: EventId, handler: EventHandler) -> PluginResult<Cleanup> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.clone().subscribe(event, handler)
    }
}

impl ServiceRegistry for AlternateBackend {
    fn provide(
        self: Arc<Self>,
        owner: PluginId,
        id: ServiceId,
        value: ServiceValue,
    ) -> PluginResult<Cleanup> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.registry.clone().provide(owner, id, value)
    }
    fn get(&self, id: &ServiceId) -> PluginResult<Option<ServiceEntry>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.registry.get(id)
    }
}

impl StateStore for AlternateBackend {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.deny {
            return Err(PluginError::State("后端读取失败".into()));
        }
        self.state.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.state.set(namespace, key, value)
    }
}

impl PermissionChecker for AlternateBackend {
    fn check(&self, manifest: &PluginManifest, permission: &Permission) -> PluginResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.deny {
            return Err(PluginError::PermissionDenied {
                plugin: manifest.id.clone(),
                permission: permission.clone(),
            });
        }
        DeclaredPermissionChecker.check(manifest, permission)
    }
}

#[tokio::test]
async fn host_can_replace_each_backend_without_changing_plugins() {
    let events = Arc::new(AlternateBackend::default());
    let registry = Arc::new(AlternateBackend::default());
    let state = Arc::new(AlternateBackend::default());
    let permissions = Arc::new(AlternateBackend::default());
    let kernel = Kernel::with_services(KernelServices {
        events: events.clone(),
        registry: registry.clone(),
        state: state.clone(),
        permissions: permissions.clone(),
        ..KernelServices::default()
    });
    register(&kernel, "provider", &[], |ctx| {
        ctx.provide_service(sid("service"), 42_u32)?;
        Ok(None)
    });
    register(&kernel, "consumer", &["provider"], |ctx| {
        ctx.check_permission(&Permission::new("test.allowed")?)?;
        assert_eq!(*ctx.service::<u32>(&sid("service"))?.unwrap(), 42);
        let captured = ctx.clone();
        ctx.on(
            EventId::new("event")?,
            Arc::new(move |_| captured.state_set("key", b"value".to_vec())),
        )?;
        ctx.emit(Event::new("event", Vec::new())?)?;
        assert_eq!(ctx.state_get("key")?, Some(b"value".to_vec()));
        Ok(None)
    });
    kernel.start(&pid("consumer")).await.unwrap();
    for backend in [&events, &registry, &state, &permissions] {
        assert!(backend.calls.load(Ordering::SeqCst) > 0);
    }
    kernel.stop_all().await.unwrap();
    assert!(
        ServiceRegistry::get(registry.as_ref(), &sid("service"))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn backend_errors_propagate_and_trigger_startup_rollback() {
    let backend = Arc::new(AlternateBackend {
        deny: true,
        ..Default::default()
    });
    let kernel = Kernel::with_services(KernelServices {
        state: backend.clone(),
        permissions: backend,
        ..Default::default()
    });
    register(&kernel, "state-error", &[], |ctx| {
        ctx.state_get("key")?;
        Ok(None)
    });
    register(&kernel, "permission-error", &[], |ctx| {
        ctx.check_permission(&Permission::new("test.allowed")?)?;
        Ok(None)
    });
    let state_error = kernel.start(&pid("state-error")).await.unwrap_err();
    assert!(state_error.to_string().contains("后端读取失败"));
    let permission_error = kernel.start(&pid("permission-error")).await.unwrap_err();
    assert!(permission_error.to_string().contains("test.allowed"));
    assert_eq!(kernel.state(&pid("state-error")), Some(PluginState::Failed));
    assert_eq!(
        kernel.state(&pid("permission-error")),
        Some(PluginState::Failed)
    );
}

#[tokio::test]
async fn dropping_kernel_releases_listeners_that_capture_contexts() {
    let kernel = Kernel::new();
    let weak_resource = Arc::new(Mutex::new(None));
    let observed = weak_resource.clone();
    register(&kernel, "owner", &[], move |ctx| {
        let resource = Arc::new(());
        *observed.lock().unwrap() = Some(Arc::downgrade(&resource));
        let captured = ctx.clone();
        ctx.on(
            EventId::new("event")?,
            Arc::new(move |_| {
                let _keep = &resource;
                captured.state_set("key", b"value".to_vec())
            }),
        )?;
        Ok(None)
    });
    kernel.start_all().await.unwrap();
    drop(kernel);
    assert!(
        weak_resource
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .is_none()
    );
}
