#[path = "support/session.rs"]
mod fixture;
use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{LlmHostConfig, SessionControlRunner};
use eve_session_api::*;
use fixture::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

fn request(session: &str, task: &str, text: &str) -> ControlInput {
    ControlInput {
        session: input(session, text),
        task_id: task.into(),
    }
}
async fn install(rig: &Rig) -> Arc<dyn ControlService> {
    rig.kernel
        .register(Box::new(
            ControlPlugin::new(
                Arc::new(SessionControlRunner::new(rig.host.clone())),
                [OWNER, SESSION_PLUGIN_ID]
                    .into_iter()
                    .map(|owner| PluginDependency {
                        id: id(owner),
                        requirement: Some("^0.1".into()),
                    })
                    .collect(),
            )
            .unwrap(),
        ))
        .unwrap();
    rig.kernel.start(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    service(rig)
}
fn service(rig: &Rig) -> Arc<dyn ControlService> {
    rig.registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone()
}
async fn done(control: &dyn ControlService, key: &GenerationKey) -> ControlReport {
    tokio::time::timeout(Duration::from_secs(3), control.wait(key))
        .await
        .unwrap()
        .unwrap()
}
struct Channel(mpsc::Sender<ControlEvent>);
impl ControlEventSink for Channel {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move { self.0.send(event).await.map_err(|_| LlmError::Cancelled) })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            self.0.closed().await;
            Ok(())
        })
    }
}
fn channel() -> (Arc<dyn ControlEventSink>, mpsc::Receiver<ControlEvent>) {
    let (tx, rx) = mpsc::channel(16);
    (Arc::new(Channel(tx)), rx)
}
fn discard() -> Arc<dyn ControlEventSink> {
    Arc::new(DiscardControlEvents)
}
fn single_call() -> Result<ModelResponse, LlmError> {
    let mut response = calls().unwrap();
    if let ModelResponse::ToolCalls { calls } = &mut response {
        calls.truncate(1);
    }
    Ok(response)
}

