#[path = "support/session.rs"]
mod fixture;

use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{LlmHostConfig, SessionControlRunner};
use eve_session_api::*;
use serde_json::Value;
use std::{
    future::{Future, poll_fn},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

fn executor() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

fn request(session: &str, task: &str) -> ControlInput {
    ControlInput {
        session: fixture::input(session, "执行器中断验收"),
        task_id: task.into(),
    }
}

async fn install(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
    runner: Arc<dyn ControlRunner>,
    dependencies: Vec<PluginDependency>,
) -> Arc<dyn ControlService> {
    kernel
        .register(Box::new(ControlPlugin::new(runner, dependencies).unwrap()))
        .unwrap();
    kernel.start(&fixture::id(CONTROL_PLUGIN_ID)).await.unwrap();
    registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone()
}

async fn done(wait: impl Future<Output = ControlResult<ControlReport>>) -> ControlReport {
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("worker 已被执行器丢弃，wait 必须返回明确终态")
        .unwrap()
}

async fn install_session(rig: &fixture::Rig) -> Arc<dyn ControlService> {
    install(
        &rig.kernel,
        &rig.registry,
        Arc::new(SessionControlRunner::new(rig.host.clone())),
        [fixture::OWNER, SESSION_PLUGIN_ID]
            .into_iter()
            .map(|owner| PluginDependency {
                id: fixture::id(owner),
                requirement: Some("^0.1".into()),
            })
            .collect(),
    )
    .await
}

async fn subscribe_all(
    waiters: Vec<ControlFuture<'static, ControlReport>>,
) -> Vec<JoinHandle<ControlResult<ControlReport>>> {
    let expected = waiters.len();
    let subscribed = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let tasks = waiters
        .into_iter()
        .map(|mut waiter| {
            let (subscribed, entered) = (subscribed.clone(), entered.clone());
            tokio::spawn(async move {
                let mut announced = false;
                poll_fn(|context| {
                    let result = waiter.as_mut().poll(context);
                    if !announced {
                        assert!(result.is_pending());
                        announced = true;
                        subscribed.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                    }
                    result
                })
                .await
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let changed = entered.notified();
            if subscribed.load(Ordering::SeqCst) == expected {
                break;
            }
            changed.await;
        }
    })
    .await
    .unwrap();
    tasks
}

fn assert_interrupted(
    control: &dyn ControlService,
    key: &GenerationKey,
    report: &ControlReport,
    cancel_requested: bool,
) {
    assert_eq!(&report.key, key);
    assert_eq!(report.run.commit, CommitState::Unknown);
    assert_eq!(report.run.started_tools, None);
    assert!(report.run.failure.is_some());
    assert_eq!(report.cancel_requested, cancel_requested);
    let snapshot = control.snapshot(&key.session).unwrap().unwrap();
    assert_eq!(snapshot.phase, ControlPhase::Blocked);
    assert!(snapshot.events_retired);
    assert_eq!(snapshot.report.as_ref(), Some(report));
    assert_eq!(control.cancel(key), Ok(CancelDisposition::AlreadyFinished));
    assert_eq!(
        control.snapshot(&key.session).unwrap().unwrap().report,
        Some(report.clone())
    );
    assert_eq!(
        control.submit(
            request(&key.session.session_id, "replacement"),
            Arc::new(DiscardControlEvents)
        ),
        Err(ControlError::Blocked)
    );
    assert_eq!(
        control.submit_if_current(
            key,
            request(&key.session.session_id, "replacement"),
            Arc::new(DiscardControlEvents)
        ),
        Err(ControlError::Blocked)
    );
}

struct NeverPolled(Arc<AtomicUsize>);

impl ControlRunner for NeverPolled {
    fn run<'a>(&'a self, _: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

#[test]
fn executor_shutdown_before_first_poll_resolves_all_waiters_and_blocks_replacement() {
    let a = executor();
    let b = executor();
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let starts = Arc::new(AtomicUsize::new(0));
    let control = a.block_on(install(
        &kernel,
        &registry,
        Arc::new(NeverPolled(starts.clone())),
        vec![],
    ));
    // enter 只提供 spawn 句柄，不驱动 A；已准入 worker 确定没有被首次 poll。
    let key = {
        let _entered = a.enter();
        control
            .submit(request("unpolled", "first"), Arc::new(DiscardControlEvents))
            .unwrap()
    };
    let mut waiters = b.block_on(subscribe_all(vec![
        control.wait(&key),
        control.wait(&key),
        control.wait(&key),
    ]));
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    drop(a);
    assert_eq!(starts.load(Ordering::SeqCst), 0);

    b.block_on(async {
        let report = done(async { waiters.remove(0).await.unwrap() }).await;
        for waiter in waiters {
            assert_eq!(done(async { waiter.await.unwrap() }).await, report);
        }
        assert_eq!(done(control.wait(&key)).await, report);
        assert_interrupted(&*control, &key, &report, false);
        assert_eq!(report.run.turn_id, None);
        tokio::time::timeout(Duration::from_secs(2), kernel.stop_all())
            .await
            .expect("停止已失去执行器的控制器不得挂起")
            .unwrap();
        assert_eq!(
            control.snapshot(&key.session),
            Err(ControlError::Unavailable)
        );
    });
}

#[derive(Default)]
struct Events(Mutex<Vec<ControlEvent>>);

impl ControlEventSink for Events {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        self.0.lock().unwrap().push(event);
        Box::pin(async { Ok(()) })
    }
}

struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct WaitingProvider {
    entered: Notify,
    in_flight: Arc<AtomicUsize>,
}

impl LlmProvider for WaitingProvider {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let _in_flight = InFlight::new(&self.in_flight);
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}

struct FaultDisk {
    inner: FileStateStore,
    fail_writes: AtomicBool,
}

impl StateStore for FaultDisk {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }

    fn set(&self, namespace: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(PluginError::State("验收注入取消记录写入失败".into()));
        }
        self.inner.set(namespace, key, bytes)
    }
}

