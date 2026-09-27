use eve_kernel::{Kernel, KernelConfig, KernelServices, PluginState, backends::TokioTaskManager};
use eve_plugin_api::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime};
use tokio::sync::{Notify, oneshot};
use tokio::time::{Instant, advance, sleep, timeout};

fn owner() -> PluginId {
    PluginId::new("tasks").unwrap()
}
fn spec(
    mode: TaskMode,
    schedule: TaskSchedule,
    action: impl Fn(Arc<dyn TaskSignal>) -> TaskFuture + Send + Sync + 'static,
) -> TaskSpec {
    TaskSpec::new("测试任务", mode, schedule, Arc::new(action)).unwrap()
}
fn immediate(mode: TaskMode) -> TaskSpec {
    spec(mode, TaskSchedule::Immediate, |_| {
        Box::pin(async { Ok(()) })
    })
}

async fn settle() {
    // 让执行器与退出监视器处理已就绪工作，不依赖墙上时钟。
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
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
    start: impl Fn(PluginContext) -> PluginFuture<'static, Option<Cleanup>> + Send + 'static,
) {
    kernel
        .register(Box::new(TestPlugin {
            manifest: PluginManifest::new("tasks", "0.1.0").unwrap(),
            start,
        }))
        .unwrap();
}
async fn context(kernel: &Kernel) -> PluginContext {
    let captured = Arc::new(Mutex::new(None));
    let result = captured.clone();
    register(kernel, move |ctx| {
        *captured.lock().unwrap() = Some(ctx);
        Box::pin(async { Ok(None) })
    });
    kernel.start(&owner()).await.unwrap();
    result.lock().unwrap().take().unwrap()
}

