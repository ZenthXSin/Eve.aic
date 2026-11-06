#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;
use eve_llm_api::*;
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider, StreamTimeoutPolicy};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use support::{Reply, Server};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn event(value: Value) -> Vec<u8> {
    format!("data: {value}\r\n\r\n").into_bytes()
}
fn terminal(tool: bool) -> Vec<u8> {
    let (delta, finish) = if tool {
        (
            json!({"content":"private draft","tool_calls":[{"index":0,"id":"one","type":"function","function":{"name":"echo","arguments":"{}"}}]}),
            "tool_calls",
        )
    } else {
        (json!({"content":"完整中文"}), "stop")
    };
    event(json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}))
}
fn usage(cached: Option<u64>) -> Vec<u8> {
    let mut value =
        json!({"id":"fixture","choices":[],"usage":{"prompt_tokens":2048,"completion_tokens":3}});
    if let Some(cached) = cached {
        value["usage"]["prompt_tokens_details"] = json!({"cached_tokens":cached});
    }
    event(value)
}
fn done() -> Vec<u8> {
    b"data: [DONE]\r\n\r\n".to_vec()
}
fn stream(chunks: Vec<(Duration, Vec<u8>)>) -> Reply {
    let mut r = Reply::json(json!({}));
    r.headers = vec![("Content-Type".into(), "text/event-stream".into())];
    r.chunked = true;
    r.delayed_chunks = chunks;
    r
}
fn request() -> ModelRequest {
    ModelRequest {
        session: None,
        messages: vec![ChatMessage::text(ChatRole::User, "fixture")],
        tools: vec![],
    }
}
#[derive(Default)]
struct Usages(Mutex<Vec<ModelUsage>>);
impl ModelUsageObserver for Usages {
    fn reported(&self, _: &str, usage: ModelUsage) {
        self.0.lock().unwrap().push(usage);
    }
}
#[derive(Default)]
struct Sink(Mutex<Vec<String>>);
impl ModelTextSink for Sink {
    fn text_delta(&self, text: String) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            self.0.lock().unwrap().push(text);
            Ok(())
        })
    }
}
fn provider(server: &Server, seen: &Arc<Usages>, total: u64) -> OpenAiProvider {
    let mut config = OpenAiConfig::chat("gpt-6.1-sol");
    config.responses_url = server.url.clone();
    config.request_timeout = ms(total);
    OpenAiProvider::new(config, "fake-key")
        .unwrap()
        .with_streamed_completions()
        .with_transient_retries()
        .with_usage_observer(seen.clone(), "dialogue")
}
fn assert_response(result: ModelResponse, tool: bool) {
    if tool {
        assert!(
            matches!(result, ModelResponse::ToolCalls { calls } if calls.len() == 1 && calls[0].id == "one")
        );
    } else {
        assert_eq!(
            result,
            ModelResponse::Final {
                text: "完整中文".into()
            }
        );
    }
}

#[tokio::test]
async fn usage_after_finish_is_collected_for_text_tools_and_all_stream_entrypoints() {
    for tool in [false, true] {
        for mode in 0..3 {
            for fragmented in [false, true] {
                let body = [terminal(tool), usage(Some(1024)), done()].concat();
                let chunks = if fragmented {
                    body.chunks(1)
                        .map(|b| (Duration::ZERO, b.to_vec()))
                        .collect()
                } else {
                    vec![(Duration::ZERO, body)]
                };
                let mut server = Server::start(vec![stream(chunks)]).await;
                let seen = Arc::new(Usages::default());
                let p = provider(&server, &seen, 3000);
                let sink = Sink::default();
                let result = match mode {
                    0 => p.complete(request()).await,
                    1 => p.complete_json(request()).await,
                    _ => p.stream(request(), &sink).await,
                }
                .unwrap();
                assert_response(result, tool);
                assert_eq!(
                    sink.0.lock().unwrap().len(),
                    usize::from(mode == 2 && !tool)
                );
                let body = server.next().await.body;
                assert_eq!(body["stream_options"], json!({"include_usage":true}));
                assert_eq!(body["model"], "gpt-6.1-sol");
                assert!(body.get("temperature").is_none());
                assert!(body.get("reasoning_effort").is_none());
                if mode == 1 {
                    assert_eq!(body["response_format"], json!({"type":"json_object"}));
                }
                assert!(server.requests.try_recv().is_err());
                assert_eq!(
                    *seen.0.lock().unwrap(),
                    vec![ModelUsage {
                        input_tokens: Some(2048),
                        cached_input_tokens: Some(1024),
                        output_tokens: Some(3)
                    }]
                );
            }
        }
    }
}

