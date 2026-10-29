mod support;
use eve_llm_api::{
    ChatMessage, ChatRole, LlmError, LlmProvider, ModelRequest, ModelResponse, ProviderFailure,
    ProviderFailureKind,
};
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

#[tokio::test]
async fn responses_json_completion_uses_text_format_without_changing_ordinary_chat() {
    let mut server = Server::start(vec![
        Reply::json(final_response("{}")),
        Reply::json(final_response("自然回复")),
    ])
    .await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    provider.complete_json(request()).await.unwrap();
    let body = server.next().await.body;
    assert_eq!(body["text"]["format"], json!({"type":"json_object"}));
    assert!(body.get("response_format").is_none());
    assert_eq!(body["tool_choice"], "none");
    provider.complete(request()).await.unwrap();
    assert!(server.next().await.body.get("text").is_none());
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
    for (status, kind) in [
        (400, ProviderFailureKind::Rejected),
        (401, ProviderFailureKind::Unauthorized),
        (402, ProviderFailureKind::PaymentRequired),
        (429, ProviderFailureKind::RateLimited),
        (500, ProviderFailureKind::ServerError),
    ] {
        let mut reply = Reply::json(json!({"error":{"message":"fake-key private-input"}}));
        reply.status = status;
        let mut server = Server::start(vec![reply]).await;
        let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
        let error = provider.complete(request()).await.unwrap_err();
        assert_eq!(
            error,
            LlmError::ProviderFailed(ProviderFailure {
                kind,
                http_status: Some(status)
            })
        );
        assert!(!error.to_string().contains("private-input"));
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
        LlmError::ProviderFailed(ProviderFailure::http(307).unwrap())
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
            LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::ResponseTooLarge))
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
    // 达到单次输出上限的未完成响应记为截断。
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::Truncated))
    );
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
        LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::Network))
    );

    let mut reply = Reply::json(final_response("回复"));
    reply.declared_length = Some(reply.body.len() + 10);
    let mut server = Server::start(vec![reply]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::Network))
    );
    server.next().await;
}

#[tokio::test]
async fn lists_model_ids_from_the_same_service_with_bounds_and_filters() {
    let mut server = Server::start(vec![Reply::json(json!({"object":"list","data":[
        {"id":"vision-a"},{"id":"vision-a"},{"id":"bad id"},{"id":""},{"object":"model"},
        {"id":"text-b"},{"id":"vision-c"}
    ]}))])
    .await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    assert!(provider.models_url().as_str().ends_with("/v1/models"));
    assert_eq!(
        provider.list_models(2).await.unwrap(),
        ["vision-a", "text-b"]
    );
    let captured = server.next().await;
    assert!(captured.headers.starts_with("GET /v1/models "));
    assert!(
        captured
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fake-key")
    );
    let chat = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: "https://example.com/api/v3/chat/completions".into(),
            ..OpenAiConfig::chat("m")
        },
        "fake-key",
    )
    .unwrap();
    assert_eq!(
        chat.models_url().as_str(),
        "https://example.com/api/v3/models"
    );
}

#[tokio::test]
async fn model_list_failures_are_reported_not_guessed() {
    let mut missing = Reply::json(json!({"error":"no"}));
    missing.status = 404;
    let server = Server::start(vec![missing, Reply::json(json!({"models":[]}))]).await;
    let provider = OpenAiProvider::new(config(&server.url), "fake-key").unwrap();
    assert!(matches!(
        provider.list_models(10).await,
        Err(LlmError::ProviderFailed(_))
    ));
    assert!(matches!(
        provider.list_models(10).await,
        Err(LlmError::Protocol(_))
    ));
}

#[derive(Default)]
struct Usages(std::sync::Mutex<Vec<(String, eve_llm_api::ModelUsage)>>);
impl eve_llm_api::ModelUsageObserver for Usages {
    fn reported(&self, role: &str, usage: eve_llm_api::ModelUsage) {
        self.0.lock().unwrap().push((role.into(), usage));
    }
}

#[tokio::test]
async fn reports_provider_usage_and_cache_hits_to_the_observer() {
    let mut with_usage = final_response("好");
    with_usage["usage"] = json!({"input_tokens":1200,"output_tokens":5,"input_tokens_details":{"cached_tokens":1024}});
    let server = Server::start(vec![
        Reply::json(with_usage),
        Reply::json(final_response("再来")),
    ])
    .await;
    let usages = std::sync::Arc::new(Usages::default());
    let provider = OpenAiProvider::new(config(&server.url), "fake-key")
        .unwrap()
        .with_usage_observer(usages.clone(), "dialogue");
    provider.complete(request()).await.unwrap();
    provider.complete(request()).await.unwrap();
    let seen = usages.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, "dialogue");
    assert_eq!(
        (seen[0].1.input_tokens, seen[0].1.cached_input_tokens),
        (Some(1200), Some(1024))
    );
    assert!(seen[1].1.is_empty(), "未报告的用量如实为空");
}

#[tokio::test]
async fn auxiliary_requests_reject_chunked_overflow_before_eof() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for vision in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n20\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n").await.unwrap();
            // Hold the stream open: a bounded client rejects this chunk without EOF.
            std::future::pending::<()>().await;
        });
        let mut cfg = config(&url);
        cfg.max_response_bytes = 16;
        let provider = OpenAiProvider::new(cfg, "fake-key").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            if vision {
                provider
                    .describe_image("describe", "image/png", b"fixture")
                    .await
                    .map(|_| ())
            } else {
                provider.list_models(10).await.map(|_| ())
            }
        })
        .await;
        server.abort();
        assert!(
            matches!(&result, Ok(Err(LlmError::ProviderFailed(failure))) if failure.kind == ProviderFailureKind::ResponseTooLarge),
            "vision={vision}: {result:?}"
        );
    }
}
