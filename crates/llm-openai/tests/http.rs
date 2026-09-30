mod support;
use eve_llm_api::{ChatMessage, ChatRole, LlmError, LlmProvider, ModelRequest, ModelResponse};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::json;
use std::time::Duration;
use support::{Reply, Server, final_response};

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "你好")],
        tools: vec![],
    }
}
fn config(url: &str) -> OpenAiConfig {
    OpenAiConfig {
        responses_url: url.into(),
        ..OpenAiConfig::new("fixture-model")
    }
}

#[test]
fn validates_config_and_credentials_without_network_or_secret_diagnostics() {
    for url in [
        "invalid",
        "http://example.com/v1/responses",
        "file:///tmp/key",
        "https://user:secret@example.com",
        "https://example.com?secret=key",
        "https://example.com#secret",
    ] {
        let error = OpenAiProvider::new(config(url), "fake-key").err().unwrap();
        assert!(matches!(error, LlmError::Configuration(_)));
        assert!(!error.to_string().contains("secret"));
    }
    for key in ["", " ", "secret\n", "secret key", "secret\0"] {
        let error = OpenAiProvider::new(OpenAiConfig::new("model"), key)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("secret"));
    }
    for cfg in [
        OpenAiConfig::new(" "),
        OpenAiConfig {
            request_timeout: Duration::ZERO,
            ..OpenAiConfig::new("model")
        },
        OpenAiConfig {
            max_response_bytes: 0,
            ..OpenAiConfig::new("model")
        },
    ] {
        assert!(OpenAiProvider::new(cfg, "fake-key").is_err());
    }
}

#[tokio::test]
async fn sends_lazy_authenticated_post_and_parses_real_http_json() {
    let mut server = Server::start(vec![Reply::json(final_response("你好，Eve"))]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    let future = provider.complete(request());
    assert!(server.requests.try_recv().is_err());
    assert_eq!(
        future.await.unwrap(),
        ModelResponse::Final {
            text: "你好，Eve".into()
        }
    );
    let captured = server.next().await;
    assert!(captured.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        captured
            .headers
            .to_lowercase()
            .contains("authorization: bearer fake-key")
    );
    assert!(!captured.body.to_string().contains("fake-key"));
    assert_eq!(captured.body["model"], "fixture-model");
    assert_eq!(captured.body["input"][0]["content"], "你好");
    assert_eq!(captured.body["store"], false);
    assert!(captured.body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn invalid_request_never_reaches_http() {
    let mut server = Server::start(vec![]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
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

#[tokio::test]
async fn http_errors_are_sanitized_and_not_retried() {
    for status in [400, 401, 429, 500] {
        let mut reply = Reply::json(json!({"error":{"message":"fake-key private-input"}}));
        reply.status = status;
        let mut server = Server::start(vec![reply]).await;
        let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
        assert_eq!(
            provider.complete(request()).await.unwrap_err(),
            LlmError::Provider(format!("OpenAI HTTP {status}"))
        );
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn redirects_are_not_followed_with_credentials() {
    let mut target = Server::start(vec![]).await;
    let mut reply = Reply::json(json!({}));
    reply.status = 307;
    reply.headers.push(("Location".into(), target.url.clone()));
    let mut server = Server::start(vec![reply]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::Provider("OpenAI HTTP 307".into())
    );
    server.next().await;
    assert!(target.requests.try_recv().is_err());
}

#[tokio::test]
async fn bounds_fixed_and_chunked_response_bodies() {
    for chunked in [false, true] {
        let mut reply = Reply::json(final_response("超大响应"));
        reply.chunked = chunked;
        let mut server = Server::start(vec![reply]).await;
        let cfg = OpenAiConfig {
            max_response_bytes: 20,
            ..config(&server.url)
        };
        let provider = OpenAiProvider::new(cfg, "fake-key").unwrap();
        assert_eq!(
            provider.complete(request()).await.unwrap_err(),
            LlmError::Provider("OpenAI 响应超过字节上限".into())
        );
        server.next().await;
    }
}

#[tokio::test]
async fn deadline_covers_body_and_allows_next_call() {
    let mut delayed = Reply::json(final_response("延迟"));
    delayed.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![delayed, Reply::json(final_response("下一轮"))]).await;
    let cfg = OpenAiConfig {
        request_timeout: Duration::from_millis(200),
        ..config(&server.url)
    };
    let provider = OpenAiProvider::new(cfg, "fake-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    server.next().await;
    assert_eq!(
        provider.complete(request()).await.unwrap(),
        ModelResponse::Final {
            text: "下一轮".into()
        }
    );
    server.next().await;
}

#[tokio::test]
async fn rejects_bad_json_and_incomplete_http_responses_without_body_leakage() {
    let mut reply = Reply::json(json!(null));
    reply.body = b"invalid fake-key private-input".to_vec();
    let mut server = Server::start(vec![reply, Reply::json(json!({"status":"incomplete", "output":[], "incomplete_details":{"reason":"max_output_tokens"}}))]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::Protocol("OpenAI 响应不是有效 JSON".into())
    );
    server.next().await;
    assert!(matches!(
        provider.complete(request()).await,
        Err(LlmError::Provider(_))
    ));
    server.next().await;
}

#[tokio::test]
async fn dropping_an_inflight_http_future_does_not_block_the_next_request() {
    let mut reply = Reply::json(final_response("过时回复"));
    reply.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![reply, Reply::json(final_response("新回复"))]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    let mut future = provider.complete(request());
    tokio::select! {
        _ = server.next() => {},
        result = &mut future => panic!("request should still be pending: {result:?}"),
    }
    drop(future);
    assert_eq!(
        provider.complete(request()).await.unwrap(),
        ModelResponse::Final {
            text: "新回复".into()
        }
    );
    server.next().await;
}

#[tokio::test]
async fn transport_failures_and_truncated_bodies_are_sanitized() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    drop(listener);
    let provider = OpenAiProvider::new(config(&url), "fake-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::Provider("OpenAI 网络请求失败".into())
    );

    let mut reply = Reply::json(final_response("回复"));
    reply.declared_length = Some(reply.body.len() + 10);
    let mut server = Server::start(vec![reply]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::Provider("OpenAI 网络请求失败".into())
    );
    server.next().await;
}