#[tokio::test]
async fn usage_snapshots_keep_zero_and_missing_distinct_without_double_counting() {
    for cached in [None, Some(0), Some(1024)] {
        let chunks = vec![(
            Duration::ZERO,
            [usage(Some(512)), terminal(false), usage(cached), done()].concat(),
        )];
        let server = Server::start(vec![stream(chunks)]).await;
        let seen = Arc::new(Usages::default());
        assert_response(
            provider(&server, &seen, 1000)
                .complete(request())
                .await
                .unwrap(),
            false,
        );
        let reports = seen.0.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].input_tokens, Some(2048));
        assert_eq!(reports[0].cached_input_tokens, cached);
    }
}

#[tokio::test]
async fn missing_usage_done_or_eof_after_finish_preserves_success() {
    for tail in [
        vec![],
        done(),
        event(json!({"id":"fixture","choices":[],"usage":null})),
    ] {
        let server = Server::start(vec![stream(vec![(
            Duration::ZERO,
            [terminal(false), tail].concat(),
        )])])
        .await;
        let seen = Arc::new(Usages::default());
        assert_response(
            provider(&server, &seen, 1000)
                .complete(request())
                .await
                .unwrap(),
            false,
        );
        assert_eq!(*seen.0.lock().unwrap(), vec![ModelUsage::default()]);
    }
}

#[tokio::test]
async fn invalid_or_oversized_tail_never_changes_result_or_retries() {
    let mut wrong_id = usage(Some(1024));
    wrong_id = String::from_utf8(wrong_id)
        .unwrap()
        .replace("fixture", "other")
        .into_bytes();
    for tail in [
        b"data: {bad}\n\n".to_vec(),
        b"data: \xff\n\n".to_vec(),
        wrong_id,
        terminal(true),
        terminal(false),
        vec![b'x'; 70 * 1024],
    ] {
        for same_chunk in [false, true] {
            let chunks = if same_chunk {
                vec![(
                    Duration::ZERO,
                    [terminal(true), tail.clone(), usage(Some(1024)), done()].concat(),
                )]
            } else {
                vec![
                    (Duration::ZERO, terminal(true)),
                    (ms(10), [tail.clone(), usage(Some(1024)), done()].concat()),
                ]
            };
            let mut server = Server::start(vec![stream(chunks)]).await;
            let seen = Arc::new(Usages::default());
            assert_response(
                provider(&server, &seen, 2000)
                    .complete(request())
                    .await
                    .unwrap(),
                true,
            );
            server.next().await;
            assert!(server.requests.try_recv().is_err());
            assert_eq!(*seen.0.lock().unwrap(), vec![ModelUsage::default()]);
        }
    }
}

#[tokio::test]
async fn transport_error_after_finish_does_not_retry() {
    let mut r = stream(vec![]);
    r.chunked = false;
    r.body = terminal(true);
    r.declared_length = Some(r.body.len() + 1000);
    let mut server = Server::start(vec![r]).await;
    let seen = Arc::new(Usages::default());
    assert_response(
        provider(&server, &seen, 2000)
            .complete(request())
            .await
            .unwrap(),
        true,
    );
    server.next().await;
    assert!(server.requests.try_recv().is_err());
    assert_eq!(seen.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tail_heartbeats_and_missing_done_cannot_extend_grace_or_original_total() {
    for total in [180, 3000] {
        let mut chunks = vec![(ms(70), terminal(true))];
        chunks.extend((0..150).map(|_| (ms(25), b": heartbeat\n\n".to_vec())));
        let mut server = Server::start(vec![stream(chunks)]).await;
        let seen = Arc::new(Usages::default());
        let p = provider(&server, &seen, total)
            .with_stream_timeout_policy(StreamTimeoutPolicy {
                first_response: ms(150),
                idle: ms(60),
                total: ms(total),
            })
            .unwrap();
        let start = Instant::now();
        assert_response(p.complete(request()).await.unwrap(), true);
        assert!(start.elapsed() < ms(if total == 180 { 500 } else { 1500 }));
        assert!(start.elapsed() >= ms(if total == 180 { 150 } else { 1000 }));
        server.next().await;
        assert!(server.requests.try_recv().is_err());
        assert_eq!(seen.0.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn prefinish_retry_shares_original_deadline_with_successful_tail() {
    let mut busy = Reply::json(json!({}));
    busy.status = 503;
    let mut server = Server::start(vec![
        busy,
        stream(vec![(ms(30), terminal(true)), (ms(500), usage(Some(1024)))]),
    ])
    .await;
    let seen = Arc::new(Usages::default());
    let start = Instant::now();
    assert_response(
        provider(&server, &seen, 1200)
            .complete(request())
            .await
            .unwrap(),
        true,
    );
    assert!(start.elapsed() >= ms(1150) && start.elapsed() < ms(1500));
    assert_eq!(server.next().await.body, server.next().await.body);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(*seen.0.lock().unwrap(), vec![ModelUsage::default()]);
}

struct Closed;
impl ModelTextSink for Closed {
    fn text_delta(&self, _: String) -> LlmFuture<'_, ()> {
        Box::pin(async { Err(LlmError::Cancelled) })
    }
}
#[tokio::test]
async fn cancellation_does_not_report_a_successful_request() {
    let mut server = Server::start(vec![
        stream(vec![(
            Duration::ZERO,
            [terminal(false), usage(Some(1024)), done()].concat(),
        )]),
        stream(vec![(Duration::ZERO, terminal(false)), (ms(1000), done())]),
    ])
    .await;
    let seen = Arc::new(Usages::default());
    let p = provider(&server, &seen, 3000);
    assert_eq!(
        p.stream(request(), &Closed).await.unwrap_err(),
        LlmError::Cancelled
    );
    assert!(seen.0.lock().unwrap().is_empty());
    server.next().await;
    assert!(
        tokio::time::timeout(ms(100), p.complete(request()))
            .await
            .is_err()
    );
    assert!(seen.0.lock().unwrap().is_empty());
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn nonstream_chat_and_responses_do_not_receive_stream_options() {
    let replies = vec![
        Reply::json(
            json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"fixture"}}]}),
        ),
        Reply::json(support::final_response("fixture")),
    ];
    let mut server = Server::start(replies).await;
    for protocol in [OpenAiProtocol::ChatCompletions, OpenAiProtocol::Responses] {
        let mut config = OpenAiConfig::new("fixture");
        config.responses_url = server.url.clone();
        config.protocol = protocol;
        OpenAiProvider::new(config, "fake-key")
            .unwrap()
            .complete(request())
            .await
            .unwrap();
        let body = server.next().await.body;
        assert_eq!(body["stream"], false);
        assert!(body.get("stream_options").is_none());
    }
}