fn interrupted_session_status(failure_commit_failed: bool) -> SessionTurnStatus {
    if failure_commit_failed {
        SessionTurnStatus::Pending
    } else {
        SessionTurnStatus::Failed {
            failure: SessionFailure {
                code: SessionFailureCode::Cancelled,
                started_tools: None,
            },
        }
    }
}

#[test]
fn provider_executor_shutdown_preserves_durable_state_and_releases_directory_and_handles() {
    for (cancel_requested, failure_commit_failed) in [(false, false), (true, false), (false, true)]
    {
        let directory = tempfile::tempdir().unwrap();
        let a = executor();
        let b = executor();
        let state = Arc::new(FaultDisk {
            inner: FileStateStore::open(directory.path()).unwrap(),
            fail_writes: AtomicBool::new(false),
        });
        let state_released = Arc::downgrade(&state);
        let provider = Arc::new(WaitingProvider::default());
        let provider_released = Arc::downgrade(&provider);
        let rig = a.block_on(fixture::Rig::new(
            provider.clone(),
            state.clone(),
            LlmHostConfig::default(),
        ));
        let control = a.block_on(install_session(&rig));
        let control_released = Arc::downgrade(&control);
        let events = Arc::new(Events::default());
        let events_released = Arc::downgrade(&events);
        let key = a.block_on(async {
            let key = control
                .submit(request("provider", "first"), events.clone())
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
                .await
                .unwrap();
            key
        });
        assert_eq!(provider.in_flight.load(Ordering::SeqCst), 1);
        let before = control.snapshot(&key.session).unwrap().unwrap();
        assert_eq!(before.phase, ControlPhase::Generating);
        assert_eq!(
            rig.snapshot("provider").turns[0].status,
            SessionTurnStatus::Pending
        );
        let queued_events = events.0.lock().unwrap().clone();
        assert!(!queued_events.is_empty());
        assert!(queued_events.iter().all(|event| control.accepts(event)));
        drop(events);
        let mut waiters = b.block_on(subscribe_all(vec![control.wait(&key), control.wait(&key)]));
        if cancel_requested {
            // A 已停止驱动，取消请求已准入但 runner 还没有处理它。
            assert_eq!(control.cancel(&key), Ok(CancelDisposition::Requested));
        }
        state
            .fail_writes
            .store(failure_commit_failed, Ordering::SeqCst);
        drop(a);
        state.fail_writes.store(false, Ordering::SeqCst);
        drop(state);
        assert_eq!(provider.in_flight.load(Ordering::SeqCst), 0);
        assert!(events_released.upgrade().is_none());

        b.block_on(async {
            let report = done(async { waiters.remove(0).await.unwrap() }).await;
            assert_eq!(
                done(async { waiters.remove(0).await.unwrap() }).await,
                report
            );
            assert_interrupted(&*control, &key, &report, cancel_requested);
            assert_eq!(
                control.snapshot(&key.session).unwrap().unwrap().turn_id,
                before.turn_id
            );
            assert!(queued_events.iter().all(|event| !control.accepts(event)));
            assert_eq!(
                rig.snapshot("provider").turns[0].status,
                interrupted_session_status(failure_commit_failed)
            );
            assert!(rig.snapshot("provider").history().is_empty());
            let retained = control.wait(&key);
            rig.stop().await;
            assert_eq!(done(retained).await, report);
            assert_eq!(
                control.snapshot(&key.session),
                Err(ControlError::Unavailable)
            );
            assert!(
                rig.registry
                    .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
                    .unwrap()
                    .is_none()
            );
            // 插件注册仍持有组合层 runner；显式卸载后才能释放其 Kernel 句柄。
            rig.kernel
                .unregister(&fixture::id(CONTROL_PLUGIN_ID))
                .unwrap();
        });
        drop(control);
        drop(rig);
        drop(provider);
        assert!(control_released.upgrade().is_none());
        assert!(provider_released.upgrade().is_none());
        assert!(state_released.upgrade().is_none());

        // 重新打开真实目录：已落盘失败必须保留；失败记录没写成才恢复 Pending。
        let recovered = b.block_on(fixture::Rig::new(
            fixture::Provider::new(vec![]),
            Arc::new(FileStateStore::open(directory.path()).unwrap()),
            LlmHostConfig::default(),
        ));
        assert_eq!(recovered.snapshot("provider").turns.len(), 1);
        assert_eq!(
            recovered.snapshot("provider").turns[0].status,
            if failure_commit_failed {
                SessionTurnStatus::Interrupted
            } else {
                interrupted_session_status(false)
            }
        );
        assert!(recovered.snapshot("provider").history().is_empty());
        assert_eq!(recovered.starts.load(Ordering::SeqCst), 0);
        b.block_on(recovered.stop());
    }
}

