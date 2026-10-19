#[allow(dead_code)]
mod support;
use eve_llm_api::{ChatMessage, ChatRole, LlmError, LlmProvider, ModelRequest, ModelResponse};
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider};
use serde_json::json;
use std::time::Duration;
use support::{Reply, Server, final_response};

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "原始问题")],
        tools: vec![],
    }
}
fn failure(status: u16) -> Reply {
    let mut reply = Reply::json(json!({"error":"PRIVATE_ERROR_BODY"}));
    reply.status = status;
    reply
}
fn provider(server: &Server, timeout: Duration) -> OpenAiProvider {
    let mut config = OpenAiConfig::new("fixture");
    config.responses_url = server.url.clone();
    config.request_timeout = timeout;
    OpenAiProvider::new(config, "fake-key")
        .unwrap()
        .with_transient_retries()
}

#[tokio::test]
async fn responses_and_chat_retry_same_body_without_changing_quality_settings() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let value = if protocol == OpenAiProtocol::Responses {
            final_response("恢复成功")
        } else {
            json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"恢复成功"}}]})
        };
        let mut server = Server::start(vec![failure(429), failure(503), Reply::json(value)]).await;
        let mut config = OpenAiConfig::new("original-model");
        config.responses_url = server.url.clone();
        config.protocol = protocol;
        config.reasoning_effort = Some("medium".into());
        config.max_output_tokens = Some(8192);
        let p = OpenAiProvider::new(config, "fake-key")
            .unwrap()
            .with_transient_retries();
        assert_eq!(
            p.complete(request()).await.unwrap(),
            ModelResponse::Final {
                text: "恢复成功".into()
            }
        );
        let first = server.next().await.body;
        assert_eq!(first["model"], "original-model");
        assert_eq!(server.next().await.body, first);
        assert_eq!(server.next().await.body, first);
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn permanent_statuses_and_malformed_success_are_not_retried() {
    for reply in [
        failure(400),
        failure(401),
        failure(402),
        failure(403),
        failure(501),
        Reply::json(json!({"invalid":"PRIVATE_ERROR_BODY"})),
    ] {
        let mut server = Server::start(vec![reply, Reply::json(final_response("不应请求"))]).await;
        let error = provider(&server, Duration::from_secs(30))
            .complete(request())
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_ERROR_BODY"));
        server.next().await;
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn transient_failure_stops_after_four_requests() {
    let mut server = Server::start(vec![
        failure(503),
        failure(503),
        failure(503),
        failure(503),
        Reply::json(final_response("不应请求")),
    ])
    .await;
    let error = provider(&server, Duration::from_secs(30))
        .complete(request())
        .await
        .unwrap_err();
    assert_eq!(error.provider_http_status(), Some(503));
    let first = server.next().await.body;
    for _ in 0..3 {
        assert_eq!(server.next().await.body, first);
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn incomplete_http_body_is_retried_before_any_result_is_exposed() {
    let mut broken = Reply::json(final_response("部分结果不得提交"));
    broken.declared_length = Some(broken.body.len() + 10);
    let mut server = Server::start(vec![broken, Reply::json(final_response("完整结果"))]).await;
    assert_eq!(
        provider(&server, Duration::from_secs(30))
            .complete_json(request())
            .await
            .unwrap(),
        ModelResponse::Final {
            text: "完整结果".into()
        }
    );
    let first = server.next().await.body;
    assert_eq!(first["text"]["format"]["type"], "json_object");
    assert_eq!(server.next().await.body, first);
}

#[tokio::test]
async fn retry_after_beyond_remaining_budget_does_not_retry_early() {
    let mut limited = failure(429);
    limited.headers.push(("Retry-After".into(), "60".into()));
    let mut server = Server::start(vec![limited, Reply::json(final_response("不应请求"))]).await;
    let error = provider(&server, Duration::from_secs(2))
        .complete(request())
        .await
        .unwrap_err();
    assert_eq!(error.provider_http_status(), Some(429));
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn cancellation_during_backoff_stops_future_attempts() {
    let mut server =
        Server::start(vec![failure(503), Reply::json(final_response("不应请求"))]).await;
    let p = provider(&server, Duration::from_secs(30));
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

#[tokio::test]
async fn original_total_deadline_bounds_body_wait_and_retries() {
    let mut slow = Reply::json(final_response("迟到结果"));
    slow.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![slow, Reply::json(final_response("不应请求"))]).await;
    let start = std::time::Instant::now();
    assert!(matches!(
        provider(&server, Duration::from_millis(100))
            .complete(request())
            .await,
        Err(LlmError::ProviderTimeout)
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn retry_after_is_honored_within_budget() {
    let mut limited = failure(429);
    limited.headers.push(("Retry-After".into(), "2".into()));
    let mut server = Server::start(vec![limited, Reply::json(final_response("恢复"))]).await;
    let start = std::time::Instant::now();
    provider(&server, Duration::from_secs(10))
        .complete(request())
        .await
        .unwrap();
    assert!(start.elapsed() >= Duration::from_secs(2));
    server.next().await;
    server.next().await;
}

#[tokio::test]
async fn opting_in_does_not_retry_streaming_requests() {
    struct Sink;
    impl eve_llm_api::ModelTextSink for Sink {
        fn text_delta(&self, _: String) -> eve_llm_api::LlmFuture<'_, ()> {
            Box::pin(async { panic!("failed headers must not emit text") })
        }
    }
    let mut server =
        Server::start(vec![failure(503), Reply::json(final_response("不应请求"))]).await;
    let error = provider(&server, Duration::from_secs(30))
        .stream(request(), &Sink)
        .await
        .unwrap_err();
    assert_eq!(error.provider_http_status(), Some(503));
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}
