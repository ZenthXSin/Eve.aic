#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;
use eve_llm_api::*;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider, StreamTimeoutPolicy};
use serde_json::json;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use support::{Reply, Server};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn event(text: &str, finish: Option<&str>) -> Vec<u8> {
    format!("data: {}\n\n", json!({"id":"fixture","choices":[{"index":0,"delta":{"content":text},"finish_reason":finish}]})).into_bytes()
}
fn stream(chunks: Vec<(u64, Vec<u8>)>) -> Reply {
    let mut reply = Reply::json(json!({}));
    reply.headers = vec![("Content-Type".into(), "text/event-stream".into())];
    reply.chunked = true;
    reply.delayed_chunks = chunks
        .into_iter()
        .map(|(delay, bytes)| (ms(delay), bytes))
        .collect();
    reply
}
fn provider(server: &Server, first: u64, idle: u64, total: u64) -> OpenAiProvider {
    let mut config = OpenAiConfig::chat("fixture");
    config.responses_url = server.url.replace("/responses", "/chat/completions");
    config.request_timeout = ms(100);
    OpenAiProvider::new(config, "fake-key")
        .unwrap()
        .with_streamed_completions()
        .with_stream_timeout_policy(StreamTimeoutPolicy {
            first_response: ms(first),
            idle: ms(idle),
            total: ms(total),
        })
        .unwrap()
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "测试")],
        tools: vec![],
    }
}
#[derive(Default)]
struct Sink(Mutex<String>);
impl ModelTextSink for Sink {
    fn text_delta(&self, text: String) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            self.0.lock().unwrap().push_str(&text);
            Ok(())
        })
    }
}

#[tokio::test]
async fn healthy_stream_can_outlast_old_total_timeout_for_complete_and_stream() {
    for explicit_stream in [false, true] {
        let server = Server::start(vec![stream(vec![
            (10, event("长", None)),
            (70, event("流", None)),
            (70, event("完成", Some("stop"))),
        ])])
        .await;
        let p = provider(&server, 120, 120, 1000);
        let start = Instant::now();
        let sink = Sink::default();
        let result = if explicit_stream {
            p.stream(request(), &sink).await
        } else {
            p.complete(request()).await
        };
        assert_eq!(
            result.unwrap(),
            ModelResponse::Final {
                text: "长流完成".into()
            }
        );
        assert!(start.elapsed() > ms(100));
        assert_eq!(
            &*sink.0.lock().unwrap(),
            if explicit_stream { "长流完成" } else { "" }
        );
    }
}

#[tokio::test]
async fn first_response_deadline_includes_headers_and_first_body_and_does_not_reset_at_headers() {
    for (headers, body) in [(160, 0), (0, 160), (60, 60)] {
        let mut reply = stream(vec![(body, event("不应完成", Some("stop")))]);
        reply.header_delay = ms(headers);
        let server = Server::start(vec![reply]).await;
        let start = Instant::now();
        assert!(matches!(
            provider(&server, 100, 300, 600).complete(request()).await,
            Err(LlmError::ProviderTimeout)
        ));
        assert!(start.elapsed() < ms(280));
    }
}

#[tokio::test]
async fn stalled_stream_never_exposes_partial_text_or_partial_tool_arguments() {
    let partial_tool = format!("data: {}\n\n",json!({"id":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"one","type":"function","function":{"name":"write_file","arguments":"{\"path\":"}}]},"finish_reason":null}]})).into_bytes();
    for partial in [event("PRIVATE_PARTIAL", None), partial_tool] {
        let server = Server::start(vec![stream(vec![
            (0, partial),
            (200, event("不应完成", Some("stop"))),
        ])])
        .await;
        let sink = Sink::default();
        assert!(matches!(
            provider(&server, 100, 60, 600)
                .stream(request(), &sink)
                .await,
            Err(LlmError::ProviderTimeout)
        ));
        assert!(sink.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn repeated_heartbeats_cannot_extend_total_deadline_forever() {
    let server = Server::start(vec![stream(
        (0..15).map(|_| (30, b": heartbeat\n\n".to_vec())).collect(),
    )])
    .await;
    let start = Instant::now();
    assert!(matches!(
        provider(&server, 100, 100, 180).complete(request()).await,
        Err(LlmError::ProviderTimeout)
    ));
    assert!(start.elapsed() < ms(400));
}

#[tokio::test]
async fn retries_share_stream_total_deadline_instead_of_legacy_request_timeout() {
    let mut rejected = Reply::json(json!({"error":"private"}));
    rejected.status = 503;
    let mut server = Server::start(vec![
        rejected,
        stream(vec![(10, event("恢复", Some("stop")))]),
    ])
    .await;
    let p = provider(&server, 100, 100, 1500).with_transient_retries();
    assert_eq!(
        p.complete(request()).await.unwrap(),
        ModelResponse::Final {
            text: "恢复".into()
        }
    );
    let first = server.next().await.body;
    assert_eq!(first, server.next().await.body);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn nonstream_requests_keep_original_deadline_and_invalid_policies_are_rejected() {
    let mut reply = Reply::json(
        json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"迟到"}}]}),
    );
    reply.body_delay = ms(200);
    let server = Server::start(vec![reply]).await;
    let mut config = OpenAiConfig::chat("fixture");
    config.responses_url = server.url.clone();
    config.request_timeout = ms(70);
    let p = OpenAiProvider::new(config, "fake-key")
        .unwrap()
        .with_stream_timeout_policy(StreamTimeoutPolicy {
            first_response: ms(100),
            idle: ms(100),
            total: ms(600),
        })
        .unwrap();
    assert!(matches!(
        p.complete(request()).await,
        Err(LlmError::ProviderTimeout)
    ));
    for (first, idle, total) in [(0, 1, 1), (1, 0, 1), (1, 1, 0), (2, 1, 1), (1, 2, 1)] {
        assert!(
            StreamTimeoutPolicy {
                first_response: ms(first),
                idle: ms(idle),
                total: ms(total)
            }
            .validate()
            .is_err()
        );
    }
}
