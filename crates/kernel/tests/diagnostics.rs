use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::*;
use std::sync::{Arc, Mutex};
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
    kernel
        .register(Box::new(TestPlugin { manifest, start }))
        .unwrap();
}

fn register_cleanup(
    kernel: &Kernel,
    id: &str,
    dependencies: &[&str],
    calls: Arc<Mutex<Vec<String>>>,
    fail: bool,
) {
    let name = id.to_owned();
    register(kernel, id, dependencies, move |_| {
        let (calls, name) = (calls.clone(), name.clone());
        Ok(Some(cleanup(move || async move {
            calls.lock().unwrap().push(name);
            if fail {
                Err(PluginError::Cleanup("清理失败的原始原因".into()))
            } else {
                Ok(())
            }
        })))
    });
}

fn shutdown_errors(error: PluginError) -> Vec<PluginStopError> {
    match error {
        PluginError::Shutdown(errors) => errors,
        other => panic!("应返回结构化停止错误：{other:?}"),
    }
}

#[tokio::test]
async fn inspector_returns_sorted_detached_snapshots_and_validates_unknown_plugins() {
    let kernel = Kernel::new();
    register(&kernel, "z-owner", &[], |ctx| {
        ctx.spawn_task(TaskSpec::new(
            "等待执行",
            TaskMode::Background,
            TaskSchedule::After(Duration::from_secs(3600)),
            Arc::new(|_| Box::pin(async { Ok(()) })),
        )?)?;
        Ok(None)
    });
    register(&kernel, "a-owner", &[], |_| Ok(None));
    register(&kernel, "failed", &[], |_| {
        Err(PluginError::Lifecycle("启动失败".into()))
    });
    let inspector: &dyn RuntimeInspector = &kernel;
    let before = inspector.plugins().unwrap();
    assert_eq!(
        before
            .iter()
            .map(|status| status.info.id.as_str())
            .collect::<Vec<_>>(),
        ["a-owner", "failed", "z-owner"]
    );
    assert!(
        before
            .iter()
            .all(|status| status.state == PluginState::Registered)
    );

    kernel.start(&pid("z-owner")).await.unwrap();
    assert!(kernel.start(&pid("failed")).await.is_err());
    let mut snapshot = kernel.plugins().unwrap();
    assert_eq!(snapshot[1].state, PluginState::Failed);
    assert_eq!(snapshot[2].state, PluginState::Active);
    snapshot[2].info.id = pid("pretend-owner");
    snapshot[2].info.version = Version::new("99.0.0").unwrap();
    snapshot[2].state = PluginState::Failed;
    assert_eq!(before[2].state, PluginState::Registered);
    let current = inspector.plugins().unwrap();
    assert_eq!(current[2].info.id, pid("z-owner"));
    assert_eq!(current[2].info.version.as_str(), "0.1.0");
    assert_eq!(current[2].state, PluginState::Active);

    let mut tasks = inspector.plugin_tasks(&pid("z-owner")).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].owner, pid("z-owner"));
    tasks[0].name = "修改返回副本".into();
    assert_eq!(
        inspector.plugin_tasks(&pid("z-owner")).unwrap()[0].name,
        "等待执行"
    );
    assert!(inspector.plugin_tasks(&pid("a-owner")).unwrap().is_empty());
    assert!(
        matches!(inspector.plugin_tasks(&pid("unknown")), Err(PluginError::InvalidLifecycle { plugin, .. }) if plugin == pid("unknown"))
    );
    assert_eq!(
        inspector.plugin_manifest(&pid("z-owner")).unwrap(),
        PluginManifest::new("z-owner", "0.1.0").unwrap()
    );
    assert!(matches!(
        inspector.plugin_manifest(&pid("unknown")),
        Err(PluginError::PluginNotFound(id)) if id == pid("unknown")
    ));
    assert!(
        matches!(kernel.tasks(&pid("unknown")), Err(PluginError::InvalidLifecycle { plugin, .. }) if plugin == pid("unknown"))
    );
    assert!(
        matches!(kernel.stop(&pid("unknown")).await, Err(PluginError::InvalidLifecycle { plugin, .. }) if plugin == pid("unknown"))
    );

    kernel.stop(&pid("z-owner")).await.unwrap();
    assert_eq!(inspector.plugins().unwrap()[2].state, PluginState::Stopped);
    assert!(inspector.plugin_tasks(&pid("z-owner")).unwrap().is_empty());
}

struct GatedPlugin {
    manifest: PluginManifest,
    starting: Arc<Notify>,
    proceed_start: Arc<Notify>,
    stopping: Arc<Notify>,
    proceed_stop: Arc<Notify>,
}