#[tokio::test]
async fn normal_tools_complete_and_old_completion_wait_survives_replacement() {
    let p = Provider::new(vec![
        Step::new(calls()),
        Step::new(final_response("回执完成")),
        Step::new(final_response("下一任务")),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let control = install(&rig).await;
    let (sink, mut rx) = channel();
    let first = control
        .submit(request("s", "task-1", "回执"), sink)
        .unwrap();
    let retained = control.wait(&first);
    let report = done(&*control, &first).await;
    assert_eq!(report.run.commit, CommitState::Completed);
    assert_eq!(report.run.text.as_deref(), Some("回执完成"));
    assert_eq!(report.run.started_tools, Some(1));
    assert_eq!(report.run.tool_results.len(), 2);
    assert!(!report.cancel_requested);
    let mut events = vec![];
    while let Ok(event) = rx.try_recv() {
        assert!(control.accepts(&event));
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|e| e.event.kind == TurnEventKind::SessionSaved)
    );
    assert_eq!(
        control.cancel(&first),
        Ok(CancelDisposition::AlreadyFinished)
    );
    assert!(events.iter().all(|e| !control.accepts(e)));
    assert_eq!(done(&*control, &first).await, report);
    let second = control
        .submit(request("s", "task-2", "下一任务"), discard())
        .unwrap();
    assert_eq!(retained.await.unwrap(), report);
    assert_eq!(
        control.wait(&first).await,
        Err(ControlError::StaleGeneration)
    );
    assert_eq!(control.cancel(&first), Err(ControlError::StaleGeneration));
    assert!(events.iter().all(|e| !control.accepts(e)));
    assert_eq!(
        done(&*control, &second).await.run.commit,
        CommitState::Completed
    );
    assert_eq!(rig.snapshot("s").history().len(), 6);
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    rig.stop().await;
}

struct PartialProvider {
    requests: AtomicUsize,
}
impl LlmProvider for PartialProvider {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        unreachable!()
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if request.messages.last().unwrap().text.as_deref() == Some("旧任务") {
                sink.text_delta("旧的部分回复".into()).await?;
                std::future::pending().await
            } else {
                sink.text_delta("新任务完成".into()).await?;
                final_response("新任务完成")
            }
        })
    }
}
#[tokio::test]
async fn explicit_cancel_filters_queued_deltas_and_all_late_old_event_kinds() {
    let p = Arc::new(PartialProvider {
        requests: AtomicUsize::new(0),
    });
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig {
            response_mode: ResponseMode::Stream,
            ..LlmHostConfig::default()
        },
    )
    .await;
    let control = install(&rig).await;
    let (sink, mut rx) = channel();
    let old = control.submit(request("s", "old", "旧任务"), sink).unwrap();
    let started = rx.recv().await.unwrap();
    let delta = rx.recv().await.unwrap();
    assert!(matches!(delta.event.kind, TurnEventKind::TextDelta { .. }));
    assert!(control.accepts(&delta));
    assert_eq!(control.cancel(&old), Ok(CancelDisposition::Requested));
    assert!(!control.accepts(&delta));
    let report = done(&*control, &old).await;
    assert_eq!(
        report.run.failure,
        Some(RunFailure::Execution(LlmError::Cancelled))
    );
    assert_eq!(report.run.commit, CommitState::Failed);
    assert_eq!(report.run.started_tools, Some(0));
    let new = control
        .submit(request("s", "new", "新任务"), discard())
        .unwrap();
    assert_eq!(
        done(&*control, &new).await.run.commit,
        CommitState::Completed
    );
    for kind in [
        TurnEventKind::ToolResult {
            ordinal: 0,
            result: ToolResult::success("late", json!(1)).unwrap(),
        },
        TurnEventKind::TurnCompleted {
            text: "旧终态".into(),
        },
        TurnEventKind::SessionSaved,
    ] {
        assert!(!control.accepts(&ControlEvent {
            key: old.clone(),
            event: TurnEvent {
                turn_id: Some(1),
                kind
            }
        }));
    }
    assert!(!control.accepts(&started));
    assert_eq!(
        rig.snapshot("s").history(),
        vec![
            ChatMessage::text(ChatRole::User, "新任务"),
            ChatMessage::text(ChatRole::Assistant, "新任务完成")
        ]
    );
    assert_eq!(p.requests.load(Ordering::SeqCst), 2);
    rig.stop().await;
}

#[tokio::test]
async fn exact_generation_and_owner_are_checked_and_cancel_is_idempotent() {
    let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let control = install(&rig).await;
    let key = control
        .submit(request("s", "task", "问题"), discard())
        .unwrap();
    p.wait_requests(1).await;
    assert_eq!(
        control.submit(request("s", "another", "问题"), discard()),
        Err(ControlError::Busy)
    );
    let event = ControlEvent {
        key: key.clone(),
        event: TurnEvent {
            turn_id: Some(1),
            kind: TurnEventKind::SessionSaved,
        },
    };
    let mut wrong_turn = event.clone();
    wrong_turn.event.turn_id = Some(2);
    assert!(!control.accepts(&wrong_turn));
    for mut wrong in [key.clone(), key.clone(), key.clone()]
        .into_iter()
        .enumerate()
        .map(|(i, mut key)| {
            match i {
                0 => key.task_id.push('x'),
                1 => key.generation += 1,
                _ => key.controller_epoch[0] ^= 1,
            };
            key
        })
    {
        assert_eq!(control.cancel(&wrong), Err(ControlError::StaleGeneration));
        assert!(!control.accepts(&ControlEvent {
            key: wrong.clone(),
            ..event.clone()
        }));
        wrong.session.user_id = "其他用户".into();
        assert_eq!(control.cancel(&wrong), Err(ControlError::OwnerMismatch));
    }
    let mut other = request("s", "task", "问题");
    other.session.key.user_id = "其他用户".into();
    assert_eq!(
        control.submit(other.clone(), discard()),
        Err(ControlError::OwnerMismatch)
    );
    assert_eq!(
        control.snapshot(&other.session.key),
        Err(ControlError::OwnerMismatch)
    );
    assert_eq!(control.cancel(&key), Ok(CancelDisposition::Requested));
    assert_eq!(
        control.cancel(&key),
        Ok(CancelDisposition::AlreadyRequested)
    );
    assert!(done(&*control, &key).await.cancel_requested);
    assert_eq!(control.cancel(&key), Ok(CancelDisposition::AlreadyFinished));
    rig.stop().await;
}