#[derive(Default)]
struct WaitingTool {
    starts: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    entered: Notify,
    shutdown_barriers: Option<Arc<ShutdownBarriers>>,
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) {
        let open = self.open.lock().unwrap();
        let (open, _) = self
            .changed
            .wait_timeout_while(open, Duration::from_secs(10), |open| !*open)
            .unwrap();
        assert!(*open, "工具退出屏障等待超时");
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

#[derive(Default)]
struct ShutdownBarriers {
    polling: Gate,
    dropping: Gate,
    drop_entered: Notify,
}

struct ReleaseBarriers(Arc<ShutdownBarriers>);

impl Drop for ReleaseBarriers {
    fn drop(&mut self) {
        self.0.polling.release();
        self.0.dropping.release();
    }
}

struct ToolDropBarrier(Arc<ShutdownBarriers>);

impl Drop for ToolDropBarrier {
    fn drop(&mut self) {
        self.0.drop_entered.notify_one();
        self.0.dropping.wait();
    }
}

impl Tool for WaitingTool {
    fn definition(&self) -> ToolDefinition {
        fixture::ReceiptTool {
            starts: self.starts.clone(),
        }
        .definition()
    }
    fn validate_arguments(&self, _: &Value) -> Result<(), ToolValidationError> {
        Ok(())
    }
    fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let _in_flight = InFlight::new(&self.in_flight);
            let _dropping = self.shutdown_barriers.clone().map(ToolDropBarrier);
            self.entered.notify_one();
            if let Some(barriers) = &self.shutdown_barriers {
                barriers.polling.wait();
            }
            std::future::pending().await
        })
    }
}

