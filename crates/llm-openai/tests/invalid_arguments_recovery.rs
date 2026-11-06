#[allow(dead_code)]
#[path = "support/mod.rs"]
mod http_support;
#[path = "support/sse.rs"]
mod sse;

use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmError, LlmProvider, ModelRequest, ModelResponse,
    ModelSession, ProtocolFailureKind, ToolCall, ToolDefinition, ToolResult,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider};
use http_support::{Reply, Server};
use serde_json::json;
use std::time::Duration;

fn request() -> ModelRequest {
    let prior = ToolCall {
        id: "already-executed".into(),
        name: "echo".into(),
        arguments: json!({"text":"prior action"}),
    };
    ModelRequest {
        session: Some(
            ModelSession::scoped(
                "dialogue",
                &ContextScope {
                    session_id: "fixture-session".into(),
                    user_id: "fixture-user".into(),
                },
            )
            .unwrap()
            .with_turn("turn-1085")
            .unwrap()
            .for_request(2),
        ),
        messages: vec![
            ChatMessage::text(ChatRole::User, "继续已有结果"),
            ChatMessage::assistant_tool_calls(vec![prior.clone()]).unwrap(),
            ChatMessage::tool_results(vec![
                ToolResult::success(prior.id, json!({"receipt":"already completed"})).unwrap(),
            ])
            .unwrap(),
        ],
        tools: vec![ToolDefinition {
            name: "echo".into(),
            description: "fixture".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            required_permissions: vec![],
            concurrency: None,
        }],
    }
}

fn reply(protocol: OpenAiProtocol, streaming: bool, arguments: &str) -> Reply {
    let item = sse::call("next-call", "echo", arguments);
    if protocol == OpenAiProtocol::Responses {
        return if streaming {
            sse::reply(&sse::events(vec![item]))
        } else {
            Reply::json(json!({"status":"completed","output":[item]}))
        };
    }
    let call = json!({"id":"next-call","type":"function",
        "function":{"name":"echo","arguments":arguments}});
    if !streaming {
        return Reply::json(json!({"choices":[{"index":0,"finish_reason":"tool_calls",
            "message":{"role":"assistant","content":null,"tool_calls":[call]}}]}));
    }
    let mut call = call;
    call["index"] = json!(0);
    let event = json!({"id":"fixture","choices":[{"index":0,"finish_reason":"tool_calls",
        "delta":{"role":"assistant","tool_calls":[call]}}]});
    let mut reply = Reply::json(json!({}));
    reply.headers = vec![("content-type".into(), "text/event-stream".into())];
    reply.chunked = true;
    reply.body = format!("data: {event}\n\ndata: [DONE]\n\n").into_bytes();
    reply
}

fn provider(server: &Server, protocol: OpenAiProtocol, streaming: bool) -> OpenAiProvider {
    let provider = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            protocol,
            request_timeout: Duration::from_secs(20),
            ..OpenAiConfig::new("fixture")
        },
        "fixture-key",
    )
    .unwrap();
    if streaming {
        provider.with_streamed_completions()
    } else {
        provider
    }
}

