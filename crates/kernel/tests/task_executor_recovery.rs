use eve_kernel::{Kernel, KernelServices, backends::TokioTaskManager};
use eve_plugin_api::*;
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::Notify;

fn owner() -> PluginId {
    PluginId::new("executor-recovery").unwrap()
}

fn current_thread_runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Capture {
    released: mpsc::Sender<()>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.released.send(());
    }
}

fn capture() -> (Arc<Capture>, Weak<Capture>, mpsc::Receiver<()>) {
    let (released, receiver) = mpsc::channel();
    let capture = Arc::new(Capture { released });
    let weak = Arc::downgrade(&capture);
    (capture, weak, receiver)
}

fn pending_spec(
    mode: TaskMode,
    entered: Option<Arc<Notify>>,
) -> (TaskSpec, Weak<Capture>, mpsc::Receiver<()>) {
    let (capture, weak, released) = capture();
    let spec = TaskSpec::new(
        "执行器退出恢复验收",
        mode,
        TaskSchedule::Immediate,
        Arc::new(move |signal| {
            let (capture, entered) = (capture.clone(), entered.clone());
            Box::pin(async move {
                let _keep = capture;
                if let Some(entered) = entered {
                    entered.notify_one();
                }
                signal.cancelled().await;
                Ok(())
            })
        }),
    )
    .unwrap();
    (spec, weak, released)
}

fn assert_capture_dropped(weak: &Weak<Capture>, released: &mpsc::Receiver<()>) {
    released
        .recv_timeout(Duration::from_secs(2))
        .expect("原任务及其工厂捕获资源必须实际析构");
    assert!(weak.upgrade().is_none());
}

fn assert_unavailable(errors: &[String]) {
    assert!(
        errors
            .iter()
            .any(|error| error.contains("原始执行结果不可用")),
        "必须明确报告原始结果不可用：{errors:?}"
    );
}

async fn shutdown(tasks: &Arc<TokioTaskManager>) -> TaskShutdownReport {
    tokio::time::timeout(
        Duration::from_secs(2),
        tasks.clone().shutdown(
            &owner(),
            Duration::from_millis(50),
            Duration::from_millis(50),
        ),
    )
    .await
    .expect("丢失监视器后收尾不能无限等待")
    .unwrap()
}

#[test]
fn unpolled_task_with_lost_monitor_is_confirmed_exited_and_reaped_with_an_error() {
    let tasks = Arc::new(TokioTaskManager::default());
    let runtime = current_thread_runtime();
    let (spec, weak, released) = pending_spec(TaskMode::Background, None);
    let id = {
        // 只进入运行时上下文，不驱动current_thread，worker和monitor均未首次轮询。
        let _entered = runtime.enter();
        tasks.spawn(owner(), spec).unwrap()
    };
    assert_eq!(tasks.list(&owner()).unwrap()[0].state, TaskState::Scheduled);
    drop(runtime);
    assert_capture_dropped(&weak, &released);

    let recovery = current_thread_runtime();
    recovery.block_on(async {
        let infos = tasks.list(&owner()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, id);
        assert_eq!(infos[0].state, TaskState::Failed);
        assert!(infos[0].exited);
        assert_eq!(infos[0].runs, 0);
        let report = shutdown(&tasks).await;
        assert_eq!(report.stopped, vec![id]);
        assert!(report.unfinished.is_empty());
        assert!(report.timed_out.is_empty());
        assert_unavailable(&report.errors);
        assert!(tasks.list(&owner()).unwrap().is_empty());
        assert!(shutdown(&tasks).await.is_clean());
    });
}

#[test]
fn running_background_and_foreground_tasks_recover_when_their_monitor_executor_is_closed() {
    for (mode, discard_wait, shutdown_first) in [
        (TaskMode::Background, false, false),
        (TaskMode::Foreground, false, false),
        (TaskMode::Foreground, true, false),
        (TaskMode::Foreground, false, true),
    ] {
        let tasks = Arc::new(TokioTaskManager::default());
        let runtime = current_thread_runtime();
        let entered = Arc::new(Notify::new());
        let (spec, weak, released) = pending_spec(mode, Some(entered.clone()));
        let (id, mut foreground) = runtime.block_on(async {
            let (id, foreground) = match mode {
                TaskMode::Background => (tasks.spawn(owner(), spec).unwrap(), None),
                TaskMode::Foreground => {
                    let foreground = tasks.clone().run_foreground(owner(), spec).unwrap();
                    (
                        tasks.list(&owner()).unwrap()[0].id.clone(),
                        Some(foreground),
                    )
                }
            };
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            (id, foreground)
        });
        assert_eq!(tasks.list(&owner()).unwrap()[0].state, TaskState::Running);
        drop(runtime);
        assert_capture_dropped(&weak, &released);
        let recovery = current_thread_runtime();
        recovery.block_on(async {
            if discard_wait {
                drop(foreground.take());
                assert!(tasks.list(&owner()).unwrap().is_empty());
            } else if let Some(foreground) = foreground.take() {
                if shutdown_first {
                    // 停止先回收注册表记录，已有前台等待仍应取得同一个失败结果。
                    let report = shutdown(&tasks).await;
                    assert_eq!(report.stopped, vec![id.clone()]);
                    assert!(report.unfinished.is_empty());
                    assert_unavailable(&report.errors);
                }
                let report = tokio::time::timeout(Duration::from_secs(2), foreground)
                    .await
                    .expect("前台结果等待不能因monitor丢失而挂起")
                    .unwrap();
                assert_eq!(report.id, id);
                assert_eq!(report.state, TaskState::Failed);
                assert_eq!(report.runs, 0);
                assert_unavailable(&report.errors);
                assert!(tasks.list(&owner()).unwrap().is_empty());
            } else {
                let infos = tasks.list(&owner()).unwrap();
                assert_eq!(infos[0].state, TaskState::Failed);
                assert!(infos[0].exited);
                let report = shutdown(&tasks).await;
                assert_eq!(report.stopped, vec![id]);
                assert!(report.unfinished.is_empty());
                assert_unavailable(&report.errors);
                assert!(tasks.list(&owner()).unwrap().is_empty());
            }
        });
    }
}

