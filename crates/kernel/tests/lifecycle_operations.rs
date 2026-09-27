use eve_kernel::{Kernel, KernelConfig, KernelServices, backends::TokioTaskManager};
use eve_plugin_api::*;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Notify;

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

async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await
}

async fn notified(signal: &Notify) {
    tokio::time::timeout(Duration::from_secs(2), signal.notified())
        .await
        .expect("受控生命周期操作未到达等待点");
}

async fn wait_report(kernel: &Kernel, id: LifecycleOperationId) -> LifecycleOperation {
    tokio::time::timeout(Duration::from_secs(2), kernel.wait_lifecycle(id))
        .await
        .expect("生命周期操作未完成")
        .unwrap()
}

fn successful(report: &LifecycleOperation) {
    assert_eq!(report.state, LifecycleOperationState::Completed(Ok(())));
}

fn record_cleanup(calls: Arc<Mutex<Vec<String>>>, name: &str) -> Cleanup {
    let name = name.to_owned();
    cleanup(move || async move {
        calls.lock().unwrap().push(name);
        Ok(())
    })
}

#[tokio::test]
async fn dropping_start_wrappers_preserves_execution_context_and_queryable_result() {
    for all in [false, true] {
        let kernel = Kernel::new();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let contexts = Arc::new(Mutex::new(None));
        let (started, proceed, captured) = (entered.clone(), release.clone(), contexts.clone());
        register(&kernel, "owner", &[], move |ctx| {
            let (started, proceed, captured) = (started.clone(), proceed.clone(), captured.clone());
            Box::pin(async move {
                *captured.lock().unwrap() = Some(ctx.clone());
                ctx.spawn_task(TaskSpec::new(
                    "存活任务",
                    TaskMode::Background,
                    TaskSchedule::Immediate,
                    Arc::new(|signal| {
                        Box::pin(async move {
                            signal.cancelled().await;
                            Ok(())
                        })
                    }),
                )?)?;
                started.notify_one();
                proceed.notified().await;
                ctx.state_set("started", vec![1])?;
                Ok(None)
            })
        });
        let owner = pid("owner");
        let mut waiting: PluginFuture<'_, ()> = if all {
            Box::pin(kernel.start_all())
        } else {
            Box::pin(kernel.start(&owner))
        };
        assert!(poll_once(waiting.as_mut()).await.is_pending());
        notified(&entered).await;
        let before = kernel.lifecycle_operations().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].state, LifecycleOperationState::Running);
        assert_eq!(kernel.state(&owner), Some(PluginState::Starting));
        drop(waiting);
        release.notify_one();
        let report = wait_report(&kernel, before[0].id).await;
        successful(&report);
        assert_eq!(kernel.state(&owner), Some(PluginState::Active));
        let context = contexts.lock().unwrap().take().unwrap();
        assert_eq!(context.state_get("started").unwrap(), Some(vec![1]));
        context.state_set("still-active", vec![2]).unwrap();
        assert_eq!(kernel.tasks(&owner).unwrap().len(), 1);
        assert_eq!(kernel.lifecycle_operations().unwrap(), vec![report]);
        assert!(kernel.acknowledge_lifecycle(before[0].id).unwrap());
        kernel.stop_all().await.unwrap();
        assert!(kernel.tasks(&owner).unwrap().is_empty());
        assert!(context.state_get("started").is_err());
        assert!(kernel.lifecycle_operations().unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancelled_start_wait_still_rolls_back_failure_and_keeps_original_error_and_state() {
    let services = KernelServices::default();
    let state = services.state.clone();
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
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let contexts = Arc::new(Mutex::new(None));
    let (started, proceed, captured, own_calls) = (
        entered.clone(),
        release.clone(),
        contexts.clone(),
        calls.clone(),
    );
    register(
        &kernel,
        "z-failing",
        &["a-existing", "b-new", "c-new"],
        move |ctx| {
            let (started, proceed, captured, own_calls) = (
                started.clone(),
                proceed.clone(),
                captured.clone(),
                own_calls.clone(),
            );
            Box::pin(async move {
                *captured.lock().unwrap() = Some(ctx.clone());
                ctx.state_set("checkpoint", vec![42])?;
                ctx.cleanup(record_cleanup(own_calls, "z-failing"))?;
                started.notify_one();
                proceed.notified().await;
                Err(PluginError::Lifecycle(
                    "取消等待后仍须保存的原始错误".into(),
                ))
            })
        },
    );
    let mut waiting = Box::pin(kernel.start_all());
    assert!(poll_once(waiting.as_mut()).await.is_pending());
    notified(&entered).await;
    let id = kernel.lifecycle_operations().unwrap()[0].id;
    drop(waiting);
    release.notify_one();
    let report = wait_report(&kernel, id).await;
    let LifecycleOperationState::Completed(Err(error)) = &report.state else {
        panic!("已取消等待的启动失败仍须留下错误终态");
    };
    assert!(error.to_string().contains("取消等待后仍须保存的原始错误"));
    assert_eq!(*calls.lock().unwrap(), ["z-failing", "c-new", "b-new"]);
    assert_eq!(kernel.state(&pid("z-failing")), Some(PluginState::Failed));
    assert_eq!(kernel.state(&pid("a-existing")), Some(PluginState::Active));
    for id in ["b-new", "c-new"] {
        assert_eq!(kernel.state(&pid(id)), Some(PluginState::Stopped));
    }
    assert!(
        contexts
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .state_get("checkpoint")
            .is_err()
    );
    assert_eq!(
        state.get(&pid("z-failing"), "checkpoint").unwrap(),
        Some(vec![42])
    );
    assert_eq!(kernel.lifecycle_operations().unwrap(), vec![report]);
    kernel.acknowledge_lifecycle(id).unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn dropping_stop_wrappers_during_task_or_cleanup_wait_keeps_all_cleanup_actions() {
    for all in [false, true] {
        for cancel_during_task in [false, true] {
            let kernel = Kernel::new();
            let task_ready = Arc::new(Notify::new());
            let task_stopping = Arc::new(Notify::new());
            let task_release = Arc::new(Notify::new());
            let cleanup_entered = Arc::new(Notify::new());
            let cleanup_release = Arc::new(Notify::new());
            let calls = Arc::new(Mutex::new(Vec::new()));
            let contexts = Arc::new(Mutex::new(None));
            let (ready, stopping, finish_task) = (
                task_ready.clone(),
                task_stopping.clone(),
                task_release.clone(),
            );
            let (cleaning, finish_cleanup, recorded, captured) = (
                cleanup_entered.clone(),
                cleanup_release.clone(),
                calls.clone(),
                contexts.clone(),
            );
            register(&kernel, "owner", &[], move |ctx| {
                let (ready, stopping, finish_task) =
                    (ready.clone(), stopping.clone(), finish_task.clone());
                let (cleaning, finish_cleanup, recorded, captured) = (
                    cleaning.clone(),
                    finish_cleanup.clone(),
                    recorded.clone(),
                    captured.clone(),
                );
                Box::pin(async move {
                    *captured.lock().unwrap() = Some(ctx.clone());
                    let task_context = ctx.clone();
                    ctx.spawn_task(TaskSpec::new(
                        "协作收尾任务",
                        TaskMode::Background,
                        TaskSchedule::Immediate,
                        Arc::new(move |signal| {
                            let (ready, stopping, finish_task, task_context) = (
                                ready.clone(),
                                stopping.clone(),
                                finish_task.clone(),
                                task_context.clone(),
                            );
                            Box::pin(async move {
                                ready.notify_one();
                                signal.cancelled().await;
                                task_context.state_set("checkpoint", vec![9])?;
                                stopping.notify_one();
                                finish_task.notified().await;
                                Ok(())
                            })
                        }),
                    )?)?;
                    ctx.cleanup(record_cleanup(recorded.clone(), "first"))?;
                    let middle_calls = recorded.clone();
                    ctx.cleanup(cleanup(move || async move {
                        middle_calls.lock().unwrap().push("middle.enter".into());
                        cleaning.notify_one();
                        finish_cleanup.notified().await;
                        middle_calls.lock().unwrap().push("middle.exit".into());
                        Ok(())
                    }))?;
                    Ok(Some(record_cleanup(recorded, "last")))
                })
            });
            let owner = pid("owner");
            kernel.start(&owner).await.unwrap();
            notified(&task_ready).await;
            let mut waiting: PluginFuture<'_, ()> = if all {
                Box::pin(kernel.stop_all())
            } else {
                Box::pin(kernel.stop(&owner))
            };
            assert!(poll_once(waiting.as_mut()).await.is_pending());
            notified(&task_stopping).await;
            assert_eq!(kernel.state(&owner), Some(PluginState::Stopping));
            assert!(calls.lock().unwrap().is_empty());
            let context = contexts.lock().unwrap().as_ref().unwrap().clone();
            assert_eq!(context.state_get("checkpoint").unwrap(), Some(vec![9]));
            let id = kernel.lifecycle_operations().unwrap()[0].id;
            if cancel_during_task {
                drop(waiting);
                task_release.notify_one();
                notified(&cleanup_entered).await;
            } else {
                task_release.notify_one();
                notified(&cleanup_entered).await;
                drop(waiting);
            }
            assert_eq!(*calls.lock().unwrap(), ["last", "middle.enter"]);
            assert!(context.state_get("checkpoint").is_err());
            cleanup_release.notify_one();
            let report = wait_report(&kernel, id).await;
            successful(&report);
            assert_eq!(
                *calls.lock().unwrap(),
                ["last", "middle.enter", "middle.exit", "first"]
            );
            assert_eq!(kernel.state(&owner), Some(PluginState::Stopped));
            assert!(kernel.tasks(&owner).unwrap().is_empty());
            assert_eq!(kernel.lifecycle_operations().unwrap(), vec![report]);
            kernel.acknowledge_lifecycle(id).unwrap();
        }
    }
}

#[tokio::test]
async fn unpolled_and_queued_requests_can_be_cancelled_without_admission_or_side_effects() {
    let kernel = Kernel::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (started, proceed) = (entered.clone(), release.clone());
    register(&kernel, "blocked", &[], move |_| {
        let (started, proceed) = (started.clone(), proceed.clone());
        Box::pin(async move {
            started.notify_one();
            proceed.notified().await;
            Ok(None)
        })
    });
    let count = Arc::new(AtomicUsize::new(0));
    let started_count = count.clone();
    register(&kernel, "queued", &[], move |_| {
        started_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    });
    let queued_owner = pid("queued");
    drop(kernel.start(&queued_owner));
    drop(kernel.submit_lifecycle(LifecycleRequest::Start(queued_owner.clone())));
    assert!(kernel.lifecycle_operations().unwrap().is_empty());
    let id = kernel
        .submit_lifecycle(LifecycleRequest::Start(pid("blocked")))
        .await
        .unwrap();
    notified(&entered).await;
    let mut queued =
        Box::pin(kernel.submit_lifecycle(LifecycleRequest::Start(queued_owner.clone())));
    assert!(poll_once(queued.as_mut()).await.is_pending());
    let mut queued_wrapper = Box::pin(kernel.start(&queued_owner));
    assert!(poll_once(queued_wrapper.as_mut()).await.is_pending());
    drop(queued);
    drop(queued_wrapper);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(kernel.state(&queued_owner), Some(PluginState::Registered));
    assert_eq!(kernel.lifecycle_operations().unwrap().len(), 1);
    release.notify_one();
    successful(&wait_report(&kernel, id).await);
    kernel.start(&queued_owner).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(kernel.lifecycle_operations().unwrap().len(), 1);
    kernel.acknowledge_lifecycle(id).unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn trait_reports_support_multiple_waiters_acknowledgement_and_bounded_retention() {
    let kernel = Kernel::with_services_and_config(
        KernelServices::default(),
        KernelConfig {
            lifecycle_report_capacity: 2,
            ..Default::default()
        },
    );
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (started, proceed) = (entered.clone(), release.clone());
    let first_start = Arc::new(AtomicUsize::new(0));
    register(&kernel, "owner", &[], move |_| {
        let (started, proceed) = (started.clone(), proceed.clone());
        let gated = first_start.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if gated {
                started.notify_one();
                proceed.notified().await;
            }
            Ok(None)
        })
    });
    let lifecycle: &dyn RuntimeLifecycle = &kernel;
    let first = lifecycle
        .submit(LifecycleRequest::Start(pid("owner")))
        .await
        .unwrap();
    notified(&entered).await;
    assert!(lifecycle.acknowledge(first).is_err());
    let mut snapshot = lifecycle.operations().unwrap();
    snapshot[0].request = LifecycleRequest::StopAll;
    snapshot[0].state = LifecycleOperationState::Completed(Ok(()));
    assert_eq!(
        lifecycle.operations().unwrap()[0].request,
        LifecycleRequest::Start(pid("owner"))
    );
    assert_eq!(
        lifecycle.operations().unwrap()[0].state,
        LifecycleOperationState::Running
    );
    let mut waiter_one = lifecycle.wait(first);
    let mut waiter_two = lifecycle.wait(first);
    let mut discarded = lifecycle.wait(first);
    assert!(poll_once(waiter_one.as_mut()).await.is_pending());
    assert!(poll_once(waiter_two.as_mut()).await.is_pending());
    assert!(poll_once(discarded.as_mut()).await.is_pending());
    drop(discarded);
    release.notify_one();
    let (one, two) = tokio::join!(waiter_one, waiter_two);
    let one = one.unwrap();
    successful(&one);
    assert_eq!(two.unwrap(), one);
    assert_eq!(lifecycle.wait(first).await.unwrap(), one);
    let second = lifecycle
        .submit(LifecycleRequest::Stop(pid("owner")))
        .await
        .unwrap();
    successful(&lifecycle.wait(second).await.unwrap());
    let reports = lifecycle.operations().unwrap();
    assert_eq!(
        reports.iter().map(|report| report.id).collect::<Vec<_>>(),
        [first, second]
    );
    assert!(first < second);
    assert!(
        lifecycle
            .submit(LifecycleRequest::Start(pid("owner")))
            .await
            .is_err()
    );
    assert_eq!(lifecycle.operations().unwrap(), reports);
    assert_eq!(kernel.state(&pid("owner")), Some(PluginState::Stopped));
    assert!(lifecycle.acknowledge(first).unwrap());
    assert!(!lifecycle.acknowledge(first).unwrap());
    assert!(lifecycle.wait(first).await.is_err());
    assert!(
        !lifecycle
            .acknowledge(LifecycleOperationId::new(u64::MAX))
            .unwrap()
    );
    let third = lifecycle
        .submit(LifecycleRequest::Start(pid("owner")))
        .await
        .unwrap();
    successful(&lifecycle.wait(third).await.unwrap());
    assert!(second < third);
    lifecycle.acknowledge(second).unwrap();
    lifecycle.acknowledge(third).unwrap();
    // 宿主先确认报告，不能让已经准入的普通调用丢失它正在等待的结果。
    let mut wrapper = Box::pin(kernel.stop_all());
    assert!(poll_once(wrapper.as_mut()).await.is_pending());
    let externally_acknowledged = lifecycle.operations().unwrap()[0].id;
    successful(&lifecycle.wait(externally_acknowledged).await.unwrap());
    lifecycle.acknowledge(externally_acknowledged).unwrap();
    wrapper.await.unwrap();
    assert!(lifecycle.operations().unwrap().is_empty());
    for _ in 0..3 {
        kernel.stop_all().await.unwrap();
        kernel.start_all().await.unwrap();
        assert!(lifecycle.operations().unwrap().is_empty());
    }
    kernel.stop_all().await.unwrap();
    register(&kernel, "fails", &[], |_| {
        Box::pin(async {
            Err(PluginError::Lifecycle("普通返回错误也应确认报告".into()))
        })
    });
    assert!(kernel.start(&pid("fails")).await.is_err());
    assert!(lifecycle.operations().unwrap().is_empty());

    let disabled = Kernel::with_services_and_config(
        KernelServices::default(),
        KernelConfig {
            lifecycle_report_capacity: 0,
            ..Default::default()
        },
    );
    register(&disabled, "disabled", &[], |_| Box::pin(async { Ok(None) }));
    assert!(
        disabled
            .submit_lifecycle(LifecycleRequest::StartAll)
            .await
            .is_err()
    );
    assert!(disabled.start_all().await.is_err());
    assert!(disabled.lifecycle_operations().unwrap().is_empty());
    assert_eq!(
        disabled.state(&pid("disabled")),
        Some(PluginState::Registered)
    );
}

