#[allow(dead_code)]
#[path = "support/mod.rs"]
mod http_support;
#[path = "support/sse.rs"]
mod sse;
use eve_llm_api::*;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use http_support::Server;
use serde_json::json;
use std::{sync::Mutex, time::Duration};

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
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "流式中文")],
        tools: vec![],
    }
}
fn provider(url: &str) -> OpenAiProvider {
    OpenAiProvider::new(
        OpenAiConfig {
            responses_url: url.into(),
            ..OpenAiConfig::new("fixture")
        },
        "fake-key",
    )
    .unwrap()
}
#[tokio::test]
async fn unicode_chunked_text_and_multiple_calls_match_complete_protocol() {
    let output = vec![sse::text("中文🌍\n第二段")];
    let calls = vec![
        sse::call("b", "echo", r#"{"text":"中文"}"#),
        sse::call("a", "echo", r#"{"text":"另一个"}"#),
    ];
    let mut server = Server::start(vec![
        sse::reply(&sse::events(output.clone())),
        sse::reply(&sse::events(calls.clone())),
        http_support::Reply::json(json!({"status":"completed","output":calls})),
    ])
    .await;
    let p = provider(&server.url);
    let sink = Sink::default();
    assert_eq!(
        p.stream(request(), &sink).await.unwrap(),
        ModelResponse::Final {
            text: "中文🌍\n第二段".into()
        }
    );
    assert_eq!(*sink.0.lock().unwrap(), "中文🌍\n第二段");
    let first = server.next().await.body;
    assert_eq!(first["stream"], true);
    assert_eq!(first["store"], false);
    let calls = p.stream(request(), &Sink::default()).await.unwrap();
    server.next().await;
    assert_eq!(calls, p.complete(request()).await.unwrap());
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn fails_closed_on_corrupt_inconsistent_or_unknown_events() {
    let normal = sse::events(vec![sse::text("中文")]);
    let mut cases = vec![];
    let mut bad = normal.clone();
    bad.pop();
    cases.push(bad);
    let mut bad = normal.clone();
    bad[3]["item_id"] = json!("wrong");
    cases.push(bad);
    let mut bad = normal.clone();
    bad[4]["sequence_number"] = json!(1);
    cases.push(bad);
    let mut bad = normal.clone();
    bad[3]["delta"] = json!("不同");
    cases.push(bad);
    let mut bad = normal.clone();
    bad[3]["type"] = json!("response.reasoning_text.delta");
    cases.push(bad);
    let mut bad = normal.clone();
    bad.last_mut().unwrap()["response"]["id"] = json!("another");
    cases.push(bad);
    let mut bad = normal.clone();
    bad[3]["response_id"] = json!("another");
    cases.push(bad);
    let mut bad = normal.clone();
    bad.insert(4, bad[3].clone());
    cases.push(bad);
    for args in ["not JSON", r#"{"text":"a","text":"b"}"#, "[]"] {
        cases.push(sse::events(vec![sse::call("one", "echo", args)]));
    }
    let mut duplicate = sse::events(vec![
        sse::call("one", "echo", "{}"),
        sse::call("two", "echo", "{}"),
    ]);
    for event in &mut duplicate {
        if event["item"]["call_id"] == "two" {
            event["item"]["call_id"] = json!("one");
        }
        if event["type"] == "response.completed" {
            event["response"]["output"][1]["call_id"] = json!("one");
        }
    }
    cases.push(duplicate);
    for events in cases {
        let mut server = Server::start(vec![sse::reply(&events)]).await;
        assert!(
            provider(&server.url)
                .stream(request(), &Sink::default())
                .await
                .is_err()
        );
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
}
#[tokio::test]
async fn validates_sse_frames_mime_status_and_byte_limits() {
    let normal = sse::events(vec![sse::text("中文")]);
    let mut reply = sse::reply(&normal);
    reply.body = format!(
        "\u{feff}: keepalive\r\nretry: 100\r\n\r\n{}",
        sse::frames(&normal).replace("\n", "\r\n")
    )
    .into_bytes();
    let server = Server::start(vec![reply]).await;
    assert!(
        provider(&server.url)
            .stream(request(), &Sink::default())
            .await
            .is_ok()
    );
    for body in [
        b"data: {bad}\n\n".to_vec(),
        b"event: wrong\ndata: {\"type\":\"response.created\"}\n\n".to_vec(),
        b"data: \xff\n\n".to_vec(),
    ] {
        let mut reply = sse::reply(&[]);
        reply.body = body;
        let server = Server::start(vec![reply]).await;
        assert!(
            provider(&server.url)
                .stream(request(), &Sink::default())
                .await
                .is_err()
        );
    }
    let mut reply = sse::reply(&normal);
    reply.headers.clear();
    let server = Server::start(vec![reply]).await;
    assert!(matches!(
        provider(&server.url)
            .stream(request(), &Sink::default())
            .await,
        Err(LlmError::Protocol(_))
    ));
    let mut reply = sse::reply(&normal);
    reply.status = 429;
    let server = Server::start(vec![reply]).await;
    assert_eq!(
        provider(&server.url)
            .stream(request(), &Sink::default())
            .await
            .unwrap_err(),
        LlmError::Provider("OpenAI HTTP 429".into())
    );
    let server = Server::start(vec![sse::reply(&normal)]).await;
    let p = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            max_response_bytes: 12,
            ..OpenAiConfig::new("fixture")
        },
        "fake-key",
    )
    .unwrap();
    assert!(matches!(
        p.stream(request(), &Sink::default()).await,
        Err(LlmError::Provider(_))
    ));
}
struct Slow;
impl ModelTextSink for Slow {
    fn text_delta(&self, _: String) -> LlmFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct Closed;
impl ModelTextSink for Closed {
    fn text_delta(&self, _: String) -> LlmFuture<'_, ()> {
        Box::pin(async { Err(LlmError::Cancelled) })
    }
}
#[tokio::test]
async fn deadlines_cover_reading_and_consumer_backpressure_without_retry() {
    let normal = sse::events(vec![sse::text("中文")]);
    let mut delayed = sse::reply(&normal);
    delayed.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![delayed, sse::reply(&normal), sse::reply(&normal)]).await;
    let p = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            request_timeout: Duration::from_millis(100),
            ..OpenAiConfig::new("fixture")
        },
        "fake-key",
    )
    .unwrap();
    assert_eq!(
        p.stream(request(), &Sink::default()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    server.next().await;
    assert_eq!(
        p.stream(request(), &Slow).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    server.next().await;
    assert_eq!(
        p.stream(request(), &Closed).await.unwrap_err(),
        LlmError::Cancelled
    );
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}
