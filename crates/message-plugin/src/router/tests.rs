use super::*;
use crate::RulesJudge;
use eve_config_api::{ConfigResult, ConfigSnapshot};
use eve_llm_api::LlmFuture;
use eve_session_api::SessionKey;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

struct Settings;
impl ConfigService for Settings {
    fn snapshot(&self, _: &str, _: u32) -> ConfigResult<ConfigSnapshot> {
        Ok(ConfigSnapshot {
            namespace: MESSAGE_NAMESPACE.into(),
            schema_version: 1,
            revision: 1,
            values: message_schema()
                .fields
                .into_iter()
                .map(|(key, field)| (key, field.default.unwrap()))
                .collect(),
        })
    }
    fn begin_request(&self, namespace: &str, version: u32) -> ConfigResult<ConfigRequest> {
        Ok(ConfigRequest::new(
            "test".into(),
            self.snapshot(namespace, version)?,
            1,
        ))
    }
    fn read_request(&self, request: &ConfigRequest) -> ConfigResult<ConfigSnapshot> {
        Ok(request.initial().clone())
    }
}

struct Control {
    state: Arc<Mutex<ControlSnapshot>>,
    cancellations: AtomicUsize,
    submissions: Mutex<Vec<ControlInput>>,
    cancel_entered: Notify,
    completion: Option<Arc<Notify>>,
}
impl Control {
    fn new(completion: Option<Arc<Notify>>) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(ControlSnapshot {
                key: GenerationKey {
                    session: SessionKey {
                        session_id: "session".into(),
                        user_id: "user".into(),
                    },
                    task_id: "task".into(),
                    controller_epoch: [7; 16],
                    generation: 1,
                },
                input_text: "原始要求".into(),
                phase: ControlPhase::Generating,
                cancel_requested: false,
                events_retired: false,
                turn_id: Some(1),
                report: None,
            })),
            cancellations: AtomicUsize::new(0),
            submissions: Mutex::new(vec![]),
            cancel_entered: Notify::new(),
            completion,
        })
    }
    fn complete(state: &mut ControlSnapshot) {
        state.phase = ControlPhase::Finished;
        state.report = Some(ControlReport {
            key: state.key.clone(),
            cancel_requested: state.cancel_requested,
            run: RunReport {
                turn_id: Some(1),
                commit: CommitState::Completed,
                text: Some("收尾已保存".into()),
                transcript: Some(vec![]),
                started_tools: Some(0),
                tool_results: vec![],
                failure: None,
            },
        });
    }
    fn key(&self) -> GenerationKey {
        self.state.lock().unwrap().key.clone()
    }
}
impl ControlService for Control {
    fn submit(
        &self,
        input: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        let mut state = self.state.lock().unwrap();
        state.key.generation += 1;
        state.key.task_id = input.task_id.clone();
        state.input_text = input.session.text.clone();
        state.cancel_requested = false;
        state.events_retired = false;
        state.phase = ControlPhase::Generating;
        state.report = None;
        self.submissions.lock().unwrap().push(input);
        Ok(state.key.clone())
    }
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        if self.key() != *expected {
            return Err(ControlError::StaleGeneration);
        }
        self.submit(input, sink)
    }
    fn cancel(&self, key: &GenerationKey) -> ControlResult<CancelDisposition> {
        let mut state = self.state.lock().unwrap();
        if state.key != *key {
            return Err(ControlError::StaleGeneration);
        }
        self.cancellations.fetch_add(1, Ordering::SeqCst);
        state.events_retired = true;
        if state.report.is_some() {
            return Ok(CancelDisposition::AlreadyFinished);
        }
        if state.cancel_requested {
            return Ok(CancelDisposition::AlreadyRequested);
        }
        state.cancel_requested = true;
        state.phase = ControlPhase::Cancelling;
        if self.completion.is_none() {
            Self::complete(&mut state);
        }
        self.cancel_entered.notify_one();
        Ok(CancelDisposition::Requested)
    }
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        let key = key.clone();
        let state = self.state.clone();
        let completion = self.completion.clone();
        Box::pin(async move {
            if let Some(completion) = completion {
                completion.notified().await;
            }
            let state = state.lock().unwrap();
            if state.key != key {
                return Err(ControlError::StaleGeneration);
            }
            state.report.clone().ok_or(ControlError::Busy)
        })
    }
    fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        Ok(Some(self.state.lock().unwrap().clone()))
    }
    fn accepts(&self, _: &ControlEvent) -> bool {
        false
    }
}

struct ClosingSink(watch::Sender<bool>);
impl ClosingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self(watch::channel(false).0))
    }
    fn close(&self) {
        self.0.send_replace(true);
    }
}
impl ControlEventSink for ClosingSink {
    fn emit(&self, _: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        let mut closed = self.0.subscribe();
        Box::pin(async move {
            while !*closed.borrow_and_update() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    }
}

struct SlowJudge {
    entered: Notify,
    release: Notify,
    dropped: AtomicBool,
}
impl SlowJudge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            release: Notify::new(),
            dropped: AtomicBool::new(false),
        })
    }
}
struct DropSignal<'a>(&'a AtomicBool);
impl Drop for DropSignal<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl RelationJudge for SlowJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        Box::pin(async move {
            let _drop = DropSignal(&self.dropped);
            self.entered.notify_one();
            self.release.notified().await;
            RulesJudge.judge(input).await
        })
    }
}