#[tokio::test]
async fn delayed_usage_is_read_and_done_finishes_without_waiting_for_eof() {
    let mut server = Server::start(vec![stream(vec![
        (Duration::ZERO, terminal(false)),
        (ms(60), usage(Some(1024))),
        (ms(30), done()),
        (ms(2000), b": still open\n\n".to_vec()),
    ])])
    .await;
    let seen = Arc::new(Usages::default());
    let start = Instant::now();
    assert_response(
        provider(&server, &seen, 3000)
            .complete(request())
            .await
            .unwrap(),
        false,
    );
    assert!(start.elapsed() >= ms(70) && start.elapsed() < ms(700));
    server.next().await;
    assert!(server.requests.try_recv().is_err());
    assert_eq!(seen.0.lock().unwrap()[0].cached_input_tokens, Some(1024));
}

#[tokio::test]
async fn semantic_byte_limit_and_tail_byte_limit_are_separate_in_same_chunk() {
    let completed = terminal(true);
    for same_chunk in [false, true] {
        let tail = [usage(Some(1024)), vec![b'x'; 300]].concat();
        let chunks = if same_chunk {
            vec![(Duration::ZERO, [completed.clone(), tail].concat())]
        } else {
            vec![(Duration::ZERO, completed.clone()), (ms(10), tail)]
        };
        let mut server = Server::start(vec![stream(chunks)]).await;
        let seen = Arc::new(Usages::default());
        let mut config = OpenAiConfig::chat("fixture");
        config.responses_url = server.url.clone();
        // 可完整验证终态，但统计帧超过剩余总字节预算。
        config.max_response_bytes = completed.len() + 20;
        let p = OpenAiProvider::new(config, "fake-key")
            .unwrap()
            .with_streamed_completions()
            .with_transient_retries()
            .with_usage_observer(seen.clone(), "dialogue");
        assert_response(p.complete(request()).await.unwrap(), true);
        server.next().await;
        assert!(server.requests.try_recv().is_err());
        assert_eq!(*seen.0.lock().unwrap(), vec![ModelUsage::default()]);
    }
}

#[tokio::test]
async fn usage_in_terminal_event_is_preserved_when_tail_is_invalid() {
    let mut value: Value = serde_json::from_str(
        std::str::from_utf8(&terminal(false))
            .unwrap()
            .trim()
            .strip_prefix("data: ")
            .unwrap(),
    )
    .unwrap();
    value["usage"] = json!({"prompt_tokens":2048,"prompt_tokens_details":{"cached_tokens":1024}});
    let server = Server::start(vec![stream(vec![(
        Duration::ZERO,
        [event(value), b"data: {bad}\n\n".to_vec()].concat(),
    )])])
    .await;
    let seen = Arc::new(Usages::default());
    assert_response(
        provider(&server, &seen, 1000)
            .complete(request())
            .await
            .unwrap(),
        false,
    );
    let reports = seen.0.lock().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].cached_input_tokens, Some(1024));
}
