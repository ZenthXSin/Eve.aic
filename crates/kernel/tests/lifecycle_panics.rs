use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::*;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicUsize, Ordering},
};

fn pid(id: &str) -> PluginId {
    PluginId::new(id).unwrap()
}

struct TestPlugin<F> {
    manifest: PluginManifest,
    start: F,
}

impl<F> Plugin for TestPlugin<F>
where
    F: Fn(PluginContext) -> PluginFuture<'static, Option<Cleanup>> + Send,
{
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        (self.start)(ctx)
    }
}

fn register(
    kernel: &Kernel,
    id: &str,
    dependencies: &[&str],
    start: impl Fn(PluginContext) -> PluginFuture<'static, Option<Cleanup>> + Send + 'static,
) {
    let mut manifest = PluginManifest::new(id, "0.1.0").unwrap();
    manifest.dependencies = dependencies
        .iter()
        .map(|id| PluginDependency {
            id: pid(id),
            requirement: None,
        })
        .collect();
    kernel
        .register(Box::new(TestPlugin { manifest, start }))
        .unwrap();
}

fn record_cleanup(calls: Arc<Mutex<Vec<String>>>, name: &str) -> Cleanup {
    let name = name.to_owned();
    cleanup(move || async move {
        calls.lock().unwrap().push(name);
        Ok(())
    })
}

#[derive(Clone, Copy)]
enum StartPanic {
    Factory,
    Future,
    NonStringFuture,
}

#[derive(Default)]
struct StartupObservations {
    old_context: Mutex<Option<PluginContext>>,
    listener: Mutex<Weak<()>>,
    task: Mutex<Weak<()>>,
    events_received: AtomicUsize,
}