#[test]
fn executor_shutdown_records_interruption_invalidates_context_and_rejects_new_operations() {
    let kernel = Kernel::new();
    let entered = Arc::new(Notify::new());
    let contexts = Arc::new(Mutex::new(None));
    let (started, captured) = (entered.clone(), contexts.clone());
    register(&kernel, "owner", &[], move |ctx| {
        let (started, captured) = (started.clone(), captured.clone());
        Box::pin(async move {
            ctx.state_set("before-interruption", vec![1])?;
            *captured.lock().unwrap() = Some(ctx);
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(None)
        })
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let id = runtime.block_on(async {
        let id = kernel
            .submit_lifecycle(LifecycleRequest::Start(pid("owner")))
            .await
            .unwrap();
        notified(&entered).await;
        id
    });
    assert_eq!(kernel.state(&pid("owner")), Some(PluginState::Starting));
    drop(runtime);
    let reports = kernel.lifecycle_operations().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].id, id);
    assert!(matches!(
        reports[0].state,
        LifecycleOperationState::Interrupted(_)
    ));
    assert_eq!(kernel.state(&pid("owner")), Some(PluginState::Failed));
    let context = contexts.lock().unwrap().take().unwrap();
    assert!(context.state_get("before-interruption").is_err());
    assert!(context.state_set("after-interruption", vec![2]).is_err());
    assert!(context.cleanup(cleanup(|| async { Ok(()) })).is_err());
    let fresh_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    fresh_runtime.block_on(async {
        assert_eq!(kernel.wait_lifecycle(id).await.unwrap(), reports[0]);
        assert!(
            kernel
                .submit_lifecycle(LifecycleRequest::Start(pid("owner")))
                .await
                .is_err()
        );
        assert!(kernel.start_all().await.is_err());
    });
    assert_eq!(kernel.lifecycle_operations().unwrap(), reports);
}