impl Plugin for GatedPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, _: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            self.starting.notify_one();
            self.proceed_start.notified().await;
            let (stopping, proceed) = (self.stopping.clone(), self.proceed_stop.clone());
            Ok(Some(cleanup(move || async move {
                stopping.notify_one();
                proceed.notified().await;
                Ok(())
            })))
        })
    }
}

#[tokio::test]
async fn inspector_observes_starting_waiting_and_stopping_without_lifecycle_lock() {
    let kernel = Kernel::new();
    let starting = Arc::new(Notify::new());
    let stopping = Arc::new(Notify::new());
    let proceed_start = Arc::new(Notify::new());
    let proceed_stop = Arc::new(Notify::new());
    kernel
        .register(Box::new(GatedPlugin {
            manifest: PluginManifest::new("a-provider", "0.1.0").unwrap(),
            starting: starting.clone(),
            proceed_start: proceed_start.clone(),
            stopping: stopping.clone(),
            proceed_stop: proceed_stop.clone(),
        }))
        .unwrap();
    register(&kernel, "z-consumer", &["a-provider"], |_| Ok(None));
    let starting_kernel = kernel.clone();
    let start = tokio::spawn(async move { starting_kernel.start(&pid("z-consumer")).await });
    tokio::time::timeout(Duration::from_secs(2), starting.notified())
        .await
        .unwrap();
    let statuses = kernel.plugins().unwrap();
    assert_eq!(statuses[0].state, PluginState::Starting);
    assert_eq!(statuses[1].state, PluginState::WaitingDependencies);
    proceed_start.notify_one();
    start.await.unwrap().unwrap();
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|status| status.state == PluginState::Active)
    );

    let stopping_kernel = kernel.clone();
    let stop = tokio::spawn(async move { stopping_kernel.stop(&pid("a-provider")).await });
    tokio::time::timeout(Duration::from_secs(2), stopping.notified())
        .await
        .unwrap();
    let statuses = kernel.plugins().unwrap();
    assert_eq!(statuses[0].state, PluginState::Stopping);
    assert_eq!(statuses[1].state, PluginState::Stopped);
    proceed_stop.notify_one();
    stop.await.unwrap().unwrap();
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|status| status.state == PluginState::Stopped)
    );
}

#[tokio::test]
async fn recursive_stop_reports_shared_consumer_errors_once_and_cleans_each_plugin_once() {
    let kernel = Kernel::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    register_cleanup(&kernel, "a-base", &[], calls.clone(), true);
    register_cleanup(&kernel, "b-left", &["a-base"], calls.clone(), true);
    register_cleanup(&kernel, "c-right", &["a-base"], calls.clone(), true);
    register_cleanup(
        &kernel,
        "d-shared",
        &["b-left", "c-right"],
        calls.clone(),
        true,
    );
    kernel.start_all().await.unwrap();
    let errors = shutdown_errors(kernel.stop(&pid("a-base")).await.unwrap_err());
    assert_eq!(errors.len(), 4);
    let mut owners = errors
        .iter()
        .map(|error| error.plugin.as_str())
        .collect::<Vec<_>>();
    owners.sort();
    assert_eq!(owners, ["a-base", "b-left", "c-right", "d-shared"]);
    for error in errors {
        assert_eq!(error.stage, StopStage::Cleanup);
        assert!(
            matches!(*error.error, PluginError::Cleanup(ref reason) if reason.contains("清理失败的原始原因"))
        );
    }
    assert_eq!(
        *calls.lock().unwrap(),
        ["d-shared", "b-left", "c-right", "a-base"]
    );
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|status| status.state == PluginState::Stopped)
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn stop_all_collects_independent_failures_and_still_stops_healthy_plugins() {
    let kernel = Kernel::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    for id in ["z-failed", "a-failed", "m-healthy", "b-failed"] {
        register_cleanup(&kernel, id, &[], calls.clone(), id != "m-healthy");
    }
    kernel.start_all().await.unwrap();
    let errors = shutdown_errors(kernel.stop_all().await.unwrap_err());
    assert_eq!(errors.len(), 3);
    assert_eq!(
        errors
            .iter()
            .map(|error| error.plugin.as_str())
            .collect::<Vec<_>>(),
        ["a-failed", "b-failed", "z-failed"]
    );
    assert!(errors.iter().all(|error| error.stage == StopStage::Cleanup));
    assert_eq!(
        *calls.lock().unwrap(),
        ["a-failed", "b-failed", "m-healthy", "z-failed"]
    );
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|status| status.state == PluginState::Stopped)
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 4);
}