async fn verify_startup_panic(mode: StartPanic, start_all: bool) {
    let services = KernelServices::default();
    let (registry, events, state) = (
        services.registry.clone(),
        services.events.clone(),
        services.state.clone(),
    );
    let kernel = Kernel::with_services(services);
    let calls = Arc::new(Mutex::new(Vec::new()));
    for id in ["a-existing", "b-new", "c-new"] {
        let calls = calls.clone();
        register(&kernel, id, &[], move |_| {
            let action = record_cleanup(calls.clone(), id);
            Box::pin(async { Ok(Some(action)) })
        });
    }
    kernel.start(&pid("a-existing")).await.unwrap();
    let observations = Arc::new(StartupObservations::default());
    let captured = observations.clone();
    let own_calls = calls.clone();
    register(
        &kernel,
        "z-panicking",
        &["a-existing", "b-new", "c-new"],
        move |ctx| {
            *captured.old_context.lock().unwrap() = Some(ctx.clone());
            ctx.state_set("checkpoint", vec![42]).unwrap();
            ctx.provide_service(ServiceId::new("startup.service").unwrap(), 7_u32)
                .unwrap();
            let listener = Arc::new(());
            *captured.listener.lock().unwrap() = Arc::downgrade(&listener);
            let listener_observations = captured.clone();
            ctx.on(
                EventId::new("startup.event").unwrap(),
                Arc::new(move |_| {
                    let _keep = &listener;
                    listener_observations
                        .events_received
                        .fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            )
            .unwrap();
            ctx.emit(Event::new("startup.event", Vec::new()).unwrap())
                .unwrap();
            let task = Arc::new(());
            *captured.task.lock().unwrap() = Arc::downgrade(&task);
            ctx.spawn_task(
                TaskSpec::new(
                    "启动中创建的任务",
                    TaskMode::Background,
                    TaskSchedule::Immediate,
                    Arc::new(move |signal| {
                        let task = task.clone();
                        Box::pin(async move {
                            let _keep = task;
                            signal.cancelled().await;
                            Ok(())
                        })
                    }),
                )
                .unwrap(),
            )
            .unwrap();
            ctx.cleanup(record_cleanup(own_calls.clone(), "z-panicking"))
                .unwrap();
            match mode {
                StartPanic::Factory => panic!("同步启动工厂崩溃"),
                StartPanic::Future => Box::pin(async {
                    tokio::task::yield_now().await;
                    std::panic::panic_any(String::from("异步启动轮询崩溃"));
                }),
                StartPanic::NonStringFuture => Box::pin(async {
                    tokio::task::yield_now().await;
                    std::panic::panic_any(17_u32);
                }),
            }
        },
    );
    let error = if start_all {
        kernel.start_all().await.unwrap_err()
    } else {
        kernel.start(&pid("z-panicking")).await.unwrap_err()
    };
    assert!(
        matches!(error, PluginError::PluginFailed { ref plugin, .. } if *plugin == pid("z-panicking"))
    );
    let message = error.to_string();
    assert!(message.contains("插件启动发生 panic"));
    assert!(message.contains(match mode {
        StartPanic::Factory => "同步启动工厂崩溃",
        StartPanic::Future => "异步启动轮询崩溃",
        StartPanic::NonStringFuture => "非字符串 panic 载荷",
    }));
    assert_eq!(kernel.state(&pid("z-panicking")), Some(PluginState::Failed));
    assert_eq!(kernel.state(&pid("a-existing")), Some(PluginState::Active));
    for id in ["b-new", "c-new"] {
        assert_eq!(kernel.state(&pid(id)), Some(PluginState::Stopped));
    }
    assert_eq!(*calls.lock().unwrap(), ["z-panicking", "c-new", "b-new"]);
    assert!(kernel.tasks(&pid("z-panicking")).unwrap().is_empty());
    assert!(observations.task.lock().unwrap().upgrade().is_none());
    assert!(
        registry
            .get(&ServiceId::new("startup.service").unwrap())
            .unwrap()
            .is_none()
    );
    assert!(observations.listener.lock().unwrap().upgrade().is_none());
    events
        .emit(Event::new("startup.event", Vec::new()).unwrap())
        .unwrap();
    assert_eq!(observations.events_received.load(Ordering::SeqCst), 1);
    // 启动失败收回运行资源，但不撤销已提交的检查点。
    assert_eq!(
        state.get(&pid("z-panicking"), "checkpoint").unwrap(),
        Some(vec![42])
    );
    let old_context = observations.old_context.lock().unwrap().take().unwrap();
    assert!(old_context.state_set("late", vec![1]).is_err());
    assert!(
        old_context
            .on(EventId::new("late.event").unwrap(), Arc::new(|_| Ok(())))
            .is_err()
    );
    assert!(matches!(
        kernel.start(&pid("z-panicking")).await,
        Err(PluginError::InvalidLifecycle { .. })
    ));
    kernel.stop_all().await.unwrap();
    assert_eq!(calls.lock().unwrap().last().unwrap(), "a-existing");
}

#[tokio::test]
async fn synchronous_start_panic_cleans_resources_and_rolls_back_only_new_dependencies() {
    verify_startup_panic(StartPanic::Factory, false).await;
}

#[tokio::test]
async fn start_future_panics_after_yield_restore_the_same_invariants_for_all_payloads() {
    for mode in [StartPanic::Future, StartPanic::NonStringFuture] {
        verify_startup_panic(mode, true).await;
    }
}

#[tokio::test]
async fn mixed_cleanup_panics_and_errors_keep_reverse_order_and_stop_other_plugins() {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let own_calls = calls.clone();
    register(&kernel, "a-broken", &[], move |ctx| {
        let calls = own_calls.clone();
        Box::pin(async move {
            ctx.provide_service(ServiceId::new("cleanup.service")?, 1_u32)?;
            ctx.cleanup(record_cleanup(calls.clone(), "first"))?;
            let ordinary = calls.clone();
            ctx.cleanup(cleanup(move || async move {
                ordinary.lock().unwrap().push("error".into());
                Err(PluginError::Cleanup("普通清理错误".into()))
            }))?;
            let factory = calls.clone();
            ctx.cleanup(Box::new(move || {
                factory.lock().unwrap().push("factory".into());
                panic!("同步清理工厂崩溃");
            }))?;
            let polling = calls.clone();
            ctx.cleanup(cleanup(move || async move {
                tokio::task::yield_now().await;
                polling.lock().unwrap().push("future".into());
                std::panic::panic_any(String::from("异步清理轮询崩溃"));
            }))?;
            Ok(Some(record_cleanup(calls, "last")))
        })
    });
    let healthy_calls = calls.clone();
    register(&kernel, "z-healthy", &[], move |_| {
        let action = record_cleanup(healthy_calls.clone(), "z-healthy");
        Box::pin(async { Ok(Some(action)) })
    });
    kernel.start_all().await.unwrap();
    let PluginError::Shutdown(errors) = kernel.stop_all().await.unwrap_err() else {
        panic!("清理错误必须进入结构化停止报告");
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].plugin, pid("a-broken"));
    assert_eq!(errors[0].stage, StopStage::Cleanup);
    assert!(matches!(*errors[0].error, PluginError::Cleanup(_)));
    let message = errors[0].error.to_string();
    for reason in [
        "普通清理错误",
        "同步清理工厂崩溃",
        "异步清理轮询崩溃",
        "资源清理发生 panic",
    ] {
        assert!(message.contains(reason));
    }
    assert_eq!(
        *calls.lock().unwrap(),
        ["last", "future", "factory", "error", "first", "z-healthy"]
    );
    assert!(
        registry
            .get(&ServiceId::new("cleanup.service").unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|plugin| plugin.state == PluginState::Stopped)
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn non_string_cleanup_panic_stays_with_consumer_and_does_not_block_provider_stop() {
    let kernel = Kernel::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider_calls = calls.clone();
    register(&kernel, "provider", &[], move |_| {
        let action = record_cleanup(provider_calls.clone(), "provider");
        Box::pin(async { Ok(Some(action)) })
    });
    let consumer_calls = calls.clone();
    register(&kernel, "consumer", &["provider"], move |_| {
        let calls = consumer_calls.clone();
        Box::pin(async move {
            Ok(Some(cleanup(move || async move {
                tokio::task::yield_now().await;
                calls.lock().unwrap().push("consumer".into());
                std::panic::panic_any(23_u64);
            })))
        })
    });
    kernel.start(&pid("consumer")).await.unwrap();
    let PluginError::Shutdown(errors) = kernel.stop(&pid("provider")).await.unwrap_err() else {
        panic!("应保留消费者清理错误");
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].plugin, pid("consumer"));
    assert_eq!(errors[0].stage, StopStage::Cleanup);
    assert!(errors[0].error.to_string().contains("非字符串 panic 载荷"));
    assert_eq!(*calls.lock().unwrap(), ["consumer", "provider"]);
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|plugin| plugin.state == PluginState::Stopped)
    );
}