#[test]
fn first_poll_without_executor_returns_error_without_registering_or_starting_work() {
    let kernel = Kernel::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = calls.clone();
    register(&kernel, "owner", &[], move |_| {
        recorded.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    });
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    let mut submitted = Box::pin(kernel.submit_lifecycle(LifecycleRequest::StartAll));
    assert!(matches!(
        submitted.as_mut().poll(&mut context),
        Poll::Ready(Err(PluginError::Lifecycle(_)))
    ));
    let mut wrapper = Box::pin(kernel.start_all());
    assert!(matches!(
        wrapper.as_mut().poll(&mut context),
        Poll::Ready(Err(PluginError::Lifecycle(_)))
    ));
    assert!(kernel.lifecycle_operations().unwrap().is_empty());
    assert_eq!(kernel.state(&pid("owner")), Some(PluginState::Registered));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        kernel.start_all().await.unwrap();
        kernel.stop_all().await.unwrap();
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(kernel.lifecycle_operations().unwrap().is_empty());
}

#[test]
fn interruption_before_worker_first_poll_preserves_existing_plugins_and_allows_retry() {
    let kernel = Kernel::new();
    let contexts = Arc::new(Mutex::new(None));
    let captured = contexts.clone();
    register(&kernel, "existing", &[], move |ctx| {
        let captured = captured.clone();
        Box::pin(async move {
            ctx.state_set("value", vec![1])?;
            *captured.lock().unwrap() = Some(ctx);
            Ok(None)
        })
    });
    let initial_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    initial_runtime
        .block_on(kernel.start(&pid("existing")))
        .unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let recorded = starts.clone();
    register(&kernel, "new", &[], move |_| {
        recorded.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    });

    let unused_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let id = {
        // 仅进入执行器上下文而不驱动current_thread，确保登记后worker一次也没有被轮询。
        let _entered = unused_runtime.enter();
        let mut submitted = Box::pin(kernel.submit_lifecycle(LifecycleRequest::Start(pid("new"))));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match submitted.as_mut().poll(&mut context) {
            Poll::Ready(Ok(id)) => id,
            other => panic!("空闲准入应在单次轮询内完成：{other:?}"),
        }
    };
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        kernel.lifecycle_operations().unwrap()[0].state,
        LifecycleOperationState::Running
    );
    drop(unused_runtime);
    let reports = kernel.lifecycle_operations().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].id, id);
    assert!(matches!(
        reports[0].state,
        LifecycleOperationState::Interrupted(_)
    ));
    assert_eq!(kernel.state(&pid("existing")), Some(PluginState::Active));
    assert_eq!(kernel.state(&pid("new")), Some(PluginState::Registered));
    let context = contexts.lock().unwrap().take().unwrap();
    assert_eq!(context.state_get("value").unwrap(), Some(vec![1]));
    context.state_set("value", vec![2]).unwrap();

    let retry_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    retry_runtime.block_on(async {
        assert_eq!(kernel.wait_lifecycle(id).await.unwrap(), reports[0]);
        kernel.start(&pid("new")).await.unwrap();
        assert_eq!(kernel.state(&pid("existing")), Some(PluginState::Active));
        assert_eq!(kernel.state(&pid("new")), Some(PluginState::Active));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(context.state_get("value").unwrap(), Some(vec![2]));
        kernel.acknowledge_lifecycle(id).unwrap();
        kernel.stop_all().await.unwrap();
    });
    assert!(kernel.lifecycle_operations().unwrap().is_empty());
}

