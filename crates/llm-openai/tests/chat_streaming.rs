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
        session: None,
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
    assert_eq!(body["tool_choice"], "none");
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
async fn null_optional_placeholders_preserve_complete_tool_identity_and_arguments() {
    let mut chunks = vec![
        chunk(
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"one","type":"function","function":{"name":"echo","arguments":"{"}}]}),
            None,
        ),
        chunk(
            json!({"role":null,"content":null,"tool_calls":[{"index":0,"id":null,"type":null,"function":{"name":null,"arguments":"\"text\":\"中文\"}"}}]}),
            None,
        ),
        chunk(
            json!({"role":null,"tool_calls":[{"index":0,"id":null,"type":null,"function":null}]}),
            None,
        ),
        chunk(json!({"role":null,"tool_calls":null}), Some("tool_calls")),
    ];
    chunks[1]["id"] = Value::Null;
    let mut server = Server::start(vec![reply(chunks)]).await;
    assert_eq!(
        provider(&server)
            .with_streamed_completions()
            .complete(request())
            .await
            .unwrap(),
        ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "one".into(),
                name: "echo".into(),
                arguments: json!({"text":"中文"})
            }]
        }
    );
    server.next().await;
    assert!(
        server.requests.try_recv().is_err(),
        "兼容 null 占位不重发请求"
    );
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
    for (key, bad) in [("id", Value::Null), ("id", json!(3)), ("type", Value::Null)] {
        let mut call = json!({"index":0,"id":"a","type":"function","function":{"name":"echo","arguments":"{}"}});
        call[key] = bad;
        cases.push(vec![chunk(
            json!({"tool_calls":[call]}),
            Some("tool_calls"),
        )]);
    }
    cases.push(vec![
        chunk(json!({"tool_calls":[{"index":0,"id":"original","type":"function","function":{"name":"echo","arguments":"{}"}}]}), None),
        chunk(json!({"tool_calls":[{"index":0,"id":"changed","type":null,"function":null}]}), Some("tool_calls")),
    ]);
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

#[tokio::test]
async fn role_only_bootstrap_can_bind_once_to_the_first_text_or_tool_identity() {
    for finish_null in [false, true] {
        let mut bootstrap = chunk(json!({"role":"assistant"}), None);
        if !finish_null {
            bootstrap["choices"][0]
                .as_object_mut()
                .unwrap()
                .remove("finish_reason");
        }
        let mut content = chunk(json!({"content":"完整正文"}), Some("stop"));
        content["id"] = json!("actual-response");
        let mut server = Server::start(vec![reply(vec![bootstrap, content])]).await;
        let sink = Sink::default();
        assert_eq!(
            provider(&server).stream(request(), &sink).await.unwrap(),
            ModelResponse::Final {
                text: "完整正文".into()
            }
        );
        assert_eq!(*sink.0.lock().unwrap(), "完整正文");
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
    let mut call = chunk(
        json!({"tool_calls":[{
            "index":0,"id":"call-one","type":"function","function":{"name":"echo","arguments":"{}"}
        }]}),
        Some("tool_calls"),
    );
    call["id"] = json!("actual-response");
    let server = Server::start(vec![reply(vec![
        chunk(json!({"role":"assistant"}), None),
        call,
    ])])
    .await;
    let sink = Sink::default();
    assert_eq!(
        provider(&server).stream(request(), &sink).await.unwrap(),
        ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "call-one".into(),
                name: "echo".into(),
                arguments: json!({})
            }]
        }
    );
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bootstrap_window_never_discards_semantics_or_allows_later_identity_changes() {
    let bootstrap = chunk(json!({"role":"assistant"}), None);
    let mut content = chunk(json!({"content":"正文"}), None);
    content["id"] = json!("actual-response");
    let mut finish = content.clone();
    finish["choices"][0]["finish_reason"] = json!("stop");
    let mut cases = Vec::new();
    // content 的 null/空串仍是实际字段，不扩大已观测到的纯角色引导例外。
    for extra in [
        json!({"content":null}),
        json!({"content":""}),
        json!({"content":"已有正文"}),
        json!({"tool_calls":null}),
        json!({"tool_calls":[]}),
        json!({"reasoning_content":"已有思考"}),
        json!({"refusal":null}),
        json!({"unknown_extra":null}),
    ] {
        let mut first = bootstrap.clone();
        first["choices"][0]["delta"]
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        cases.push(vec![first, finish.clone()]);
    }
    for extra in [
        json!({"usage":null}),
        json!({"usage":{}}),
        json!({"unknown_extra":null}),
    ] {
        let mut first = bootstrap.clone();
        first
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        cases.push(vec![first, finish.clone()]);
    }
    let mut extra_choice = bootstrap.clone();
    extra_choice["choices"][0]["unknown_extra"] = Value::Null;
    cases.push(vec![extra_choice, finish.clone()]);
    for middle in [
        chunk(json!({"role":"assistant"}), None),
        chunk(json!({}), None),
        chunk(json!({"content":null}), None),
        chunk(json!({"content":""}), None),
        json!({"id":"chat-one","choices":[],"usage":{"prompt_tokens":10}}),
    ] {
        cases.push(vec![bootstrap.clone(), middle, finish.clone()]);
    }
    let mut empty_changed = chunk(json!({}), None);
    empty_changed["id"] = json!("actual-response");
    cases.push(vec![bootstrap.clone(), empty_changed, finish.clone()]);
    let mut third_id = finish.clone();
    third_id["id"] = json!("third-response");
    cases.push(vec![bootstrap.clone(), content.clone(), third_id.clone()]);
    let mut call = chunk(
        json!({"tool_calls":[{
            "index":0,"id":"call-one","type":"function","function":{"name":"echo","arguments":"{}"}
        }]}),
        None,
    );
    call["id"] = json!("actual-response");
    cases.push(vec![bootstrap, call, third_id]);
    let mut same_content = content;
    same_content["id"] = json!("chat-one");
    cases.push(vec![same_content, finish]);
    for events in cases {
        let mut server = Server::start(vec![reply(events)]).await;
        let sink = Sink::default();
        let error = provider(&server)
            .with_transient_retries()
            .stream(request(), &sink)
            .await
            .unwrap_err();
        assert_eq!(
            error.protocol_failure(),
            Some(ProtocolFailureKind::StreamInvalid)
        );
        assert!(sink.0.lock().unwrap().is_empty());
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
}