#[tokio::test]
async fn immediate_cancel_creates_no_pending_and_context_wait_is_cancellable() {
    let p = Provider::new(vec![]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let control = install(&rig).await;
    let first = control
        .submit(request("s", "immediate", "问题"), discard())
        .unwrap();
    control.cancel(&first).unwrap();
    let report = done(&*control, &first).await;
    assert_eq!(report.run.commit, CommitState::NotStarted);
    assert!(rig.service().snapshot(&key("s")).unwrap().is_none());
    *rig.context.gate.lock().unwrap() = Some(Arc::new(Notify::new()));
    let second = control
        .submit(request("s", "context", "问题"), discard())
        .unwrap();
    rig.context.entered.notified().await;
    control.cancel(&second).unwrap();
    assert_eq!(
        done(&*control, &second).await.run.commit,
        CommitState::Failed
    );
    assert!(p.requests.lock().unwrap().is_empty());
    assert_eq!(rig.snapshot("s").turns.len(), 1);
    rig.stop().await;
}

#[tokio::test]
async fn failed_cancel_commit_blocks_replacement_and_preserves_pending() {
    let store = Arc::new(FaultStore::default());
    let p = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(p.clone(), store.clone(), LlmHostConfig::default()).await;
    let control = install(&rig).await;
    let key = control
        .submit(request("s", "task", "问题"), discard())
        .unwrap();
    p.wait_requests(1).await;
    store.fail.store(true, Ordering::SeqCst);
    control.cancel(&key).unwrap();
    let report = done(&*control, &key).await;
    assert_eq!(report.run.commit, CommitState::Pending);
    assert_eq!(
        report.run.failure,
        Some(RunFailure::FailureRecord {
            storage: SessionError::Storage,
            execution: LlmError::Cancelled
        })
    );
    assert_eq!(
        control.snapshot(&key.session).unwrap().unwrap().phase,
        ControlPhase::Blocked
    );
    assert_eq!(
        control.submit(request("s", "replacement", "问题"), discard()),
        Err(ControlError::Blocked)
    );
    assert_eq!(
        rig.snapshot("s").turns[0].status,
        SessionTurnStatus::Pending
    );
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    store.fail.store(false, Ordering::SeqCst);
    rig.stop().await;
}

#[tokio::test]
async fn completed_tool_output_survives_commit_failure_without_auto_retry() {
    let store = Arc::new(FaultStore::default());
    let p = Provider::new(vec![
        Step::new(single_call()),
        Step {
            fail_commit: Some(store.clone()),
            ..Step::new(final_response("已生成"))
        },
    ]);
    let rig = Rig::new(p.clone(), store.clone(), LlmHostConfig::default()).await;
    let control = install(&rig).await;
    let key = control
        .submit(request("s", "task", "回执"), discard())
        .unwrap();
    let report = done(&*control, &key).await;
    assert_eq!(report.run.commit, CommitState::Pending);
    assert_eq!(report.run.text.as_deref(), Some("已生成"));
    let transcript = report.run.transcript.as_ref().unwrap();
    validate_completed_turn("回执", transcript).unwrap();
    assert_eq!(
        transcript[1].tool_calls[0].arguments,
        json!({"text":"中文回执"})
    );
    assert_eq!(report.run.started_tools, Some(1));
    assert_eq!(report.run.tool_results.len(), 1);
    assert_eq!(
        control.submit(request("s", "retry", "问题"), discard()),
        Err(ControlError::Blocked)
    );
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    store.fail.store(false, Ordering::SeqCst);
    rig.stop().await;
}

struct SavedPause {
    entered: Notify,
}
impl ControlEventSink for SavedPause {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            if event.event.kind == TurnEventKind::SessionSaved {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn cancellation_after_durable_commit_reports_delivery_without_erasing_history() {
    let p = Provider::new(vec![
        Step::new(final_response("已落盘")),
        Step::new(final_response("继续")),
    ]);
    let rig = Rig::new(p, Arc::new(FaultStore::default()), LlmHostConfig::default()).await;
    let control = install(&rig).await;
    let sink = Arc::new(SavedPause {
        entered: Notify::new(),
    });
    let key = control
        .submit(request("s", "task", "问题"), sink.clone())
        .unwrap();
    sink.entered.notified().await;
    assert_eq!(rig.snapshot("s").history().len(), 2);
    control.cancel(&key).unwrap();
    let report = done(&*control, &key).await;
    assert!(report.cancel_requested);
    assert_eq!(report.run.commit, CommitState::Completed);
    assert_eq!(report.run.text.as_deref(), Some("已落盘"));
    assert_eq!(
        report.run.failure,
        Some(RunFailure::Delivery(LlmError::Cancelled))
    );
    let next = control
        .submit(request("s", "new", "继续"), discard())
        .unwrap();
    done(&*control, &next).await;
    assert_eq!(rig.snapshot("s").history().len(), 4);
    rig.stop().await;
}

#[tokio::test]
async fn disconnect_cancels_idle_provider_and_dropping_wait_does_not_cancel_execution() {
    let release = Arc::new(Notify::new());
    let p = Provider::new(vec![
        Step::blocked(Arc::new(Notify::new())),
        Step::blocked(release.clone()),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let control = install(&rig).await;
    let (sink, mut rx) = channel();
    let key = control.submit(request("s", "task", "问题"), sink).unwrap();
    rx.recv().await.unwrap();
    p.wait_requests(1).await;
    drop(rx);
    let report = done(&*control, &key).await;
    assert!(!report.cancel_requested);
    assert_eq!(
        report.run.failure,
        Some(RunFailure::Execution(LlmError::Cancelled))
    );
    let next = control
        .submit(request("s", "another", "继续"), discard())
        .unwrap();
    drop(control.wait(&next));
    p.wait_requests(2).await;
    assert_eq!(
        control.snapshot(&next.session).unwrap().unwrap().phase,
        ControlPhase::Generating
    );
    release.notify_one();
    assert_eq!(
        done(&*control, &next).await.run.commit,
        CommitState::Completed
    );
    rig.stop().await;
}

struct ProbeTool {
    starts: AtomicUsize,
    started: Notify,
    release: Notify,
    serial: bool,
    drop_gate: Option<Arc<DropGate>>,
}
struct DropGate {
    entered: Notify,
    released: Mutex<bool>,
    condition: Condvar,
}
impl DropGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.condition.notify_all();
    }
}
struct DropProbe(Arc<DropGate>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let released = self.0.released.lock().unwrap();
        let (_released, result) = self
            .0
            .condition
            .wait_timeout_while(released, Duration::from_secs(3), |r| !*r)
            .unwrap();
        assert!(!result.timed_out(), "工具析构等待释放超时");
    }
}
impl Tool for ProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            concurrency: if self.serial {
                None
            } else {
                Some(ToolConcurrency::ParallelSafe)
            },
            ..ReceiptTool {
                starts: Arc::new(AtomicUsize::new(0)),
            }
            .definition()
        }
    }
    fn validate_arguments(&self, _: &Value) -> Result<(), ToolValidationError> {
        Ok(())
    }
    fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let index = self.starts.fetch_add(1, Ordering::SeqCst);
            let _drop = self.drop_gate.clone().map(DropProbe);
            self.started.notify_one();
            if index == 0 {
                self.release.notified().await;
            }
            Ok(json!({"index":index}))
        })
    }
}
async fn install_tool(rig: &Rig, tool: Arc<ProbeTool>) {
    rig.kernel.stop(&id(OWNER)).await.unwrap();
    rig.kernel.unregister(&id(OWNER)).unwrap();
    rig.kernel
        .register(Box::new(ServicesPlugin {
            manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
            context: rig.context.clone(),
            tool,
        }))
        .unwrap();
    rig.kernel.start(&id(OWNER)).await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_wait_and_kernel_stop_wait_for_tool_destructor_and_failure_commit() {
    let p = Provider::new(vec![Step::new(single_call())]);
    let store = Arc::new(FaultStore::default());
    let rig = Rig::new(p, store.clone(), LlmHostConfig::default()).await;
    let drop_gate = Arc::new(DropGate {
        entered: Notify::new(),
        released: Mutex::new(false),
        condition: Condvar::new(),
    });
    let tool = Arc::new(ProbeTool {
        starts: AtomicUsize::new(0),
        started: Notify::new(),
        release: Notify::new(),
        serial: false,
        drop_gate: Some(drop_gate.clone()),
    });
    install_tool(&rig, tool.clone()).await;
    let control = install(&rig).await;
    let key = control
        .submit(request("s", "task", "工具"), discard())
        .unwrap();
    tool.started.notified().await;
    store.pause_next.store(true, Ordering::SeqCst);
    control.cancel(&key).unwrap();
    drop_gate.entered.notified().await;
    let waiter = tokio::spawn(control.wait(&key));
    assert_eq!(
        control.submit(request("s", "replacement", "问题"), discard()),
        Err(ControlError::Busy)
    );
    let kernel = rig.kernel.clone();
    let stop = tokio::spawn(async move { kernel.stop_all().await });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    assert!(!stop.is_finished());
    drop_gate.release();
    store.entered.notified().await;
    assert!(!waiter.is_finished());
    assert!(!stop.is_finished());
    assert_eq!(
        control.submit(request("s", "replacement", "问题"), discard()),
        Err(ControlError::Busy)
    );
    store.release();
    let report = waiter.await.unwrap().unwrap();
    assert_eq!(report.run.commit, CommitState::Failed);
    assert_eq!(report.run.started_tools, Some(1));
    assert_eq!(report.run.tool_results.len(), 1);
    tokio::time::timeout(Duration::from_secs(3), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        control.snapshot(&key.session),
        Err(ControlError::Unavailable)
    );
}

#[tokio::test]
async fn queued_serial_session_cancel_does_not_start_or_skip_other_tasks() {
    for serial in [true, false] {
        let p = Provider::new(vec![
            Step::new(single_call()),
            Step::new(single_call()),
            Step::new(single_call()),
            Step::new(final_response("完成")),
            Step::new(final_response("完成")),
        ]);
        let rig = Rig::new(
            p.clone(),
            Arc::new(FaultStore::default()),
            LlmHostConfig {
                max_parallel_tool_calls: if serial { 10 } else { 1 },
                ..LlmHostConfig::default()
            },
        )
        .await;
        let tool = Arc::new(ProbeTool {
            starts: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
            serial,
            drop_gate: None,
        });
        install_tool(&rig, tool.clone()).await;
        let control = install(&rig).await;
        let first = control
            .submit(request("first", "a", "工具"), discard())
            .unwrap();
        tool.started.notified().await;
        let mut second_input = request("second", "b", "工具");
        second_input.session.key.user_id = "用户二".into();
        let second = control.submit(second_input, discard()).unwrap();
        p.wait_requests(2).await;
        let mut third_input = request("third", "c", "工具");
        third_input.session.key.user_id = "用户三".into();
        let third = control.submit(third_input, discard()).unwrap();
        p.wait_requests(3).await;
        control.cancel(&second).unwrap();
        assert_eq!(done(&*control, &second).await.run.started_tools, Some(0));
        assert_eq!(tool.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            control.snapshot(&first.session).unwrap().unwrap().phase,
            ControlPhase::Tools
        );
        tool.release.notify_one();
        assert_eq!(done(&*control, &first).await.run.started_tools, Some(1));
        assert_eq!(done(&*control, &third).await.run.started_tools, Some(1));
        assert_eq!(tool.starts.load(Ordering::SeqCst), 2);
        assert!(
            rig.service()
                .snapshot(&second.session)
                .unwrap()
                .unwrap()
                .history()
                .is_empty()
        );
        rig.stop().await;
    }
}

#[tokio::test]
async fn plugin_restart_invalidates_old_service_and_controller_epoch() {
    let p = Provider::new(vec![
        Step::new(final_response("一")),
        Step::new(final_response("二")),
    ]);
    let rig = Rig::new(p, Arc::new(FaultStore::default()), LlmHostConfig::default()).await;
    let old = install(&rig).await;
    let first = old
        .submit(request("s", "same-task", "问题"), discard())
        .unwrap();
    done(&*old, &first).await;
    rig.kernel.stop(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    assert_eq!(
        old.submit(request("s", "same-task", "问题"), discard()),
        Err(ControlError::Unavailable)
    );
    rig.kernel.start(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    let new = service(&rig);
    let second = new
        .submit(request("s", "same-task", "问题"), discard())
        .unwrap();
    assert_ne!(first.controller_epoch, second.controller_epoch);
    assert_eq!(first.generation, second.generation);
    assert_eq!(new.cancel(&first), Err(ControlError::StaleGeneration));
    done(&*new, &second).await;
    rig.stop().await;
}

struct PanicRunner;
impl ControlRunner for PanicRunner {
    fn run<'a>(&'a self, _: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        panic!("执行器异常")
    }
}
#[tokio::test]
async fn runner_creation_panic_is_reported_unknown_and_blocks_replacement() {
    let rig = Rig::new(
        Provider::new(vec![]),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    rig.kernel
        .register(Box::new(
            ControlPlugin::new(Arc::new(PanicRunner), vec![]).unwrap(),
        ))
        .unwrap();
    rig.kernel.start(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    let control = service(&rig);
    let key = control
        .submit(request("s", "task", "问题"), discard())
        .unwrap();
    let report = done(&*control, &key).await;
    assert_eq!(report.run.commit, CommitState::Unknown);
    assert_eq!(report.run.started_tools, None);
    assert_eq!(report.run.failure, Some(RunFailure::RunnerPanicked));
    assert_eq!(
        control.submit(request("s", "retry", "问题"), discard()),
        Err(ControlError::Blocked)
    );
    rig.stop().await;
}

struct BlockingStart {
    manifest: PluginManifest,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
impl Plugin for BlockingStart {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, _: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(None)
        })
    }
}
#[tokio::test]
async fn cancel_while_waiting_kernel_admission_creates_no_session_or_provider_request() {
    let p = Provider::new(vec![]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let control = install(&rig).await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    rig.kernel
        .register(Box::new(BlockingStart {
            manifest: PluginManifest::new("blocking-start", "0.1.0").unwrap(),
            entered: entered.clone(),
            release: release.clone(),
        }))
        .unwrap();
    let kernel = rig.kernel.clone();
    let starting = tokio::spawn(async move { kernel.start(&id("blocking-start")).await });
    entered.notified().await;
    let key = control
        .submit(request("s", "waiting", "问题"), discard())
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        control.snapshot(&key.session).unwrap().unwrap().phase,
        ControlPhase::Starting
    );
    control.cancel(&key).unwrap();
    assert_eq!(
        done(&*control, &key).await.run.commit,
        CommitState::NotStarted
    );
    assert!(rig.service().snapshot(&key.session).unwrap().is_none());
    assert!(p.requests.lock().unwrap().is_empty());
    release.notify_one();
    starting.await.unwrap().unwrap();
    rig.stop().await;
}

#[tokio::test]
async fn full_bounded_channel_can_be_cancelled_without_draining_queue() {
    let p = Arc::new(PartialProvider {
        requests: AtomicUsize::new(0),
    });
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig {
            response_mode: ResponseMode::Stream,
            ..LlmHostConfig::default()
        },
    )
    .await;
    let control = install(&rig).await;
    let (tx, mut rx) = mpsc::channel(1);
    let key = control
        .submit(
            request("s", "slow-channel", "旧任务"),
            Arc::new(Channel(tx)),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while p.requests.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    control.cancel(&key).unwrap();
    assert_eq!(done(&*control, &key).await.run.commit, CommitState::Failed);
    assert!(!control.accepts(&rx.recv().await.unwrap()));
    assert!(rx.try_recv().is_err());
    rig.stop().await;
}

#[tokio::test]
async fn invalid_task_input_has_no_execution_or_control_reservation() {
    let p = Provider::new(vec![]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let control = install(&rig).await;
    for task in ["", " leading", "trailing ", "line\n", &"x".repeat(257)] {
        assert_eq!(
            control.submit(request("s", task, "问题"), discard()),
            Err(ControlError::InvalidInput)
        );
    }
    assert_eq!(
        control.submit(request("s", "task", "  "), discard()),
        Err(ControlError::InvalidInput)
    );
    let mut invalid = request("s", "task", "问题");
    invalid.session.key.user_id = "".into();
    assert_eq!(
        control.submit(invalid, discard()),
        Err(ControlError::InvalidInput)
    );
    assert_eq!(control.snapshot(&key("s")).unwrap(), None);
    assert!(p.requests.lock().unwrap().is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn wrong_owner_rejection_after_restart_does_not_lock_out_persisted_owner() {
    let p = Provider::new(vec![
        Step::new(final_response("已保存")),
        Step::new(final_response("原用户继续")),
    ]);
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let old = install(&rig).await;
    let first = old
        .submit(request("s", "first", "原用户问题"), discard())
        .unwrap();
    done(&*old, &first).await;
    rig.kernel.stop(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    rig.kernel.start(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    let control = service(&rig);
    let mut wrong = request("s", "wrong", "其他用户问题");
    wrong.session.key.user_id = "其他用户".into();
    let rejected = control.submit(wrong, discard()).unwrap();
    let report = done(&*control, &rejected).await;
    assert_eq!(report.run.commit, CommitState::NotStarted);
    assert_eq!(
        report.run.failure,
        Some(RunFailure::Session(SessionError::OwnerMismatch))
    );
    let next = control
        .submit(request("s", "right", "原用户继续"), discard())
        .unwrap();
    assert_eq!(
        done(&*control, &next).await.run.commit,
        CommitState::Completed
    );
    assert_eq!(rig.snapshot("s").history().len(), 4);
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    rig.stop().await;
}

struct PanicChannel;
impl ControlEventSink for PanicChannel {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            if matches!(event.event.kind, TurnEventKind::TextDelta { .. }) {
                panic!("通道异常");
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn channel_panic_is_contained_and_failure_commit_completes() {
    let p = Arc::new(PartialProvider {
        requests: AtomicUsize::new(0),
    });
    let rig = Rig::new(
        p.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig {
            response_mode: ResponseMode::Stream,
            ..LlmHostConfig::default()
        },
    )
    .await;
    let control = install(&rig).await;
    let key = control
        .submit(
            request("s", "channel-panic", "旧任务"),
            Arc::new(PanicChannel),
        )
        .unwrap();
    let report = done(&*control, &key).await;
    assert_eq!(report.run.commit, CommitState::Failed);
    assert!(matches!(
        report.run.failure,
        Some(RunFailure::Execution(LlmError::Backend(_)))
    ));
    assert_eq!(report.run.started_tools, Some(0));
    assert_eq!(p.requests.load(Ordering::SeqCst), 1);
    assert!(rig.snapshot("s").history().is_empty());
    rig.stop().await;
}