struct PanickingShutdownTasks {
    inner: Arc<TokioTaskManager>,
}

impl TaskManager for PanickingShutdownTasks {
    fn spawn(&self, owner: PluginId, spec: TaskSpec) -> PluginResult<TaskId> {
        self.inner.spawn(owner, spec)
    }

    fn run_foreground(
        self: Arc<Self>,
        owner: PluginId,
        spec: TaskSpec,
    ) -> PluginResult<PluginFuture<'static, TaskRunReport>> {
        self.inner.clone().run_foreground(owner, spec)
    }

    fn list(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>> {
        self.inner.list(owner)
    }

    fn shutdown(
        self: Arc<Self>,
        owner: &PluginId,
        timeout: Duration,
        abort_timeout: Duration,
    ) -> PluginFuture<'static, TaskShutdownReport> {
        let owner = owner.clone();
        Box::pin(async move {
            if owner == pid("stop-target") {
                tokio::task::yield_now().await;
                panic!("注入任务后端停止panic");
            }
            self.inner
                .clone()
                .shutdown(&owner, timeout, abort_timeout)
                .await
        })
    }
}

#[tokio::test]
async fn shutdown_backend_panic_records_interruption_and_invalidates_all_active_contexts() {
    let tasks = Arc::new(TokioTaskManager::default());
    let kernel = Kernel::with_services(KernelServices {
        tasks: Arc::new(PanickingShutdownTasks {
            inner: tasks.clone(),
        }),
        ..Default::default()
    });
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let cleanups = Arc::new(Mutex::new(Vec::new()));
    for id in ["active-peer", "stop-target"] {
        let (captured, cleanups) = (contexts.clone(), cleanups.clone());
        register(&kernel, id, &[], move |ctx| {
            let (captured, cleanups) = (captured.clone(), cleanups.clone());
            Box::pin(async move {
                ctx.state_set("checkpoint", vec![7])?;
                ctx.spawn_task(TaskSpec::new(
                    "待收尾的任务",
                    TaskMode::Background,
                    TaskSchedule::Immediate,
                    Arc::new(|signal| {
                        Box::pin(async move {
                            signal.cancelled().await;
                            Ok(())
                        })
                    }),
                )?)?;
                captured.lock().unwrap().push(ctx);
                Ok(Some(record_cleanup(cleanups, id)))
            })
        });
    }
    kernel.start_all().await.unwrap();
    let id = kernel
        .submit_lifecycle(LifecycleRequest::Stop(pid("stop-target")))
        .await
        .unwrap();
    let report = wait_report(&kernel, id).await;
    assert!(matches!(
        report.state,
        LifecycleOperationState::Interrupted(_)
    ));
    assert!(cleanups.lock().unwrap().is_empty());
    for owner in ["active-peer", "stop-target"] {
        assert_eq!(kernel.state(&pid(owner)), Some(PluginState::Failed));
        let remaining = kernel.tasks(&pid(owner)).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(!remaining[0].exited);
    }
    for context in contexts.lock().unwrap().iter() {
        assert!(context.state_get("checkpoint").is_err());
        assert!(context.state_set("late", vec![1]).is_err());
        assert!(context.cleanup(cleanup(|| async { Ok(()) })).is_err());
    }
    kernel.acknowledge_lifecycle(id).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert!(kernel.lifecycle_operations().unwrap().is_empty());
    // 中断报告没有承诺清理完成；测试宿主持有真实后端，显式收回残余任务。
    for owner in ["active-peer", "stop-target"] {
        let stopped = tasks
            .clone()
            .shutdown(
                &pid(owner),
                Duration::from_secs(1),
                Duration::from_millis(100),
            )
            .await
            .unwrap();
        assert!(stopped.is_clean());
    }
}
