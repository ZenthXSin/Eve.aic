use super::*;
use eve_plugin_api::{Cleanup, Event, PluginDependency, PluginFuture, cleanup};
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

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.provide_service(ServiceId::new("number")?, 42_u32)?;
            ctx.state_set("started", b"yes".to_vec())?;
            Ok(Some(cleanup(|| async { Ok(()) })))
        })
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

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
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
        })
    }
}

#[tokio::test]
async fn starts_dependencies_and_exposes_state_service_and_events() {
    let kernel = Kernel::new();
    let seen = Arc::new(AtomicUsize::new(0));
    kernel.register(Box::new(Producer::new())).unwrap();
    kernel
        .register(Box::new(Consumer::new(seen.clone())))
        .unwrap();
    let consumer = PluginId::new("consumer").unwrap();
    let producer = PluginId::new("producer").unwrap();

    kernel.start(&consumer).await.unwrap();
    assert_eq!(kernel.state(&producer), Some(PluginState::Active));
    assert_eq!(kernel.state(&consumer), Some(PluginState::Active));

    let scope = kernel
        .inner
        .plugins
        .lock()
        .unwrap()
        .get(&consumer)
        .unwrap()
        .scope
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let hooks = KernelHooks {
        kernel: Arc::downgrade(&kernel.inner),
        scope,
    };
    hooks
        .emit(
            &consumer,
            Event::new("consumer-ready", b"consumer".to_vec()).unwrap(),
        )
        .unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    assert_eq!(hooks.state_get(&producer, "started"), Some(b"yes".to_vec()));
    kernel.stop_all().await.unwrap();
    assert_eq!(kernel.state(&consumer), Some(PluginState::Stopped));
    assert_eq!(kernel.state(&producer), Some(PluginState::Stopped));
}

#[tokio::test]
async fn async_cleanup_runs_in_reverse_order_and_continues_after_errors() {
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

        fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
            Box::pin(async move {
                let first = self.order.clone();
                ctx.cleanup(cleanup(move || async move {
                    first.lock().unwrap().push("first");
                    Err(PluginError::Cleanup("expected".to_string()))
                }))?;
                let second = self.order.clone();
                ctx.cleanup(cleanup(move || async move {
                    second.lock().unwrap().push("second");
                    Ok(())
                }))?;
                Ok(None)
            })
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
    kernel.start(&id).await.unwrap();
    assert!(kernel.stop(&id).await.is_err());
    assert_eq!(*order.lock().unwrap(), vec!["second", "first"]);
    assert_eq!(kernel.state(&id), Some(PluginState::Stopped));
}

#[tokio::test]
async fn failed_start_rolls_back_dependencies_started_for_the_request() {
    struct FailingConsumer {
        manifest: PluginManifest,
    }

    impl Plugin for FailingConsumer {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn start(&mut self, _ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
            let error = PluginError::PluginFailed {
                plugin: self.manifest.id.clone(),
                message: "expected failure".to_string(),
            };
            Box::pin(async move { Err(error) })
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
    assert!(kernel.start(&failing).await.is_err());
    assert_eq!(kernel.state(&failing), Some(PluginState::Failed));
    assert_eq!(kernel.state(&producer), Some(PluginState::Stopped));
}