fn router(control: Arc<Control>, judge: Arc<dyn RelationJudge>) -> Router {
    Router(Arc::new(Inner {
        control,
        judge,
        config: Arc::new(Settings),
        closed: watch::channel(false).0,
        book: Mutex::new(Book {
            active: true,
            next: 1,
            entries: HashMap::new(),
            questions: HashMap::new(),
            gates: HashMap::new(),
        }),
    }))
}
fn message(control: &Control, text: &str) -> IncomingMessage {
    IncomingMessage {
        target: control.key(),
        message_id: "message".into(),
        text: text.into(),
        reply_to: None,
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("路由未在截止时间内完成")
}

#[tokio::test]
async fn sink_closure_drops_pending_judgement_before_any_control_action() {
    let control = Control::new(None);
    let judge = SlowJudge::new();
    let router = router(control.clone(), judge.clone());
    let sink = ClosingSink::new();
    let ticket = router
        .submit(message(&control, "/correct 修改要求"), sink.clone())
        .unwrap();
    bounded(judge.entered.notified()).await;
    sink.close();
    let report = bounded(router.wait(&ticket)).await.unwrap();
    assert_eq!(report.outcome, RouteOutcome::Stopped { prior: None });
    assert!(judge.dropped.load(Ordering::SeqCst));
    assert_eq!(control.cancellations.load(Ordering::SeqCst), 0);
    assert!(control.submissions.lock().unwrap().is_empty());
    router.0.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_without_a_new_generation_invalidates_pending_revision() {
    for finished in [false, true] {
        let control = Control::new(None);
        if finished {
            Control::complete(&mut control.state.lock().unwrap());
        }
        let judge = SlowJudge::new();
        let router = router(control.clone(), judge.clone());
        let ticket = router
            .submit(message(&control, "/correct 修改要求"), ClosingSink::new())
            .unwrap();
        bounded(judge.entered.notified()).await;
        control.cancel(&control.key()).unwrap();
        judge.release.notify_one();
        let report = bounded(router.wait(&ticket)).await.unwrap();
        assert_eq!(report.outcome, RouteOutcome::Stale { prior: None });
        assert_eq!(control.cancellations.load(Ordering::SeqCst), 1);
        assert!(control.submissions.lock().unwrap().is_empty());
        router.0.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_new_explicit_request_can_replace_an_already_cancelled_generation() {
    let control = Control::new(None);
    control.cancel(&control.key()).unwrap();
    let router = router(control.clone(), Arc::new(RulesJudge));
    let ticket = router
        .submit(message(&control, "/new 新的要求"), ClosingSink::new())
        .unwrap();
    let report = bounded(router.wait(&ticket)).await.unwrap();
    assert!(matches!(report.outcome, RouteOutcome::Replaced { .. }));
    let submissions = control.submissions.lock().unwrap().clone();
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].session.text, "新的要求");
    router.0.close().await.unwrap();
}

#[tokio::test]
async fn sink_closure_after_admitted_cancel_still_waits_for_the_final_report() {
    let completion = Arc::new(Notify::new());
    let control = Control::new(Some(completion.clone()));
    let router = router(control.clone(), Arc::new(RulesJudge));
    let sink = ClosingSink::new();
    let ticket = router
        .submit(message(&control, "/correct 修改要求"), sink.clone())
        .unwrap();
    bounded(control.cancel_entered.notified()).await;
    sink.close();
    // 收尾完成前不能返回 Stopped 并暗示工具已经停止。
    let mut pending = router.wait(&ticket);
    assert!(!ready(pending.as_mut()));
    Control::complete(&mut control.state.lock().unwrap());
    completion.notify_one();
    let report = bounded(pending).await.unwrap();
    let RouteOutcome::Stopped { prior: Some(prior) } = report.outcome else {
        panic!("必须保留已准入取消的最终报告");
    };
    assert_eq!(prior.run.commit, CommitState::Completed);
    assert_eq!(prior.run.text.as_deref(), Some("收尾已保存"));
    assert!(control.submissions.lock().unwrap().is_empty());
    router.0.close().await.unwrap();
}

#[tokio::test]
async fn sink_closure_while_waiting_for_the_session_gate_stops_the_message() {
    let control = Control::new(None);
    let judge = SlowJudge::new();
    let router = router(control.clone(), judge.clone());
    let gate = Arc::new(AsyncMutex::new(()));
    router
        .0
        .book
        .lock()
        .unwrap()
        .gates
        .insert(control.key().session.session_id, gate.clone());
    let _guard = gate.lock().await;
    let sink = ClosingSink::new();
    let ticket = router
        .submit(message(&control, "/correct 修改要求"), sink.clone())
        .unwrap();
    bounded(judge.entered.notified()).await;
    judge.release.notify_one();
    bounded(async {
        while !judge.dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    sink.close();
    let report = bounded(router.wait(&ticket)).await.unwrap();
    assert_eq!(report.outcome, RouteOutcome::Stopped { prior: None });
    assert_eq!(control.cancellations.load(Ordering::SeqCst), 0);
    assert!(control.submissions.lock().unwrap().is_empty());
    router.0.close().await.unwrap();
}