struct BlockingRelease(Option<mpsc::Sender<()>>);

impl BlockingRelease {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for BlockingRelease {
    fn drop(&mut self) {
        self.release();
    }
}

#[test]
fn lost_monitor_does_not_confirm_a_blocking_worker_until_its_captures_are_released() {
    let tasks = Arc::new(TokioTaskManager::default());
    let (thread_stopped, threads_stopped) = mpsc::channel();
    let worker_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_stop(move || {
            let _ = thread_stopped.send(());
        })
        .build()
        .unwrap();
    let (release_sender, release_receiver) = mpsc::channel();
    // 任意后续断言失败时先释放阻塞线程；recv_timeout另提供最终兜底。
    let mut release = BlockingRelease(Some(release_sender));
    let blocked = Arc::new(Mutex::new(release_receiver));
    let (entered, ready) = mpsc::channel();
    let (capture, weak, released) = capture();
    let spec = TaskSpec::new(
        "受控原生阻塞任务",
        TaskMode::Background,
        TaskSchedule::Immediate,
        Arc::new(move |_| {
            let (blocked, entered, capture) = (blocked.clone(), entered.clone(), capture.clone());
            Box::pin(async move {
                let _keep = capture;
                let _ = entered.send(());
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
                Ok(())
            })
        }),
    )
    .unwrap();
    let id = {
        let _entered = worker_runtime.enter();
        tasks.spawn(owner(), spec).unwrap()
    };
    ready
        .recv_timeout(Duration::from_secs(2))
        .expect("阻塞worker未启动");
    worker_runtime.shutdown_background();
    let recovery = current_thread_runtime();
    let before_release = recovery.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(2),
            tasks.clone().shutdown(
                &owner(),
                Duration::from_millis(50),
                Duration::from_millis(50),
            ),
        )
        .await
    });
    let still_held = weak.upgrade().is_some();
    let infos = tasks.list(&owner());
    // 在分析结果前放行，避免测试失败让原生阻塞遗留在测试进程中。
    release.release();
    assert_capture_dropped(&weak, &released);
    for _ in 0..2 {
        threads_stopped
            .recv_timeout(Duration::from_secs(2))
            .expect("放行阻塞后执行器线程必须实际结束");
    }
    let report = before_release
        .expect("另一个执行器上的停止必须遵守期限")
        .unwrap();
    assert!(still_held);
    assert!(report.stopped.is_empty());
    assert_eq!(report.unfinished, vec![id.clone()]);
    assert!(!infos.unwrap()[0].exited);

    recovery.block_on(async {
        let infos = tasks.list(&owner()).unwrap();
        assert!(infos[0].exited);
        assert_ne!(infos[0].state, TaskState::Finished);
        let report = shutdown(&tasks).await;
        assert_eq!(report.stopped, vec![id]);
        assert!(report.unfinished.is_empty());
        assert_unavailable(&report.errors);
        assert!(tasks.list(&owner()).unwrap().is_empty());
    });
}

struct ContextPlugin {
    manifest: PluginManifest,
    contexts: Arc<Mutex<Vec<PluginContext>>>,
}

impl Plugin for ContextPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        self.contexts.lock().unwrap().push(ctx);
        Box::pin(async { Ok(None) })
    }
}

#[test]
fn kernel_reports_lost_task_results_but_allows_restart_after_confirmed_exit() {
    let tasks = Arc::new(TokioTaskManager::default());
    let kernel = Kernel::with_services(KernelServices {
        tasks: tasks.clone(),
        ..Default::default()
    });
    let contexts = Arc::new(Mutex::new(Vec::new()));
    kernel
        .register(Box::new(ContextPlugin {
            manifest: PluginManifest::new(owner().as_str(), "0.1.0").unwrap(),
            contexts: contexts.clone(),
        }))
        .unwrap();
    let host_runtime = current_thread_runtime();
    host_runtime.block_on(kernel.start(&owner())).unwrap();
    let old_context = contexts.lock().unwrap()[0].clone();
    let task_runtime = current_thread_runtime();
    let (spec, weak, released) = pending_spec(TaskMode::Background, None);
    let id = {
        let _entered = task_runtime.enter();
        old_context.spawn_task(spec).unwrap()
    };
    drop(task_runtime);
    assert_capture_dropped(&weak, &released);
    host_runtime.block_on(async {
        let PluginError::Shutdown(errors) = kernel.stop(&owner()).await.unwrap_err() else {
            panic!("任务监视器丢失必须报告停止错误");
        };
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].plugin, owner());
        assert_eq!(errors[0].stage, StopStage::Tasks);
        assert!(errors[0].error.to_string().contains("原始执行结果不可用"));
        assert!(errors[0].error.to_string().contains(&id.to_string()));
        assert_eq!(kernel.state(&owner()), Some(PluginState::Stopped));
        assert!(kernel.tasks(&owner()).unwrap().is_empty());
        assert!(old_context.state_set("late", vec![1]).is_err());
        kernel.start(&owner()).await.unwrap();
        assert_eq!(kernel.state(&owner()), Some(PluginState::Active));
        assert_eq!(contexts.lock().unwrap().len(), 2);
        let current = contexts.lock().unwrap()[1].clone();
        current.state_set("restarted", vec![2]).unwrap();
        kernel.stop_all().await.unwrap();
    });
}
