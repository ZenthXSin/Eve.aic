#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;
use eve_llm_api::*;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::{Value, json};
use std::{sync::Mutex, time::Duration};
use support::{Reply, Server};

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
        messages: vec![ChatMessage::text(ChatRole::User, "问题")],
        tools: vec![],
    }
}
fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id":"chat-one","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}
fn reply(chunks: Vec<Value>) -> Reply {
    let mut r = Reply::json(json!({}));
    r.headers = vec![(
        "Content-Type".into(),
        "text/event-stream; charset=utf-8".into(),
    )];
    r.chunked = true;
    r.body = format!(
        "\u{feff}: keepalive\r\n\r\n{}data: [DONE]\r\n\r\n",
        chunks
            .iter()
            .map(|v| format!("data: {v}\r\n\r\n"))
            .collect::<String>()
    )
    .into_bytes();
    r
}
fn text() -> Vec<Value> {
    vec![
        chunk(
            json!({"role":"assistant","reasoning_content":"不应泄漏"}),
            None,
        ),
        chunk(json!({"content":"中文🌍"}), None),
        chunk(json!({"content":"完整回复"}), Some("stop")),
    ]
}
fn provider(server: &Server) -> OpenAiProvider {
    OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            request_timeout: Duration::from_secs(5),
            ..OpenAiConfig::chat("grok-fixture")
        },
        "fake-key",
    )
    .unwrap()
}
#[tokio::test]
async fn streamed_complete_and_json_keep_final_contract_and_reasoning_private() {
    let mut server = Server::start(vec![
        reply(text()),
        reply(vec![chunk(
            json!({"content":"{\"ok\":true}"}),
            Some("stop"),
        )]),
    ])
    .await;
    let p = provider(&server).with_streamed_completions();
    assert_eq!(
        p.complete(request()).await.unwrap(),
        ModelResponse::Final {
            text: "中文🌍完整回复".into()
        }
    );
    assert_eq!(server.next().await.body["stream"], true);
    assert_eq!(
        p.complete_json(request()).await.unwrap(),
        ModelResponse::Final {
            text: "{\"ok\":true}".into()
        }
    );
    let body = server.next().await.body;
    assert_eq!(body["stream"], true);
    assert_eq!(body["response_format"], json!({"type":"json_object"}));
}
#[tokio::test]
async fn fragmented_interleaved_tools_preserve_order_and_never_publish_drafts() {
    let chunks = vec![
        chunk(
            json!({"role":"assistant","content":"工具前的草稿",
        "tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"echo","arguments":"{\"n\":"}},
                      {"index":0,"id":"a","type":"function","function":{"name":"ec","arguments":"{\"text\":\"中"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls":[{"index":0,"function":{"name":"ho","arguments":"文\"}"}},
            {"index":1,"function":{"arguments":"2}"}}]}),
            Some("tool_calls"),
        ),
    ];
    let server = Server::start(vec![reply(chunks)]).await;
    let sink = Sink::default();
    assert_eq!(
        provider(&server).stream(request(), &sink).await.unwrap(),
        ModelResponse::ToolCalls {
            calls: vec![
                ToolCall {
                    id: "a".into(),
                    name: "echo".into(),
                    arguments: json!({"text":"中文"})
                },
                ToolCall {
                    id: "b".into(),
                    name: "echo".into(),
                    arguments: json!({"n":2})
                }
            ]
        }
    );
    assert!(sink.0.lock().unwrap().is_empty());
}
#[tokio::test]
async fn corrupt_or_incomplete_streams_never_return_partial_text_or_calls() {
    let mut cases = vec![
        vec![chunk(json!({"content":"partial"}), None)],
        vec![chunk(json!({"content":"partial"}), Some("length"))],
        vec![chunk(json!({"content":"partial"}), Some("content_filter"))],
        vec![chunk(json!({"refusal":"secret"}), Some("stop"))],
        vec![chunk(json!({"content":"partial"}), Some("unknown"))],
    ];
    let mut changed_id = text();
    changed_id[1]["id"] = json!("other");
    cases.push(changed_id);
    let mut changed_index = text();
    changed_index[1]["choices"][0]["index"] = json!(1);
    cases.push(changed_index);
    for args in ["{", "[]", r#"{"a":1,"a":2}"#] {
        cases.push(vec![chunk(json!({"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"echo","arguments":args}}]}),Some("tool_calls"))]);
    }
    for chunks in cases {
        let mut server = Server::start(vec![reply(chunks)]).await;
        let sink = Sink::default();
        assert!(
            provider(&server)
                .with_transient_retries()
                .stream(request(), &sink)
                .await
                .is_err()
        );
        server.next().await;
        assert!(server.requests.try_recv().is_err());
        assert!(sink.0.lock().unwrap().is_empty());
    }
    let mut eof = reply(vec![chunk(json!({"content":"partial"}), None)]);
    eof.body.truncate(eof.body.len() - 16);
    let server = Server::start(vec![eof]).await;
    assert!(
        provider(&server)
            .with_streamed_completions()
            .complete(request())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn retries_only_hidden_complete_requests_within_original_deadline() {
    let mut busy = Reply::json(json!({}));
    busy.status = 503;
    let mut server = Server::start(vec![busy, reply(text())]).await;
    let p = provider(&server)
        .with_streamed_completions()
        .with_transient_retries();
    assert!(p.complete(request()).await.is_ok());
    assert_eq!(server.next().await.body, server.next().await.body);
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn mime_byte_budget_and_deadline_apply_to_chat_streams() {
    let mut wrong = reply(text());
    wrong.headers.clear();
    let server = Server::start(vec![wrong]).await;
    assert!(matches!(
        provider(&server)
            .with_streamed_completions()
            .complete(request())
            .await,
        Err(LlmError::Protocol(_))
    ));
    let server = Server::start(vec![reply(text())]).await;
    let p = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            max_response_bytes: 32,
            ..OpenAiConfig::chat("fixture")
        },
        "fake-key",
    )
    .unwrap()
    .with_streamed_completions();
    assert_eq!(
        p.complete(request())
            .await
            .unwrap_err()
            .provider_failure()
            .unwrap()
            .kind,
        ProviderFailureKind::ResponseTooLarge
    );
    let mut delayed = reply(text());
    delayed.body_delay = Duration::from_secs(1);
    let server = Server::start(vec![delayed]).await;
    let p = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            request_timeout: Duration::from_millis(50),
            ..OpenAiConfig::chat("fixture")
        },
        "fake-key",
    )
    .unwrap()
    .with_streamed_completions();
    assert_eq!(
        p.complete(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
}
