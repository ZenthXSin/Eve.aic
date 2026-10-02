#[allow(dead_code)]
mod support;
use eve_llm_api::{
    ChatMessage, ChatRole, LlmError, LlmFuture, LlmProvider, ModelRequest, ModelResponse,
    ModelTextSink,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider};
use serde_json::json;
use std::time::Duration;
use support::{Reply, Server};

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "你好")],
        tools: vec![],
    }
}
fn reply(text: &str) -> Reply {
    Reply::json(json!({"choices":[{"index":0,"finish_reason":"stop",
        "message":{"role":"assistant","content":text}}]}))
}
fn config(url: &str) -> OpenAiConfig {
    OpenAiConfig::chat("deepseek-v4.1-flash")
        .with_base_url(&url.replace("/responses", "/chat/completions"))
        .unwrap()
}
#[test]
fn explicit_protocol_normalizes_roots_versions_and_complete_chat_urls() {
    for (base, expected) in [
        (
            "https://example.com",
            "https://example.com/v1/chat/completions",
        ),
        (
            "https://example.com/v1/",
            "https://example.com/v1/chat/completions",
        ),
        (
            "https://example.com/proxy/v1",
            "https://example.com/proxy/v1/chat/completions",
        ),
        (
            "https://example.com/v1/chat/completions/",
            "https://example.com/v1/chat/completions",
        ),
    ] {
        let cfg = OpenAiConfig::chat("model").with_base_url(base).unwrap();
        assert_eq!(cfg.protocol, OpenAiProtocol::ChatCompletions);
        assert_eq!(cfg.responses_url, expected);
        assert!(OpenAiProvider::new(cfg, "fixture-key").is_ok());
    }
    assert!(
        OpenAiConfig::chat("model")
            .with_base_url("https://example.com/v1/responses")
            .is_err()
    );
    assert!(
        OpenAiConfig::new("model")
            .with_base_url("https://example.com/v1/chat/completions")
            .is_err()
    );
}
#[tokio::test]
async fn real_http_uses_chat_envelope_authentication_and_optional_chat_parameters() {
    let mut server = Server::start(vec![reply("默认回复"), reply("显式回复")]).await;
    let mut cfg = config(&server.url);
    let provider = OpenAiProvider::new(cfg.clone(), "fixture-key").unwrap();
    let pending = provider.complete(request());
    assert!(server.requests.try_recv().is_err());
    assert_eq!(
        pending.await.unwrap(),
        ModelResponse::Final {
            text: "默认回复".into()
        }
    );
    let captured = server.next().await;
    assert!(
        captured
            .headers
            .starts_with("POST /v1/chat/completions HTTP/1.1")
    );
    assert!(
        captured
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    assert_eq!(captured.body["model"], "deepseek-v4.1-flash");
    assert_eq!(captured.body["messages"][0]["content"], "你好");
    assert_eq!(captured.body["stream"], false);
    for field in [
        "input",
        "tools",
        "reasoning",
        "reasoning_effort",
        "max_output_tokens",
        "max_tokens",
    ] {
        assert!(captured.body.get(field).is_none());
    }
    cfg.max_output_tokens = Some(512);
    cfg.reasoning_effort = Some("none".into());
    OpenAiProvider::new(cfg, "fixture-key")
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    let body = server.next().await.body;
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["reasoning_effort"], "none");
    assert!(body.get("max_output_tokens").is_none());
    assert!(!body.to_string().contains("fixture-key"));
}
#[tokio::test]
async fn failures_and_size_limits_are_sanitized_without_retry() {
    let mut bad = Reply::json(json!({"error":"fixture-key private body"}));
    bad.status = 500;
    let mut server = Server::start(vec![bad]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fixture-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::Provider("OpenAI HTTP 500".into())
    );
    server.next().await;
    assert!(server.requests.try_recv().is_err());
    for chunked in [false, true] {
        let mut oversized = reply("超过上限");
        oversized.chunked = chunked;
        let mut server = Server::start(vec![oversized]).await;
        let cfg = OpenAiConfig {
            max_response_bytes: 16,
            ..config(&server.url)
        };
        let error = OpenAiProvider::new(cfg, "fixture-key")
            .unwrap()
            .complete(request())
            .await
            .unwrap_err();
        assert!(matches!(error, LlmError::Provider(_)));
        assert!(!error.to_string().contains("超过上限"));
        server.next().await;
    }
}
#[tokio::test]
async fn chat_deadline_and_cancellation_allow_subsequent_calls() {
    let mut delayed = reply("迟到");
    delayed.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![delayed, reply("下一轮")]).await;
    let cfg = OpenAiConfig {
        request_timeout: Duration::from_millis(200),
        ..config(&server.url)
    };
    let provider = OpenAiProvider::new(cfg, "fixture-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    server.next().await;
    assert!(provider.complete(request()).await.is_ok());
    server.next().await;

    let mut delayed = reply("旧回复");
    delayed.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![delayed, reply("新回复")]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fixture-key").unwrap();
    let mut pending = provider.complete(request());
    tokio::select! {
        _ = server.next() => {}
        response = &mut pending => panic!("must remain pending: {response:?}"),
    }
    drop(pending);
    assert!(provider.complete(request()).await.is_ok());
    server.next().await;
}
#[tokio::test]
async fn chat_stream_and_invalid_input_fail_before_http() {
    struct Sink;
    impl ModelTextSink for Sink {
        fn text_delta(&self, _: String) -> LlmFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    let mut server = Server::start(vec![]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fixture-key").unwrap();
    assert!(matches!(
        provider.stream(request(), &Sink).await,
        Err(LlmError::Unsupported(_))
    ));
    assert!(matches!(
        provider
            .complete(ModelRequest {
                messages: vec![],
                tools: vec![]
            })
            .await,
        Err(LlmError::Protocol(_))
    ));
    assert!(server.requests.try_recv().is_err());
}
