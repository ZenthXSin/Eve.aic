#[path = "support/session.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
#[path = "../../llm-openai/tests/support/sse.rs"]
mod sse;
use eve_llm_api::*;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use eve_runtime::{LlmHostConfig, SessionRunError};
use eve_session_api::*;
use fixture::*;
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Streaming {
    inner: Arc<Provider>,
    deltas: AtomicUsize,
}
impl Streaming {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            inner: Provider::new(steps),
            deltas: AtomicUsize::new(0),
        })
    }
}
impl LlmProvider for Streaming {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.inner.complete(request)
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            let response = self.inner.complete(request).await?;
            if let ModelResponse::Final { text } = &response {
                for text in text.chars().map(|c| c.to_string()) {
                    sink.text_delta(text).await?;
                    self.deltas.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(response)
        })
    }
}
fn config() -> LlmHostConfig {
    LlmHostConfig {
        response_mode: ResponseMode::Stream,
        ..LlmHostConfig::default()
    }
}
fn input() -> SessionInput {
    SessionInput {
        key: key("one"),
        text: "测试".into(),
    }
}
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<TurnEvent>>,
    fail_on: Option<&'static str>,
    store: Option<Arc<FaultStore>>,
}
fn stage(kind: &TurnEventKind) -> &'static str {
    match kind {
        TurnEventKind::ProviderStarted { .. } => "request",
        TurnEventKind::TextDelta { .. } => "delta",
        TurnEventKind::ResponseCompleted { .. } => "response",
        TurnEventKind::ToolBatchStarted { .. } => "batch",
        TurnEventKind::ToolResult { .. } => "tool",
        TurnEventKind::TurnCompleted { .. } => "completed",
        TurnEventKind::SessionSaved => "saved",
        TurnEventKind::Failed { .. } => "failed",
    }
}
impl TurnEventSink for Recorder {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            let name = stage(&event.kind);
            self.events.lock().unwrap().push(event);
            if name == "completed"
                && let Some(store) = &self.store
            {
                store.fail.store(true, Ordering::SeqCst);
            }
            if self.fail_on == Some(name) {
                return Err(LlmError::Cancelled);
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn stream_and_complete_transcripts_match_and_events_distinguish_durable_commit() {
    for tools in [false, true] {
        let steps = || {
            if tools {
                vec![Step::new(calls()), Step::new(final_response("中文回复"))]
            } else {
                vec![Step::new(final_response("中文回复"))]
            }
        };
        let p = Streaming::new(steps());
        let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
        let sink = Recorder::default();
        let output = rig.host.run_turn_with_events(input(), &sink).await.unwrap();
        let events = sink.events.lock().unwrap().clone();
        assert!(events.iter().all(|e| e.turn_id == Some(1)));
        let names: Vec<_> = events.iter().map(|e| stage(&e.kind)).collect();
        assert_eq!(
            &names[names.len() - 3..],
            &["response", "completed", "saved"]
        );
        if tools {
            assert_eq!(
                &names[..6],
                &["request", "response", "batch", "tool", "tool", "request"]
            );
        }
        assert_eq!(rig.snapshot("one").history(), output.output.transcript);
        assert_eq!(rig.starts.load(Ordering::SeqCst), usize::from(tools));
        let complete = Rig::new(
            Streaming::new(steps()),
            Arc::new(FaultStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        assert_eq!(
            complete.host.run_turn(input()).await.unwrap().output,
            output.output
        );
        complete.stop().await;
        rig.stop().await;
    }
}
#[tokio::test]
async fn commit_failure_retains_output_pending_and_side_effects() {
    let store = Arc::new(FaultStore::default());
    let p = Streaming::new(vec![Step::new(calls()), Step::new(final_response("回执"))]);
    let rig = Rig::new(p, store.clone(), config()).await;
    let sink = Recorder {
        store: Some(store.clone()),
        ..Recorder::default()
    };
    let before = store.bytes();
    let error = rig
        .host
        .run_turn_with_events(input(), &sink)
        .await
        .unwrap_err();
    let SessionRunError::Commit { output, .. } = error else {
        panic!("需保留已生成输出");
    };
    assert_eq!(output.text, "回执");
    assert_eq!(output.diagnostics.started_tools, 1);
    assert_eq!(
        rig.snapshot("one").turns[0].status,
        SessionTurnStatus::Pending
    );
    assert_ne!(store.bytes(), before);
    let events = sink.events.lock().unwrap().clone();
    assert!(events.iter().any(|e| stage(&e.kind) == "completed"));
    assert!(!events.iter().any(|e| stage(&e.kind) == "saved"));
    store.fail.store(false, Ordering::SeqCst);
    rig.stop().await;
}
#[tokio::test]
async fn saved_but_delivery_failed_is_not_a_commit_failure() {
    let rig = Rig::new(
        Streaming::new(vec![Step::new(final_response("完成"))]),
        Arc::new(FaultStore::default()),
        config(),
    )
    .await;
    let sink = Recorder {
        fail_on: Some("saved"),
        ..Recorder::default()
    };
    let error = rig
        .host
        .run_turn_with_events(input(), &sink)
        .await
        .unwrap_err();
    let SessionRunError::Delivery { output, .. } = error else {
        panic!("状态已保存必须单独报告");
    };
    assert_eq!(rig.snapshot("one").history(), output.output.transcript);
    assert_eq!(output.turn_id, 1);
    rig.stop().await;
}
#[tokio::test]
async fn consumer_failure_after_tools_retains_diagnostics_and_never_requests_final() {
    let p = Streaming::new(vec![Step::new(calls())]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let sink = Recorder {
        fail_on: Some("tool"),
        ..Recorder::default()
    };
    let SessionRunError::Turn(failure) = rig
        .host
        .run_turn_with_events(input(), &sink)
        .await
        .unwrap_err()
    else {
        panic!()
    };
    assert_eq!(failure.error, LlmError::Cancelled);
    assert_eq!(failure.diagnostics.started_tools, 1);
    assert_eq!(failure.diagnostics.tool_results.len(), 2);
    assert_eq!(p.inner.requests.lock().unwrap().len(), 1);
    assert!(rig.snapshot("one").history().is_empty());
    rig.stop().await;
}
struct Channel(tokio::sync::mpsc::Sender<TurnEvent>);
impl TurnEventSink for Channel {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move { self.0.send(event).await.map_err(|_| LlmError::Cancelled) })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(async {
            self.0.closed().await;
            Ok(())
        })
    }
}
#[tokio::test]
async fn bounded_backpressure_disconnect_and_stop_cover_entire_stream() {
    let p = Streaming::new(vec![Step::new(final_response("中文回复"))]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let host = rig.host.clone();
    let turn = tokio::spawn(async move { host.run_turn_with_events(input(), &Channel(tx)).await });
    assert_eq!(stage(&rx.recv().await.unwrap().kind), "request");
    // 第一个增量占满唯一槽位，后续发送必须等待消费。
    tokio::time::timeout(Duration::from_secs(3), async {
        while p.deltas.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(p.deltas.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.snapshot("one").turns[0].status,
        SessionTurnStatus::Pending
    );
    let kernel = rig.kernel.clone();
    let stop = tokio::spawn(async move { kernel.stop_all().await });
    tokio::task::yield_now().await;
    assert!(!stop.is_finished());
    drop(rx);
    let SessionRunError::Turn(failure) = turn.await.unwrap().unwrap_err() else {
        panic!()
    };
    assert_eq!(failure.error, LlmError::Cancelled);
    assert_eq!(p.deltas.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(3), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn consumer_timeout_and_provider_unsupported_do_not_fake_completion() {
    struct Slow;
    impl TurnEventSink for Slow {
        fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
            Box::pin(async move {
                if stage(&event.kind) == "delta" {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    }
    let p = Streaming::new(vec![Step::new(final_response("中文"))]);
    let rig = Rig::new(
        p,
        Arc::new(FaultStore::default()),
        LlmHostConfig {
            event_timeout: Duration::from_millis(100),
            ..config()
        },
    )
    .await;
    let SessionRunError::Turn(failure) = rig
        .host
        .run_turn_with_events(input(), &Slow)
        .await
        .unwrap_err()
    else {
        panic!()
    };
    assert_eq!(failure.error, LlmError::Cancelled);
    assert!(rig.snapshot("one").history().is_empty());
    rig.stop().await;
    let p = Provider::new(vec![]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let SessionRunError::Turn(failure) = rig
        .host
        .run_turn_with_events(input(), &Recorder::default())
        .await
        .unwrap_err()
    else {
        panic!()
    };
    assert!(matches!(failure.error, LlmError::Unsupported(_)));
    assert!(p.requests.lock().unwrap().is_empty());
    rig.stop().await;
}
#[tokio::test]
async fn abort_during_delta_records_cancelled_and_does_not_replay_failed_input() {
    let p = Streaming::new(vec![
        Step::new(final_response("未完成回复")),
        Step::new(final_response("新轮")),
    ]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let host = rig.host.clone();
    let turn = tokio::spawn(async move { host.run_turn_with_events(input(), &Channel(tx)).await });
    rx.recv().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while p.deltas.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    turn.abort();
    assert!(turn.await.unwrap_err().is_cancelled());
    assert!(matches!(
        rig.snapshot("one").turns[0].status,
        SessionTurnStatus::Failed { .. }
    ));
    let mut next = input();
    next.text = "下一轮".into();
    rig.host.run_turn(next).await.unwrap();
    assert_eq!(
        p.inner.requests.lock().unwrap()[1]
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .count(),
        1
    );
    assert_eq!(rig.starts.load(Ordering::SeqCst), 0);
    rig.stop().await;
}
#[tokio::test]
async fn malformed_http_batches_and_disconnect_execute_zero_tools() {
    let mut duplicate = vec![
        sse::call("one", "receipt", r#"{"text":"中文"}"#),
        sse::call("two", "receipt", r#"{"text":"中文"}"#),
    ];
    duplicate[1]["call_id"] = json!("one");
    let mut cases = vec![
        sse::events(duplicate),
        sse::events(vec![sse::call("one", "receipt", "not JSON")]),
    ];
    let mut truncated = sse::events(vec![sse::call("one", "receipt", r#"{"text":"中文"}"#)]);
    truncated.pop();
    cases.push(truncated);
    for events in cases {
        let mut server = http_support::Server::start(vec![sse::reply(&events)]).await;
        let p = OpenAiProvider::new(
            OpenAiConfig {
                responses_url: server.url.clone(),
                ..OpenAiConfig::new("fixture")
            },
            "fake-key",
        )
        .unwrap();
        let rig = Rig::new(Arc::new(p), Arc::new(FaultStore::default()), config()).await;
        let result = rig
            .host
            .run_turn_with_events(input(), &Recorder::default())
            .await;
        assert!(result.is_err());
        assert_eq!(rig.starts.load(Ordering::SeqCst), 0);
        assert!(rig.snapshot("one").history().is_empty());
        server.next().await;
        assert!(server.requests.try_recv().is_err());
        rig.stop().await;
    }
}

#[tokio::test]
async fn failed_begin_emits_no_request_and_has_no_turn_id() {
    let store = Arc::new(FaultStore::default());
    let p = Streaming::new(vec![]);
    let rig = Rig::new(p.clone(), store.clone(), config()).await;
    store.fail.store(true, Ordering::SeqCst);
    let sink = Recorder::default();
    assert!(matches!(
        rig.host.run_turn_with_events(input(), &sink).await,
        Err(SessionRunError::Session(_))
    ));
    assert!(p.inner.requests.lock().unwrap().is_empty());
    let events = sink.events.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].turn_id, None);
    assert_eq!(stage(&events[0].kind), "failed");
    store.fail.store(false, Ordering::SeqCst);
    rig.stop().await;
}
#[tokio::test]
async fn host_bounds_text_and_rejects_dishonest_provider_terminal() {
    let p = Streaming::new(vec![Step::new(final_response("中文"))]);
    let rig = Rig::new(
        p,
        Arc::new(FaultStore::default()),
        LlmHostConfig {
            max_stream_text_bytes: 3,
            ..config()
        },
    )
    .await;
    let sink = Recorder::default();
    let SessionRunError::Turn(failure) = rig
        .host
        .run_turn_with_events(input(), &sink)
        .await
        .unwrap_err()
    else {
        panic!()
    };
    assert!(matches!(failure.error, LlmError::Protocol(_)));
    assert_eq!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| stage(&e.kind) == "delta")
            .count(),
        1
    );
    rig.stop().await;
    struct Dishonest;
    impl LlmProvider for Dishonest {
        fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
            Box::pin(async { final_response("完整文本") })
        }
        fn stream<'a>(
            &'a self,
            _: ModelRequest,
            sink: &'a dyn ModelTextSink,
        ) -> LlmFuture<'a, ModelResponse> {
            Box::pin(async move {
                sink.text_delta("不同文本".into()).await?;
                final_response("完整文本")
            })
        }
    }
    let rig = Rig::new(
        Arc::new(Dishonest),
        Arc::new(FaultStore::default()),
        config(),
    )
    .await;
    let SessionRunError::Turn(failure) = rig
        .host
        .run_turn_with_events(input(), &Recorder::default())
        .await
        .unwrap_err()
    else {
        panic!()
    };
    assert!(matches!(failure.error, LlmError::Protocol(_)));
    assert!(rig.snapshot("one").history().is_empty());
    rig.stop().await;
}
#[tokio::test]
async fn consumer_panic_is_contained_without_repeating_provider() {
    struct Panic;
    impl TurnEventSink for Panic {
        fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
            Box::pin(async move {
                if stage(&event.kind) == "delta" {
                    panic!("consumer panic");
                }
                Ok(())
            })
        }
    }
    let p = Streaming::new(vec![Step::new(final_response("中文"))]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let SessionRunError::Turn(failure) = rig
        .host
        .run_turn_with_events(input(), &Panic)
        .await
        .unwrap_err()
    else {
        panic!()
    };
    assert!(matches!(failure.error, LlmError::Backend(_)));
    assert_eq!(p.inner.requests.lock().unwrap().len(), 1);
    assert!(rig.snapshot("one").history().is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn disconnect_while_provider_is_idle_drops_network_wait_immediately() {
    let p = Streaming::new(vec![Step::blocked(Arc::new(tokio::sync::Notify::new()))]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let host = rig.host.clone();
    let turn = tokio::spawn(async move { host.run_turn_with_events(input(), &Channel(tx)).await });
    rx.recv().await.unwrap();
    p.inner.wait_requests(1).await;
    drop(rx);
    let SessionRunError::Turn(failure) = tokio::time::timeout(Duration::from_secs(1), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
    else {
        panic!()
    };
    assert_eq!(failure.error, LlmError::Cancelled);
    assert_eq!(failure.diagnostics.started_tools, 0);
    assert!(rig.snapshot("one").history().is_empty());
    rig.stop().await;
}
#[tokio::test]
async fn disconnect_during_tool_cancels_and_joins_without_erasing_started_count() {
    let response = ModelResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "one".into(),
            name: "receipt".into(),
            arguments: json!({"text":"超时"}),
        }],
    };
    let p = Streaming::new(vec![Step::new(Ok(response))]);
    let rig = Rig::new(p.clone(), Arc::new(FaultStore::default()), config()).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let host = rig.host.clone();
    let turn = tokio::spawn(async move { host.run_turn_with_events(input(), &Channel(tx)).await });
    for name in ["request", "response", "batch"] {
        assert_eq!(stage(&rx.recv().await.unwrap().kind), name);
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while rig.starts.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(rx);
    let SessionRunError::Turn(failure) = tokio::time::timeout(Duration::from_secs(1), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
    else {
        panic!()
    };
    assert_eq!(failure.error, LlmError::Cancelled);
    assert_eq!(failure.diagnostics.started_tools, 1);
    assert_eq!(failure.diagnostics.tool_results.len(), 1);
    assert!(matches!(
        failure.diagnostics.tool_results[0].output,
        ToolOutput::Failure {
            code: ToolFailureCode::Cancelled,
            ..
        }
    ));
    assert_eq!(p.inner.requests.lock().unwrap().len(), 1);
    rig.stop().await;
}