#[derive(Clone, Copy)]
enum BackendFault {
    ShutdownReport,
    ShutdownError,
    InspectionError,
    ResidualTask,
}

struct FaultyTasks(BackendFault);

impl TaskManager for FaultyTasks {
    fn spawn(&self, _: PluginId, _: TaskSpec) -> PluginResult<TaskId> {
        Err(PluginError::Task("此测试后端不执行任务".into()))
    }

    fn run_foreground(
        self: Arc<Self>,
        _: PluginId,
        _: TaskSpec,
    ) -> PluginResult<PluginFuture<'static, TaskRunReport>> {
        Err(PluginError::Task("此测试后端不执行任务".into()))
    }

    fn list(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>> {
        match self.0 {
            BackendFault::InspectionError => Err(PluginError::Task("退出检查后端不可用".into())),
            BackendFault::ResidualTask => Ok(vec![TaskInfo {
                id: TaskId::new(7),
                owner: owner.clone(),
                name: "仍未退出".into(),
                task_type: TaskTypeId::new("test.residual").unwrap(),
                mode: TaskMode::Background,
                schedule: TaskScheduleInfo::Immediate,
                state: TaskState::Stopping,
                runs: 0,
                exited: false,
            }]),
            _ => Ok(Vec::new()),
        }
    }

    fn shutdown(
        self: Arc<Self>,
        _: &PluginId,
        _: Duration,
        _: Duration,
    ) -> PluginFuture<'static, TaskShutdownReport> {
        Box::pin(async move {
            match self.0 {
                BackendFault::ShutdownReport => Ok(TaskShutdownReport {
                    errors: vec!["任务停止的原始原因".into()],
                    ..Default::default()
                }),
                BackendFault::ShutdownError => Err(PluginError::Task("任务停止的原始原因".into())),
                _ => Ok(TaskShutdownReport::default()),
            }
        })
    }
}

#[tokio::test]
async fn task_and_cleanup_failures_keep_separate_stages_and_original_causes() {
    for fault in [BackendFault::ShutdownReport, BackendFault::ShutdownError] {
        let kernel = Kernel::with_services(KernelServices {
            tasks: Arc::new(FaultyTasks(fault)),
            ..Default::default()
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        register_cleanup(&kernel, "broken", &[], calls.clone(), true);
        kernel.start_all().await.unwrap();
        let errors = shutdown_errors(kernel.stop_all().await.unwrap_err());
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|error| error.plugin == pid("broken")));
        let task_error = errors
            .iter()
            .find(|error| error.stage == StopStage::Tasks)
            .unwrap();
        assert!(
            matches!(*task_error.error, PluginError::Task(ref reason) if reason.contains("任务停止的原始原因"))
        );
        let cleanup_error = errors
            .iter()
            .find(|error| error.stage == StopStage::Cleanup)
            .unwrap();
        assert!(
            matches!(*cleanup_error.error, PluginError::Cleanup(ref reason) if reason.contains("清理失败的原始原因"))
        );
        assert_eq!(*calls.lock().unwrap(), ["broken"]);
        assert_eq!(kernel.state(&pid("broken")), Some(PluginState::Stopped));
    }
}

#[tokio::test]
async fn failed_or_inconsistent_exit_inspection_is_reported_and_prevents_restart() {
    for fault in [BackendFault::InspectionError, BackendFault::ResidualTask] {
        let kernel = Kernel::with_services(KernelServices {
            tasks: Arc::new(FaultyTasks(fault)),
            ..Default::default()
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        register_cleanup(&kernel, "unchecked", &[], calls.clone(), false);
        kernel.start_all().await.unwrap();
        let errors = shutdown_errors(kernel.stop_all().await.unwrap_err());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].plugin, pid("unchecked"));
        assert_eq!(errors[0].stage, StopStage::Inspection);
        assert_eq!(kernel.state(&pid("unchecked")), Some(PluginState::Failed));
        assert_eq!(*calls.lock().unwrap(), ["unchecked"]);
        let inspector: &dyn RuntimeInspector = &kernel;
        match fault {
            BackendFault::InspectionError => {
                assert!(errors[0].error.to_string().contains("退出检查后端不可用"));
                assert!(
                    inspector
                        .plugin_tasks(&pid("unchecked"))
                        .unwrap_err()
                        .to_string()
                        .contains("退出检查后端不可用")
                );
            }
            BackendFault::ResidualTask => {
                let tasks = inspector.plugin_tasks(&pid("unchecked")).unwrap();
                assert_eq!(tasks[0].id, TaskId::new(7));
                assert!(!tasks[0].exited);
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            kernel.start(&pid("unchecked")).await,
            Err(PluginError::InvalidLifecycle { .. })
        ));
    }
}
