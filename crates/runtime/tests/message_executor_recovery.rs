use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::*;
use eve_kernel::{Kernel, KernelServices};
use eve_message_api::*;
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin, RulesJudge};
use eve_plugin_api::*;
use eve_session_api::SessionKey;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::{Notify, watch},
};

const DEADLINE: Duration = Duration::from_secs(3);

fn runtime() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}
fn discard() -> Arc<dyn ControlEventSink> {
    Arc::new(DiscardControlEvents)
}
fn message(key: &GenerationKey, id: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        message_id: id.into(),
        target: key.clone(),
        text: text.into(),
        reply_to: None,
    }
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("已经中断的执行器不得留下永久等待")
}
fn assert_pending(wait: &mut MessageFuture<'static, RouteReport>) {
    assert!(matches!(
        wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
}
struct DropProbe(Arc<AtomicUsize>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Judge {
    calls: AtomicUsize,
    entered: Notify,
    dropped: Arc<AtomicUsize>,
}
impl Judge {
    async fn called(&self, count: usize) {
        bounded(async {
            loop {
                let notified = self.entered.notified();
                if self.calls.load(Ordering::SeqCst) >= count {
                    return;
                }
                notified.await;
            }
        })
        .await;
    }
}
impl RelationJudge for Judge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        Box::pin(async move {
            let _probe = DropProbe(self.dropped.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if input.message.message_id.starts_with("pending") {
                std::future::pending::<()>().await;
            }
            RulesJudge.judge(input).await
        })
    }
}

// 控制状态独立于 Tokio worker，隔离 Control 插件自身的执行器中断问题。
struct Control {
    snapshot: Mutex<ControlSnapshot>,
    done: watch::Sender<Option<ControlReport>>,
    cancels: AtomicUsize,
    replacements: AtomicUsize,
    waits: AtomicUsize,
    waiting: Notify,
    dropped: Arc<AtomicUsize>,
}
impl Control {
    fn new() -> Self {
        Self {
            snapshot: Mutex::new(ControlSnapshot {
                key: GenerationKey {
                    session: SessionKey::new("session", "user").unwrap(),
                    task_id: "task".into(),
                    controller_epoch: [17; 16],
                    generation: 1,
                },
                input_text: "原任务".into(),
                phase: ControlPhase::Generating,
                cancel_requested: false,
                events_retired: false,
                turn_id: Some(7),
                report: None,
            }),
            done: watch::channel(None).0,
            cancels: AtomicUsize::new(0),
            replacements: AtomicUsize::new(0),
            waits: AtomicUsize::new(0),
            waiting: Notify::new(),
            dropped: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn key(&self) -> GenerationKey {
        self.snapshot.lock().unwrap().key.clone()
    }
    fn finish(&self, commit: CommitState, started_tools: Option<u64>) -> ControlReport {
        let mut snapshot = self.snapshot.lock().unwrap();
        let report = ControlReport {
            key: snapshot.key.clone(),
            cancel_requested: snapshot.cancel_requested,
            run: RunReport {
                turn_id: Some(7),
                commit,
                text: Some("真实原代结果".into()),
                transcript: Some(vec![]),
                started_tools,
                tool_results: vec![],
                failure: None,
            },
        };
        snapshot.phase = if matches!(commit, CommitState::Unknown | CommitState::Pending) {
            ControlPhase::Blocked
        } else {
            ControlPhase::Finished
        };
        snapshot.report = Some(report.clone());
        self.done.send_replace(Some(report.clone()));
        report
    }
    async fn wait_started(&self) {
        bounded(async {
            loop {
                let notified = self.waiting.notified();
                if self.waits.load(Ordering::SeqCst) != 0 {
                    return;
                }
                notified.await;
            }
        })
        .await;
    }
}
impl ControlService for Control {
    fn submit(
        &self,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        panic!("消息路由必须按完整代际有条件提交")
    }
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        let mut snapshot = self.snapshot.lock().unwrap();
        if snapshot.key != *expected {
            return Err(ControlError::StaleGeneration);
        }
        if snapshot.phase == ControlPhase::Blocked {
            return Err(ControlError::Blocked);
        }
        assert_eq!(snapshot.phase, ControlPhase::Finished);
        self.replacements.fetch_add(1, Ordering::SeqCst);
        snapshot.key.generation += 1;
        snapshot.key.task_id = input.task_id;
        snapshot.input_text = input.session.text;
        snapshot.phase = ControlPhase::Starting;
        snapshot.report = None;
        snapshot.cancel_requested = false;
        snapshot.events_retired = false;
        self.done.send_replace(None);
        Ok(snapshot.key.clone())
    }
    fn cancel(&self, key: &GenerationKey) -> ControlResult<CancelDisposition> {
        let mut snapshot = self.snapshot.lock().unwrap();
        if snapshot.key != *key {
            return Err(ControlError::StaleGeneration);
        }
        self.cancels.fetch_add(1, Ordering::SeqCst);
        snapshot.cancel_requested = true;
        snapshot.events_retired = true;
        Ok(CancelDisposition::Requested)
    }
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        assert_eq!(*key, self.key());
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.waiting.notify_one();
        let mut done = self.done.subscribe();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            let _probe = DropProbe(dropped);
            loop {
                if let Some(report) = done.borrow_and_update().clone() {
                    return Ok(report);
                }
                done.changed().await.unwrap();
            }
        })
    }
    fn snapshot(&self, key: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        let snapshot = self.snapshot.lock().unwrap();
        assert_eq!(*key, snapshot.key.session);
        Ok(Some(snapshot.clone()))
    }
    fn accepts(&self, _: &ControlEvent) -> bool {
        false
    }
}
struct InjectedControl {
    manifest: PluginManifest,
    service: Arc<Control>,
}
impl Plugin for InjectedControl {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let service = self.service.clone();
        Box::pin(async move {
            ctx.provide_service(
                ServiceId::new(CONTROL_SERVICE_ID)?,
                ControlServiceHandle(service),
            )?;
            Ok(None)
        })
    }
}
struct Rig {
    kernel: Kernel,
    messages: Arc<dyn MessageService>,
    control: Arc<Control>,
    judge: Arc<Judge>,
    _directory: tempfile::TempDir,
}
impl Rig {
    async fn new() -> Self {
        let services = KernelServices::default();
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        let directory = tempfile::tempdir().unwrap();
        kernel
            .register(Box::new(
                ConfigPlugin::new(
                    ConfigBootstrap::new(directory.path(), vec![message_schema()])
                        .with_environment(BTreeMap::new()),
                )
                .unwrap(),
            ))
            .unwrap();
        let control = Arc::new(Control::new());
        kernel
            .register(Box::new(InjectedControl {
                manifest: PluginManifest::new(CONTROL_PLUGIN_ID, "0.1.0").unwrap(),
                service: control.clone(),
            }))
            .unwrap();
        let judge = Arc::new(Judge::default());
        kernel
            .register(Box::new(RelationPlugin::new(judge.clone()).unwrap()))
            .unwrap();
        kernel
            .register(Box::new(MessageRouterPlugin::builtin().unwrap()))
            .unwrap();
        kernel
            .start(&PluginId::new(ROUTER_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let messages = registry
            .get(&ServiceId::new(ROUTER_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<MessageServiceHandle>()
            .unwrap()
            .0
            .clone();
        Self {
            kernel,
            messages,
            control,
            judge,
            _directory: directory,
        }
    }
    fn submit(&self, id: &str, text: &str) -> MessageTicket {
        self.messages
            .submit(message(&self.control.key(), id, text), discard())
            .unwrap()
    }
    async fn report(&self, ticket: &MessageTicket) -> RouteReport {
        bounded(self.messages.wait(ticket)).await.unwrap()
    }
    fn assert_no_actions(&self) {
        assert_eq!(self.control.cancels.load(Ordering::SeqCst), 0);
        assert_eq!(self.control.replacements.load(Ordering::SeqCst), 0);
    }
    async fn stop_after_interruption(&self) {
        // 已丢弃的 worker 仍作为明确的生命周期收尾错误上报。
        assert!(bounded(self.kernel.stop_all()).await.is_err());
    }
}
fn assert_interrupted(report: &RouteReport, ticket: &MessageTicket) {
    assert_eq!(report.message, ticket.message);
    assert_eq!(report.decision, None);
    assert_eq!(report.outcome, RouteOutcome::Blocked { prior: None });
}

#[test]
fn runtime_drop_before_first_poll_finishes_all_waiters_without_replay() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    // enter 只绑定执行器，不驱动 current-thread 任务，确保 worker 从未 poll。
    let ticket = {
        let _entered = first.enter();
        rig.submit("before-poll", "/new 新要求")
    };
    let mut retained = rig.messages.wait(&ticket);
    let second_waiter = rig.messages.wait(&ticket);
    assert_pending(&mut retained);
    assert_eq!(rig.judge.calls.load(Ordering::SeqCst), 0);
    drop(first);

    runtime().block_on(async {
        let (a, b, c) =
            bounded(async { tokio::join!(retained, second_waiter, rig.messages.wait(&ticket)) })
                .await;
        let report = a.unwrap();
        assert_interrupted(&report, &ticket);
        assert_eq!(b.unwrap(), report);
        assert_eq!(c.unwrap(), report);
        let duplicate = rig
            .messages
            .submit(ticket.message.clone(), discard())
            .unwrap();
        assert_eq!(rig.report(&duplicate).await, report);
        for changed in [
            IncomingMessage {
                text: "/cancel".into(),
                ..ticket.message.clone()
            },
            IncomingMessage {
                reply_to: Some("question".into()),
                ..ticket.message.clone()
            },
            IncomingMessage {
                target: GenerationKey {
                    generation: 2,
                    ..ticket.message.target.clone()
                },
                ..ticket.message.clone()
            },
        ] {
            assert_eq!(
                rig.messages.submit(changed, discard()),
                Err(MessageError::Conflict)
            );
        }
        assert_eq!(rig.judge.calls.load(Ordering::SeqCst), 0);
        rig.assert_no_actions();
        let next = rig.submit("fresh", "/continue");
        assert_eq!(rig.report(&next).await.outcome, RouteOutcome::Unchanged);
        assert_eq!(rig.report(&ticket).await, report);
        rig.stop_after_interruption().await;
    });
}

#[test]
fn runtime_drop_during_judgment_drops_resources_and_preserves_target() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    let original = rig.control.snapshot(&rig.control.key().session).unwrap();
    let ticket = first.block_on(async {
        let ticket = rig.submit("pending-judge", "/correct 新要求");
        rig.judge.called(1).await;
        ticket
    });
    let wait = rig.messages.wait(&ticket);
    drop(first);
    assert_eq!(rig.judge.dropped.load(Ordering::SeqCst), 1);
    runtime().block_on(async {
        let report = bounded(wait).await.unwrap();
        assert_interrupted(&report, &ticket);
        assert_eq!(rig.report(&ticket).await, report);
        assert_eq!(
            rig.control
                .snapshot(&ticket.message.target.session)
                .unwrap(),
            original
        );
        rig.assert_no_actions();
        let next = rig.submit("fresh", "/continue");
        assert_eq!(rig.report(&next).await.outcome, RouteOutcome::Unchanged);
        rig.stop_after_interruption().await;
    });
}

#[test]
fn runtime_drop_after_cancel_releases_session_gate_without_restarting_tools() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    let (active, queued) = first.block_on(async {
        let active = rig.submit("active", "/correct 新要求");
        rig.control.wait_started().await;
        let queued = rig.submit("queued", "/new 独立请求");
        rig.judge.called(2).await;
        (active, queued)
    });
    let mut active_wait = rig.messages.wait(&active);
    let mut queued_wait = rig.messages.wait(&queued);
    assert_pending(&mut active_wait);
    assert_pending(&mut queued_wait);
    assert_eq!(rig.control.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(rig.control.waits.load(Ordering::SeqCst), 1);
    drop(first);
    assert_eq!(rig.control.dropped.load(Ordering::SeqCst), 1);
    runtime().block_on(async {
        let (a, b) = bounded(async { tokio::join!(active_wait, queued_wait) }).await;
        let report = a.unwrap();
        assert_interrupted(&report, &active);
        assert_interrupted(&b.unwrap(), &queued);
        assert!(rig.control.snapshot(&active.message.target.session).unwrap().unwrap().cancel_requested);
        assert_eq!(rig.control.replacements.load(Ordering::SeqCst), 0);
        // 新消息能取得会话 gate；未知工具副作用只能澄清，不能自动修订。
        let prior = rig.control.finish(CommitState::Failed, None);
        let retry = rig.submit("explicit-revision", "/correct 再次要求");
        assert!(matches!(rig.report(&retry).await.outcome,
            RouteOutcome::Clarify { reason: ClarifyReason::SideEffects, prior: Some(p), .. } if *p == prior));
        assert_eq!(rig.control.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(rig.report(&active).await, report);
        // 换代后旧消息仍返回自己的中断报告，不重新作用于新代。
        rig.control.snapshot.lock().unwrap().key.generation += 1;
        let duplicate = rig.messages.submit(active.message.clone(), discard()).unwrap();
        assert_eq!(rig.report(&duplicate).await, report);
        let stale = rig.messages.submit(message(&active.message.target, "stale", "/new 旧目标"), discard()).unwrap();
        assert_eq!(rig.report(&stale).await.outcome, RouteOutcome::Stale { prior: None });
        assert_eq!(rig.control.cancels.load(Ordering::SeqCst), 2);
        assert_eq!(rig.control.replacements.load(Ordering::SeqCst), 0);
        rig.stop_after_interruption().await;
    });
}

#[test]
fn completed_replacement_and_prior_report_survive_runtime_drop() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    let prior = rig.control.finish(CommitState::Completed, Some(1));
    let (ticket, report) = first.block_on(async {
        let ticket = rig.submit("new", "/new 独立请求");
        let report = rig.report(&ticket).await;
        (ticket, report)
    });
    let RouteOutcome::Replaced {
        generation,
        prior: reported,
    } = &report.outcome
    else {
        panic!("预期替代成功：{report:?}");
    };
    assert_eq!(**reported, prior);
    assert_eq!(*generation, rig.control.key());
    let retained = rig.messages.wait(&ticket);
    drop(first);
    runtime().block_on(async {
        assert_eq!(bounded(retained).await.unwrap(), report);
        assert_eq!(rig.report(&ticket).await, report);
        let duplicate = rig
            .messages
            .submit(ticket.message.clone(), discard())
            .unwrap();
        assert_eq!(rig.report(&duplicate).await, report);
        assert_eq!(rig.control.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(rig.control.replacements.load(Ordering::SeqCst), 1);
        bounded(rig.kernel.stop_all()).await.unwrap();
    });
}

#[test]
fn ordinary_shutdown_report_is_not_overwritten_by_guard() {
    runtime().block_on(async {
        let rig = Rig::new().await;
        let ticket = rig.submit("pending-close", "/new 新请求");
        rig.judge.called(1).await;
        let retained = rig.messages.wait(&ticket);
        bounded(rig.kernel.stop_all()).await.unwrap();
        let report = bounded(retained).await.unwrap();
        assert_eq!(report.outcome, RouteOutcome::Stopped { prior: None });
        assert_eq!(
            rig.messages.wait(&ticket).await,
            Err(MessageError::Unavailable)
        );
        rig.assert_no_actions();
    });
}

#[test]
fn submitting_with_closed_runtime_handle_still_finishes_ticket() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    let handle = first.handle().clone();
    drop(first);
    let ticket = {
        let _entered = handle.enter();
        rig.submit("closed-handle", "/new 新请求")
    };
    runtime().block_on(async {
        assert_interrupted(&rig.report(&ticket).await, &ticket);
        assert_eq!(rig.judge.calls.load(Ordering::SeqCst), 0);
        rig.assert_no_actions();
        rig.stop_after_interruption().await;
    });
}

#[test]
fn unknown_control_report_remains_authoritative_after_message_interruption() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    let interrupted = first.block_on(async {
        let ticket = rig.submit("waiting", "/new 独立请求");
        rig.control.wait_started().await;
        ticket
    });
    drop(first);
    let prior = rig.control.finish(CommitState::Unknown, None);
    runtime().block_on(async {
        assert_interrupted(&rig.report(&interrupted).await, &interrupted);
        let next = rig.submit("new-after-interruption", "/new 明确新请求");
        assert_eq!(
            rig.report(&next).await.outcome,
            RouteOutcome::Blocked {
                prior: Some(Box::new(prior))
            }
        );
        assert_eq!(rig.control.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(rig.control.cancels.load(Ordering::SeqCst), 1);
        rig.stop_after_interruption().await;
    });
}

#[test]
fn shutdown_after_executor_loss_waits_for_remaining_cancellation_reports() {
    let first = runtime();
    let rig = first.block_on(Rig::new());
    let interrupted = {
        let _entered = first.enter();
        rig.submit("interrupted", "/continue")
    };
    drop(first);
    runtime().block_on(async {
        assert_interrupted(&rig.report(&interrupted).await, &interrupted);
        let ticket = rig.submit("live-cancellation", "/new 新请求");
        rig.control.wait_started().await;
        let retained = rig.messages.wait(&ticket);
        let stopping_kernel = rig.kernel.clone();
        let stopping = tokio::spawn(async move { stopping_kernel.stop_all().await });
        // 等到准入关闭，再确认正在等待真实原代报告，不能用中断报告冒充收尾。
        bounded(async {
            loop {
                match rig.messages.submit(ticket.message.clone(), discard()) {
                    Err(MessageError::Unavailable) => break,
                    Ok(_) => tokio::task::yield_now().await,
                    other => panic!("关闭期间准入返回异常：{other:?}"),
                }
            }
        })
        .await;
        assert!(!stopping.is_finished());
        assert_eq!(rig.control.dropped.load(Ordering::SeqCst), 0);
        let prior = rig.control.finish(CommitState::Failed, Some(1));
        assert!(bounded(stopping).await.unwrap().is_err());
        let report = bounded(retained).await.unwrap();
        assert_eq!(
            report.outcome,
            RouteOutcome::Stopped {
                prior: Some(Box::new(prior))
            }
        );
        assert_eq!(rig.control.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(rig.control.replacements.load(Ordering::SeqCst), 0);
    });
}