#[tokio::test]
async fn malformed_arguments_recover_once_with_same_receipts_body_and_session() {
    for protocol in [OpenAiProtocol::ChatCompletions, OpenAiProtocol::Responses] {
        for streaming in [false, true] {
            let mut server = Server::start(vec![
                reply(protocol, streaming, "{invalid-private-arguments"),
                reply(protocol, streaming, r#"{"text":"recovered"}"#),
            ])
            .await;
            let response = provider(&server, protocol, streaming)
                .with_transient_retries()
                .with_tool_argument_recovery()
                .complete(request())
                .await
                .unwrap();
            assert_eq!(
                response,
                ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "next-call".into(),
                        name: "echo".into(),
                        arguments: json!({"text":"recovered"}),
                    }]
                }
            );
            let first = server.next().await;
            let second = server.next().await;
            assert_eq!(first.body, second.body);
            assert_eq!(first.headers, second.headers);
            assert!(first.body.to_string().contains("already completed"));
            assert!(first.body.get("prompt_cache_key").is_some());
            assert!(first.body.get("temperature").is_none());
            assert!(
                !second
                    .body
                    .to_string()
                    .contains("invalid-private-arguments")
            );
            assert!(server.requests.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn second_invalid_batch_stops_without_exposing_or_repairing_arguments() {
    for protocol in [OpenAiProtocol::ChatCompletions, OpenAiProtocol::Responses] {
        let mut server = Server::start(vec![
            reply(protocol, true, "{broken"),
            reply(protocol, true, "{broken-again"),
            reply(protocol, true, r#"{"text":"must not request"}"#),
        ])
        .await;
        let error = provider(&server, protocol, true)
            .with_transient_retries()
            .with_tool_argument_recovery()
            .complete(request())
            .await
            .unwrap_err();
        assert_eq!(
            error.protocol_failure(),
            Some(ProtocolFailureKind::ToolArguments)
        );
        assert!(!error.to_string().contains("broken"));
        server.next().await;
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn recovery_requires_opt_in_and_real_streaming_does_not_replay() {
    struct Sink;
    impl eve_llm_api::ModelTextSink for Sink {
        fn text_delta(&self, _: String) -> eve_llm_api::LlmFuture<'_, ()> {
            Box::pin(async { panic!("invalid tool draft must stay private") })
        }
    }
    for (streaming, recovery, transient) in [
        (false, false, true),
        (false, true, false),
        (true, true, true),
    ] {
        let protocol = OpenAiProtocol::ChatCompletions;
        let mut server = Server::start(vec![reply(protocol, streaming, "{bad")]).await;
        let mut p = provider(&server, protocol, streaming);
        if recovery {
            p = p.with_tool_argument_recovery();
        }
        if transient {
            p = p.with_transient_retries();
        }
        let result = if streaming {
            p.stream(request(), &Sink).await
        } else {
            p.complete(request()).await
        };
        assert!(matches!(result, Err(LlmError::Protocol(_))));
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn argument_recovery_and_transport_failures_share_the_original_attempt_limit() {
    let protocol = OpenAiProtocol::ChatCompletions;
    let unavailable = || {
        let mut reply = Reply::json(json!({}));
        reply.status = 503;
        reply
    };
    let mut server = Server::start(vec![
        unavailable(),
        unavailable(),
        unavailable(),
        reply(protocol, false, "{last-bad-batch"),
        reply(protocol, false, r#"{"text":"must not request"}"#),
    ])
    .await;
    let error = provider(&server, protocol, false)
        .with_transient_retries()
        .with_tool_argument_recovery()
        .complete(request())
        .await
        .unwrap_err();
    assert_eq!(
        error.protocol_failure(),
        Some(ProtocolFailureKind::ToolArguments)
    );
    for _ in 0..4 {
        server.next().await;
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn arguments_with_less_than_backoff_remaining_keep_the_original_error() {
    let protocol = OpenAiProtocol::ChatCompletions;
    let mut server = Server::start(vec![reply(protocol, false, "{bad")]).await;
    let p = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            protocol,
            request_timeout: Duration::from_millis(900),
            ..OpenAiConfig::new("fixture")
        },
        "fixture-key",
    )
    .unwrap()
    .with_transient_retries()
    .with_tool_argument_recovery();
    let error = p.complete(request()).await.unwrap_err();
    assert_eq!(
        error.protocol_failure(),
        Some(ProtocolFailureKind::ToolArguments)
    );
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn cancelling_argument_backoff_never_starts_the_recovery_request() {
    let protocol = OpenAiProtocol::ChatCompletions;
    let mut server = Server::start(vec![reply(protocol, false, "{bad")]).await;
    let p = provider(&server, protocol, false)
        .with_transient_retries()
        .with_tool_argument_recovery();
    let task = tokio::spawn(async move { p.complete(request()).await });
    server.next().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), server.requests.recv())
            .await
            .is_err()
    );
}