#[tokio::test]
async fn rollback_cleanup_panics_preserve_start_error_and_continue_remaining_dependencies() {
    for start_all in [false, true] {
        let kernel = Kernel::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for id in ["a-provider", "b-provider", "c-provider"] {
            let calls = calls.clone();
            register(&kernel, id, &[], move |_| {
                let calls = calls.clone();
                let action: Cleanup = match id {
                    "b-provider" => Box::new(move || {
                        calls.lock().unwrap().push(id.into());
                        panic!("回滚同步清理崩溃");
                    }),
                    "c-provider" => cleanup(move || async move {
                        tokio::task::yield_now().await;
                        calls.lock().unwrap().push(id.into());
                        panic!("回滚异步清理崩溃");
                    }),
                    _ => record_cleanup(calls, id),
                };
                Box::pin(async { Ok(Some(action)) })
            });
        }
        register(
            &kernel,
            "z-failure",
            &["a-provider", "b-provider", "c-provider"],
            |_| {
                Box::pin(async {
                    Err(PluginError::Lifecycle("原始启动错误必须保留".into()))
                })
            },
        );
        let error = if start_all {
            kernel.start_all().await.unwrap_err()
        } else {
            kernel.start(&pid("z-failure")).await.unwrap_err()
        };
        let PluginError::Rollback { cause, errors } = error else {
            panic!("启动错误与回滚错误必须同时保留");
        };
        assert!(cause.to_string().contains("原始启动错误必须保留"));
        assert_eq!(errors.len(), 2);
        for (error, expected_owner, expected_reason) in [
            (&errors[0], "c-provider", "回滚异步清理崩溃"),
            (&errors[1], "b-provider", "回滚同步清理崩溃"),
        ] {
            let PluginError::Shutdown(failures) = error else {
                panic!("回滚必须保留结构化停止错误");
            };
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].plugin, pid(expected_owner));
            assert_eq!(failures[0].stage, StopStage::Cleanup);
            assert!(failures[0].error.to_string().contains(expected_reason));
        }
        assert_eq!(
            *calls.lock().unwrap(),
            ["c-provider", "b-provider", "a-provider"]
        );
        assert_eq!(kernel.state(&pid("z-failure")), Some(PluginState::Failed));
        for id in ["a-provider", "b-provider", "c-provider"] {
            assert_eq!(kernel.state(&pid(id)), Some(PluginState::Stopped));
        }
    }
}