#[tokio::test(start_paused = true)]
async fn foreground_repeats_are_serial_and_wait_after_each_completion() {
    let manager = Arc::new(TokioTaskManager::default());
    let starts = Arc::new(Mutex::new(Vec::new()));
    let observed = starts.clone();
    let report = manager
        .clone()
        .run_foreground(
            owner(),
            spec(
                TaskMode::Foreground,
                TaskSchedule::Every {
                    interval: Duration::from_secs(5),
                    runs: Some(3),
                },
                move |_| {
                    let observed = observed.clone();
                    Box::pin(async move {
                        observed.lock().unwrap().push(Instant::now());
                        sleep(Duration::from_secs(2)).await;
                        Ok(())
                    })
                },
            ),
        )
        .unwrap()
        .await
        .unwrap();
    assert_eq!(report.runs, 3);
    assert_eq!(report.state, TaskState::Finished);
    let starts = starts.lock().unwrap();
    assert_eq!(starts[1] - starts[0], Duration::from_secs(7));
    assert_eq!(starts[2] - starts[1], Duration::from_secs(7));
    assert!(manager.list(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn delayed_and_absolute_tasks_wait_and_overdue_tasks_run_once() {
    let manager = Arc::new(TokioTaskManager::default());
    let count = Arc::new(AtomicUsize::new(0));
    for schedule in [
        TaskSchedule::After(Duration::from_secs(60)),
        TaskSchedule::At(SystemTime::now() + Duration::from_secs(60)),
    ] {
        let count = count.clone();
        manager
            .spawn(
                owner(),
                spec(TaskMode::Background, schedule, move |_| {
                    let count = count.clone();
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
            )
            .unwrap();
    }
    settle().await;
    assert!(
        manager
            .list(&owner())
            .unwrap()
            .iter()
            .all(|task| task.state == TaskState::Scheduled)
    );
    advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    advance(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let report = manager
        .clone()
        .run_foreground(
            owner(),
            spec(
                TaskMode::Foreground,
                TaskSchedule::At(SystemTime::UNIX_EPOCH),
                |_| Box::pin(async { Ok(()) }),
            ),
        )
        .unwrap()
        .await
        .unwrap();
    assert_eq!(report.runs, 1);
    let report = manager
        .shutdown(&owner(), Duration::from_secs(5), Duration::from_millis(100))
        .await
        .unwrap();
    assert!(report.is_clean());
    assert_eq!(report.stopped.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn infinite_background_repetition_stops_between_runs() {
    let manager = Arc::new(TokioTaskManager::default());
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    manager
        .spawn(
            owner(),
            spec(
                TaskMode::Background,
                TaskSchedule::Every {
                    interval: Duration::from_secs(10),
                    runs: None,
                },
                move |_| {
                    let observed = observed.clone();
                    Box::pin(async move {
                        observed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                },
            ),
        )
        .unwrap();
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(
        manager
            .clone()
            .shutdown(&owner(), Duration::from_secs(1), Duration::from_millis(100))
            .await
            .unwrap()
            .is_clean()
    );
    advance(Duration::from_secs(100)).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(manager.list(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn foreground_cancellation_reports_actual_runs_including_zero() {
    for schedule in [
        TaskSchedule::After(Duration::from_secs(60)),
        TaskSchedule::Every {
            interval: Duration::from_secs(60),
            runs: Some(5),
        },
    ] {
        let manager = Arc::new(TokioTaskManager::default());
        let waiting = manager
            .clone()
            .run_foreground(
                owner(),
                spec(TaskMode::Foreground, schedule.clone(), |_| {
                    Box::pin(async { Ok(()) })
                }),
            )
            .unwrap();
        settle().await;
        let shutdown = manager
            .clone()
            .shutdown(&owner(), Duration::from_secs(1), Duration::from_millis(100))
            .await
            .unwrap();
        assert!(shutdown.is_clean());
        let report = waiting.await.unwrap();
        assert_eq!(report.state, TaskState::Cancelled);
        assert_eq!(
            report.runs,
            if matches!(schedule, TaskSchedule::After(_)) {
                0
            } else {
                1
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn task_errors_and_panics_are_reported_and_do_not_stop_other_tasks() {
    let manager = Arc::new(TokioTaskManager::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let action_runs = runs.clone();
    let report = manager
        .clone()
        .run_foreground(
            owner(),
            spec(
                TaskMode::Foreground,
                TaskSchedule::Every {
                    interval: Duration::from_secs(1),
                    runs: Some(5),
                },
                move |_| {
                    let action_runs = action_runs.clone();
                    Box::pin(async move {
                        if action_runs.fetch_add(1, Ordering::SeqCst) == 2 {
                            return Err(PluginError::Task("第三次失败".into()));
                        }
                        Ok(())
                    })
                },
            ),
        )
        .unwrap()
        .await
        .unwrap();
    assert_eq!(report.runs, 2);
    assert_eq!(report.state, TaskState::Failed);
    assert!(report.errors[0].contains("第三次失败"));
    manager
        .spawn(
            owner(),
            spec(TaskMode::Background, TaskSchedule::Immediate, |_| {
                panic!("任务工厂 panic")
            }),
        )
        .unwrap();
    manager
        .spawn(
            owner(),
            spec(TaskMode::Background, TaskSchedule::Immediate, |_| {
                Box::pin(async { panic!("任务 Future panic") })
            }),
        )
        .unwrap();
    manager
        .spawn(owner(), immediate(TaskMode::Background))
        .unwrap();
    settle().await;
    assert_eq!(
        manager
            .list(&owner())
            .unwrap()
            .iter()
            .filter(|task| task.state == TaskState::Failed)
            .count(),
        2
    );
    let report = manager
        .shutdown(&owner(), Duration::from_secs(1), Duration::from_millis(100))
        .await
        .unwrap();
    assert_eq!(report.stopped.len(), 3);
    assert_eq!(report.errors.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn backend_validates_mutated_specs_and_modes() {
    let manager = Arc::new(TokioTaskManager::default());
    assert!(
        manager
            .spawn(owner(), immediate(TaskMode::Foreground))
            .is_err()
    );
    assert!(
        manager
            .clone()
            .run_foreground(owner(), immediate(TaskMode::Background))
            .is_err()
    );
    let mut empty = immediate(TaskMode::Background);
    empty.name.clear();
    assert!(manager.spawn(owner(), empty).is_err());
    for schedule in [
        TaskSchedule::Every {
            interval: Duration::ZERO,
            runs: Some(1),
        },
        TaskSchedule::Every {
            interval: Duration::from_secs(1),
            runs: Some(0),
        },
        TaskSchedule::After(Duration::MAX),
    ] {
        let mut invalid = immediate(TaskMode::Background);
        invalid.schedule = schedule;
        assert!(manager.spawn(owner(), invalid).is_err());
    }
    let mut infinite = immediate(TaskMode::Foreground);
    infinite.schedule = TaskSchedule::Every {
        interval: Duration::from_secs(1),
        runs: None,
    };
    assert!(manager.clone().run_foreground(owner(), infinite).is_err());
    assert!(manager.list(&owner()).unwrap().is_empty());
}

#[test]
fn spawn_without_executor_returns_error_instead_of_panicking() {
    let manager = TokioTaskManager::default();
    assert!(
        manager
            .spawn(owner(), immediate(TaskMode::Background))
            .is_err()
    );
    assert!(manager.list(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn signal_can_be_awaited_multiple_times_before_and_after_cancel() {
    let manager = Arc::new(TokioTaskManager::default());
    let ready = Arc::new(Notify::new());
    let started = ready.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    manager
        .spawn(
            owner(),
            spec(
                TaskMode::Background,
                TaskSchedule::Immediate,
                move |signal| {
                    let (started, observed) = (started.clone(), observed.clone());
                    Box::pin(async move {
                        let before = signal.cancelled();
                        started.notify_one();
                        before.await;
                        assert!(signal.is_cancelled());
                        signal.cancelled().await;
                        observed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                },
            ),
        )
        .unwrap();
    ready.notified().await;
    assert!(
        manager
            .shutdown(&owner(), Duration::from_secs(5), Duration::from_millis(100))
            .await
            .unwrap()
            .is_clean()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_uses_one_deadline_and_confirms_aborted_tasks_exit() {
    let manager = Arc::new(TokioTaskManager::default());
    let ready = Arc::new(Notify::new());
    for _ in 0..3 {
        let started = ready.clone();
        manager
            .spawn(
                owner(),
                spec(TaskMode::Background, TaskSchedule::Immediate, move |_| {
                    let started = started.clone();
                    Box::pin(async move {
                        started.notify_one();
                        std::future::pending::<PluginResult<()>>().await
                    })
                }),
            )
            .unwrap();
        ready.notified().await;
    }
    let began = Instant::now();
    let report = manager
        .clone()
        .shutdown(&owner(), Duration::from_secs(5), Duration::from_millis(100))
        .await
        .unwrap();
    assert_eq!(report.timed_out.len(), 3);
    assert_eq!(report.stopped.len(), 3);
    assert!(report.unfinished.is_empty());
    assert!(began.elapsed() <= Duration::from_millis(5100));
    assert!(manager.list(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn old_context_and_unpolled_foreground_future_cannot_register_tasks() {
    let kernel = Kernel::new();
    let ctx = context(&kernel).await;
    let deferred = ctx.run_foreground(immediate(TaskMode::Foreground));
    kernel.stop_all().await.unwrap();
    kernel.start(&owner()).await.unwrap();
    assert!(deferred.await.is_err());
    assert!(ctx.spawn_task(immediate(TaskMode::Background)).is_err());
    assert!(
        ctx.run_foreground(immediate(TaskMode::Foreground))
            .await
            .is_err()
    );
    assert!(ctx.tasks().is_err());
    assert!(kernel.tasks(&owner()).unwrap().is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stopping_seals_registration_but_allows_tasks_to_save_state_before_cleanup() {
    let services = KernelServices::default();
    let state = services.state.clone();
    let kernel = Kernel::with_services(services);
    let cleaned = Arc::new(AtomicUsize::new(0));
    let observed = cleaned.clone();
    register(&kernel, move |ctx| {
        let (state, observed) = (state.clone(), observed.clone());
        Box::pin(async move {
            let ready = Arc::new(Notify::new());
            let started = ready.clone();
            let held = ctx.clone();
            ctx.spawn_task(spec(
                TaskMode::Background,
                TaskSchedule::Immediate,
                move |signal| {
                    let (held, started) = (held.clone(), started.clone());
                    Box::pin(async move {
                        started.notify_one();
                        signal.cancelled().await;
                        assert!(held.spawn_task(immediate(TaskMode::Background)).is_err());
                        assert!(
                            held.run_foreground(immediate(TaskMode::Foreground))
                                .await
                                .is_err()
                        );
                        assert!(
                            held.provide_service(ServiceId::new("late")?, 1_u32)
                                .is_err()
                        );
                        assert!(held.cleanup(cleanup(|| async { Ok(()) })).is_err());
                        held.state_set("saved", b"yes".to_vec())
                    })
                },
            ))?;
            ready.notified().await;
            Ok(Some(cleanup(move || async move {
                assert_eq!(state.get(&owner(), "saved")?, Some(b"yes".to_vec()));
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })))
        })
    });
    kernel.start(&owner()).await.unwrap();
    kernel.stop_all().await.unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert!(kernel.tasks(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn failed_start_cancels_its_tasks_and_releases_captured_resources() {
    let kernel = Kernel::new();
    let observed = Arc::new(Mutex::new(None));
    let captured = observed.clone();
    register(&kernel, move |ctx| {
        let captured = captured.clone();
        Box::pin(async move {
            let resource = Arc::new(());
            *captured.lock().unwrap() = Some(Arc::downgrade(&resource));
            let ready = Arc::new(Notify::new());
            let started = ready.clone();
            ctx.spawn_task(spec(
                TaskMode::Background,
                TaskSchedule::Immediate,
                move |signal| {
                    let (resource, started) = (resource.clone(), started.clone());
                    Box::pin(async move {
                        let _hold = resource;
                        started.notify_one();
                        signal.cancelled().await;
                        Ok(())
                    })
                },
            ))?;
            ready.notified().await;
            Err(PluginError::Lifecycle("预期启动失败".into()))
        })
    });
    assert!(kernel.start(&owner()).await.is_err());
    assert_eq!(kernel.state(&owner()), Some(PluginState::Failed));
    assert!(kernel.tasks(&owner()).unwrap().is_empty());
    assert!(
        observed
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn active_foreground_tasks_are_cancelled_when_the_plugin_stops() {
    let kernel = Kernel::new();
    let ctx = context(&kernel).await;
    let ready = Arc::new(Notify::new());
    let started = ready.clone();
    let waiting = tokio::spawn(ctx.run_foreground(spec(
        TaskMode::Foreground,
        TaskSchedule::Immediate,
        move |signal| {
            let started = started.clone();
            Box::pin(async move {
                started.notify_one();
                signal.cancelled().await;
                Ok(())
            })
        },
    )));
    ready.notified().await;
    kernel.stop_all().await.unwrap();
    let report = waiting.await.unwrap().unwrap();
    assert_eq!(report.state, TaskState::Cancelled);
    assert_eq!(report.runs, 1);
    assert!(kernel.tasks(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn dropping_foreground_wait_cancels_work_and_drops_captures() {
    let manager = Arc::new(TokioTaskManager::default());
    let resource = Arc::new(());
    let weak = Arc::downgrade(&resource);
    let ready = Arc::new(Notify::new());
    let started = ready.clone();
    let waiting = manager
        .clone()
        .run_foreground(
            owner(),
            spec(TaskMode::Foreground, TaskSchedule::Immediate, move |_| {
                let (resource, started) = (resource.clone(), started.clone());
                Box::pin(async move {
                    let _hold = resource;
                    started.notify_one();
                    std::future::pending().await
                })
            }),
        )
        .unwrap();
    ready.notified().await;
    drop(waiting);
    settle().await;
    assert!(weak.upgrade().is_none());
    assert!(manager.list(&owner()).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn dropping_default_kernel_aborts_tasks_without_a_reference_cycle() {
    let kernel = Kernel::new();
    let ctx = context(&kernel).await;
    let resource = Arc::new(());
    let weak = Arc::downgrade(&resource);
    ctx.spawn_task(spec(
        TaskMode::Background,
        TaskSchedule::Immediate,
        move |_| {
            let resource = resource.clone();
            Box::pin(async move {
                let _hold = resource;
                std::future::pending().await
            })
        },
    ))
    .unwrap();
    settle().await;
    drop(kernel);
    settle().await;
    assert!(weak.upgrade().is_none());
}

// 用受控阻塞模拟无法被 Tokio abort 立即终止的动作，测试总会主动释放该线程。
#[tokio::test]
async fn blocking_task_is_reported_unfinished_and_kernel_cannot_restart_it() {
    let services = KernelServices::default();
    let tasks = services.tasks.clone();
    let kernel = Kernel::with_services_and_config(
        services,
        KernelConfig {
            task_shutdown_timeout: Duration::from_millis(20),
            task_abort_timeout: Duration::from_millis(20),
        },
    );
    let ctx = context(&kernel).await;
    let (release, blocked) = std::sync::mpsc::channel();
    let blocked = Arc::new(Mutex::new(blocked));
    let (entered, ready) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    // 阻塞工作使用独立执行器，测试宿主的计时线程保持可调度。
    let worker_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    {
        let _entered_runtime = worker_runtime.enter();
        ctx.spawn_task(spec(
            TaskMode::Background,
            TaskSchedule::Immediate,
            move |_| {
                let (blocked, entered) = (blocked.clone(), entered.clone());
                Box::pin(async move {
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    let _ = blocked.lock().unwrap().recv_timeout(Duration::from_secs(3));
                    Ok(())
                })
            },
        ))
        .unwrap();
    }
    ready.await.unwrap();
    let result = timeout(Duration::from_secs(1), kernel.stop_all()).await;
    let infos = kernel.tasks(&owner()).unwrap();
    // 在任何断言之前释放，避免测试失败把工作线程留在阻塞状态。
    let _ = release.send(());
    let reaped = tasks
        .shutdown(&owner(), Duration::from_secs(1), Duration::from_millis(100))
        .await;
    worker_runtime.shutdown_background();
    let error = result.expect("停止不能在 abort 后无限等待").unwrap_err();
    assert!(error.to_string().contains("尚未确认退出"));
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].state, TaskState::TimedOut);
    assert!(!infos[0].exited);
    assert_eq!(kernel.state(&owner()), Some(PluginState::Failed));
    assert!(kernel.start(&owner()).await.is_err());
    let report = reaped.unwrap();
    assert_eq!(report.stopped.len(), 1);
    assert!(report.unfinished.is_empty());
}

struct AlternativeTasks {
    inner: Arc<TokioTaskManager>,
    registered: AtomicUsize,
    stopped: AtomicUsize,
}
impl TaskManager for AlternativeTasks {
    fn spawn(&self, owner: PluginId, spec: TaskSpec) -> PluginResult<TaskId> {
        self.registered.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn(owner, spec)
    }
    fn run_foreground(
        self: Arc<Self>,
        owner: PluginId,
        spec: TaskSpec,
    ) -> PluginResult<PluginFuture<'static, TaskRunReport>> {
        self.registered.fetch_add(1, Ordering::SeqCst);
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
        self.stopped.fetch_add(1, Ordering::SeqCst);
        self.inner.clone().shutdown(owner, timeout, abort_timeout)
    }
}

#[tokio::test(start_paused = true)]
async fn task_backend_can_be_replaced_without_changing_plugin_api() {
    let tasks = Arc::new(AlternativeTasks {
        inner: Arc::new(TokioTaskManager::default()),
        registered: AtomicUsize::new(0),
        stopped: AtomicUsize::new(0),
    });
    let kernel = Kernel::with_services(KernelServices {
        tasks: tasks.clone(),
        ..Default::default()
    });
    let ctx = context(&kernel).await;
    assert_eq!(
        ctx.run_foreground(immediate(TaskMode::Foreground))
            .await
            .unwrap()
            .runs,
        1
    );
    ctx.spawn_task(spec(
        TaskMode::Background,
        TaskSchedule::After(Duration::from_secs(60)),
        |_| Box::pin(async { Ok(()) }),
    ))
    .unwrap();
    kernel.stop_all().await.unwrap();
    assert_eq!(tasks.registered.load(Ordering::SeqCst), 2);
    assert_eq!(tasks.stopped.load(Ordering::SeqCst), 1);
}