#[test]
fn tool_executor_shutdown_reports_unknown_before_tool_destructor_finishes() {
    let a = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let b = executor();
    let mut calls = fixture::calls().unwrap();
    if let ModelResponse::ToolCalls { calls } = &mut calls {
        calls.truncate(1);
    }
    let rig = a.block_on(fixture::Rig::new(
        fixture::Provider::new(vec![fixture::Step::new(Ok(calls))]),
        Arc::new(fixture::FaultStore::default()),
        LlmHostConfig::default(),
    ));
    let barriers = Arc::new(ShutdownBarriers::default());
    let _release_on_failure = ReleaseBarriers(barriers.clone());
    let tool = Arc::new(WaitingTool {
        shutdown_barriers: Some(barriers.clone()),
        ..WaitingTool::default()
    });
    a.block_on(async {
        rig.kernel.stop(&fixture::id(fixture::OWNER)).await.unwrap();
        rig.kernel.unregister(&fixture::id(fixture::OWNER)).unwrap();
        rig.kernel
            .register(Box::new(fixture::ServicesPlugin {
                manifest: PluginManifest::new(fixture::OWNER, "0.1.0").unwrap(),
                context: rig.context.clone(),
                tool: tool.clone(),
            }))
            .unwrap();
        rig.kernel
            .start(&fixture::id(fixture::OWNER))
            .await
            .unwrap();
    });
    let control = a.block_on(install_session(&rig));
    let events = Arc::new(Events::default());
    let key = a.block_on(async {
        let key = control
            .submit(request("tool", "first"), events.clone())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), tool.entered.notified())
            .await
            .unwrap();
        key
    });
    assert_eq!(tool.starts.load(Ordering::SeqCst), 1);
    assert_eq!(tool.in_flight.load(Ordering::SeqCst), 1);
    assert_eq!(
        control.snapshot(&key.session).unwrap().unwrap().phase,
        ControlPhase::Tools
    );
    let queued_events = events.0.lock().unwrap().clone();
    assert!(
        queued_events
            .iter()
            .any(|event| matches!(event.event.kind, TurnEventKind::ToolBatchStarted { .. }))
    );
    let shutting_down = std::thread::spawn(move || drop(a));
    let report = b.block_on(done(control.wait(&key)));
    assert_eq!(tool.in_flight.load(Ordering::SeqCst), 1);
    assert!(!shutting_down.is_finished());
    barriers.polling.release();
    b.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), barriers.drop_entered.notified())
            .await
            .expect("工具 Future 必须进入析构屏障");
        assert_eq!(done(control.wait(&key)).await, report);
        assert_eq!(tool.in_flight.load(Ordering::SeqCst), 1);
        assert!(!shutting_down.is_finished());
        assert_interrupted(&*control, &key, &report, false);
        assert!(report.run.tool_results.is_empty());
        assert_eq!(tool.starts.load(Ordering::SeqCst), 1);
        assert!(queued_events.iter().all(|event| !control.accepts(event)));
        assert_eq!(
            rig.snapshot("tool").turns[0].status,
            interrupted_session_status(false)
        );
    });
    barriers.dropping.release();
    shutting_down.join().unwrap();
    assert_eq!(tool.in_flight.load(Ordering::SeqCst), 0);
    b.block_on(async {
        assert_eq!(done(control.wait(&key)).await, report);
        rig.stop().await;
        rig.kernel
            .unregister(&fixture::id(CONTROL_PLUGIN_ID))
            .unwrap();
    });
}

#[test]
fn executor_shutdown_preserves_completed_report_and_retained_older_generation_wait() {
    let a = executor();
    let b = executor();
    let provider = fixture::Provider::new(vec![
        fixture::Step::new(fixture::final_response("已持久化完成")),
        fixture::Step::blocked(Arc::new(Notify::new())),
    ]);
    let rig = a.block_on(fixture::Rig::new(
        provider.clone(),
        Arc::new(fixture::FaultStore::default()),
        LlmHostConfig::default(),
    ));
    let control = a.block_on(install_session(&rig));
    let (first, completed) = a.block_on(async {
        let first = control
            .submit(request("finished", "first"), Arc::new(DiscardControlEvents))
            .unwrap();
        let completed = done(control.wait(&first)).await;
        (first, completed)
    });
    assert_eq!(completed.run.commit, CommitState::Completed);
    assert_eq!(completed.run.text.as_deref(), Some("已持久化完成"));
    let retained = control.wait(&first);
    // 已完成 worker 的析构保护不能在完成时写入 Unknown。
    assert_eq!(a.block_on(done(control.wait(&first))), completed);
    let second = a.block_on(async {
        let key = control
            .submit(
                request("finished", "second"),
                Arc::new(DiscardControlEvents),
            )
            .unwrap();
        provider.wait_requests(2).await;
        key
    });
    drop(a);
    b.block_on(async {
        assert_eq!(done(retained).await, completed);
        assert_eq!(
            control.wait(&first).await,
            Err(ControlError::StaleGeneration)
        );
        let report = done(control.wait(&second)).await;
        assert_interrupted(&*control, &second, &report, false);
        let snapshot = rig.snapshot("finished");
        assert!(matches!(
            snapshot.turns[0].status,
            SessionTurnStatus::Completed { .. }
        ));
        assert_eq!(snapshot.turns[1].status, interrupted_session_status(false));
        assert_eq!(
            snapshot.history().last().unwrap().text.as_deref(),
            Some("已持久化完成")
        );
        rig.stop().await;
        rig.kernel
            .unregister(&fixture::id(CONTROL_PLUGIN_ID))
            .unwrap();
    });
}
