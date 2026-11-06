mod support;

use eve_llm_api::{
    ChatMessage, ChatRole, LlmError, LlmProvider, ModelRequest, ProviderFailure,
    ProviderFailureKind,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider, StreamTimeoutPolicy};
use serde_json::{Value, json};
use std::{process::Command, time::Duration};
use support::{Reply, Server, final_response};

fn request() -> ModelRequest {
    ModelRequest {
        session: None,
        messages: vec![ChatMessage::text(ChatRole::User, "fixture-private-input")],
        tools: vec![],
    }
}

#[test]
fn transport_diagnostics_preserve_failures_and_correlate_safe_phases() {
    const CHILD: &str = "EVE_TEST_TRANSPORT_DIAGNOSTICS";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transport_diagnostics_preserve_failures_and_correlate_safe_phases",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        for private in ["fixture-private", "127.0.0.1", "http://", "/v1/"] {
            assert!(!stderr.contains(private));
        }
        let records = |tag: &str| -> Vec<Value> {
            stderr
                .lines()
                .filter_map(|line| line.strip_prefix(tag))
                .map(|line| serde_json::from_str(line.trim()).unwrap())
                .collect()
        };
        let failures = records("EVE_HTTP_TRANSPORT ");
        let timing = records("EVE_HTTP_TIMING ");
        assert_eq!(failures.len(), 3);
        for (record, phase) in
            failures
                .iter()
                .zip(["before_headers", "response_body", "stream_body"])
        {
            assert_eq!(record["phase"], phase);
            assert!(record["call"].is_u64());
            assert_eq!(record["attempt"], 1);
            assert!(record["role"].is_null());
            assert_eq!(record["is_timeout"], false);
            assert!(record["is_connect"].is_boolean());
            assert!(record["io_kind"].is_null() || record["io_kind"].is_string());
            assert!(record["io_os_error"].is_null() || record["io_os_error"].is_i64());
            let phases: Vec<_> = timing
                .iter()
                .filter(|event| event["request"] == record["request"])
                .map(|event| {
                    assert_eq!(event["call"], record["call"]);
                    assert_eq!(event["attempt"], record["attempt"]);
                    assert_eq!(event["role"], record["role"]);
                    event["phase"].as_str().unwrap()
                })
                .collect();
            assert_eq!(phases.first(), Some(&"dispatch"));
            assert_eq!(phases.last(), Some(&"closed_without_decoded_response"));
            assert!(!phases.contains(&"decoded"));
            assert_eq!(phases.contains(&"headers"), phase != "before_headers");
        }
        let timeouts = records("EVE_MODEL_TIMEOUT ");
        assert_eq!(timeouts.len(), 1);
        assert_eq!(timeouts[0]["phase"], "first_response");
        assert!(timing.iter().any(|event| {
            event["call"] == timeouts[0]["call"]
                && event["request"] == timeouts[0]["request"]
                && event["attempt"] == timeouts[0]["attempt"]
                && event["phase"] == "closed_without_decoded_response"
        }));
        assert!(!stderr.contains("EVE_MODEL_RETRY"));
        return;
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // 请求已完整发送，返回不合法的响应头：只应记录 before_headers。
            for enabled in [false, true] {
                let mut reply = Reply::json(final_response("fixture-private-output"));
                reply.headers.push(("fixture".into(), "\u{7f}".into()));
                let mut server = Server::start(vec![reply]).await;
                let mut provider = provider(&server.url);
                if enabled {
                    provider = provider.with_timing_diagnostics();
                }
                assert_eq!(
                    provider.complete(request()).await.unwrap_err(),
                    LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::Network))
                );
                let sent = server.next().await;
                assert!(sent.headers.starts_with("POST /v1/responses HTTP/1.1"));
                assert_eq!(sent.body["model"], "fixture-model");
                assert!(server.requests.try_recv().is_err());
            }

            // 响应头已收到，但连接在声明的响应体长度前关闭。
            for streaming in [false, true] {
                let mut reply = Reply::json(json!({}));
                reply.body.clear();
                reply.declared_length = Some(1024);
                if streaming {
                    reply
                        .headers
                        .push(("Content-Type".into(), "text/event-stream".into()));
                }
                let mut server = Server::start(vec![reply]).await;
                let mut provider = provider(&server.url).with_timing_diagnostics();
                if streaming {
                    provider = provider.with_streamed_completions();
                }
                assert_eq!(
                    provider.complete(request()).await.unwrap_err(),
                    LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::Network))
                );
                server.next().await;
                assert!(server.requests.try_recv().is_err());
            }

            // 本地首响应期限先到期，仍走原分阶段日志，不伪造传输错误。
            let mut reply = Reply::json(json!({}));
            reply.header_delay = Duration::from_secs(5);
            let mut server = Server::start(vec![reply]).await;
            let provider = provider(&server.url)
                .with_timing_diagnostics()
                .with_streamed_completions()
                .with_stream_timeout_policy(StreamTimeoutPolicy {
                    first_response: Duration::from_millis(100),
                    idle: Duration::from_secs(1),
                    total: Duration::from_secs(3),
                })
                .unwrap();
            assert_eq!(
                provider.complete(request()).await.unwrap_err(),
                LlmError::ProviderTimeout
            );
            server.next().await;
            assert!(server.requests.try_recv().is_err());
        });
}

fn provider(url: &str) -> OpenAiProvider {
    OpenAiProvider::new(
        OpenAiConfig {
            responses_url: url.into(),
            request_timeout: Duration::from_secs(3),
            ..OpenAiConfig::new("fixture-model")
        },
        "fixture-private-key",
    )
    .unwrap()
}

struct IgnoreUsage;
impl eve_llm_api::ModelUsageObserver for IgnoreUsage {
    fn reported(&self, _: &str, _: eve_llm_api::ModelUsage) {}
}

struct PendingSink;
impl eve_llm_api::ModelTextSink for PendingSink {
    fn text_delta(&self, _: String) -> eve_llm_api::LlmFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn retry_and_outer_timeout_are_correlated_without_logging_private_roles() {
    const CHILD: &str = "EVE_TEST_DIAGNOSTIC_CORRELATION";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "retry_and_outer_timeout_are_correlated_without_logging_private_roles",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        for private in ["fixture-private", "127.0.0.1", "http://", "/v1/"] {
            assert!(!stderr.contains(private));
        }
        let records = |tag: &str| -> Vec<Value> {
            stderr
                .lines()
                .filter_map(|line| line.strip_prefix(tag))
                .map(|line| serde_json::from_str(line.trim()).unwrap())
                .collect()
        };
        let retries = records("EVE_MODEL_RETRY ");
        let timings = records("EVE_HTTP_TIMING ");
        let transports = records("EVE_HTTP_TRANSPORT ");
        let timeouts = records("EVE_MODEL_TIMEOUT ");
        assert_eq!(retries.len(), 3);
        assert_eq!(retries.iter().filter(|r| r["role"].is_null()).count(), 1);
        for retry in &retries {
            assert!(retry["call"].is_u64());
            assert!(retry["request"].is_u64());
            assert_eq!(retry["attempt"], 1);
            assert_eq!(retry["retry"], 1);
            assert_eq!(retry["max_retries"], 3);
            assert_eq!(retry["delay_ms"], 1000);
            let events: Vec<_> = timings
                .iter()
                .filter(|event| event["call"] == retry["call"])
                .collect();
            if retry["role"].is_null() {
                // 关闭计时仍沿用原有 retry 日志；未知用途不输出原始 role。
                assert!(events.is_empty());
                continue;
            }
            assert!(matches!(
                retry["role"].as_str(),
                Some("dialogue" | "speech")
            ));
            assert!(events.iter().all(|event| event["role"] == retry["role"]));
            let starts: Vec<_> = events
                .iter()
                .filter(|event| event["phase"] == "dispatch")
                .collect();
            assert_eq!(starts.len(), 2);
            assert_eq!(starts[0]["request"], retry["request"]);
            assert_eq!(starts[0]["attempt"], 1);
            assert_eq!(starts[1]["attempt"], 2);
            assert_ne!(starts[0]["request"], starts[1]["request"]);
            assert!(events.iter().any(|event| {
                event["request"] == retry["request"]
                    && event["phase"] == "closed_without_decoded_response"
            }));
            assert!(events.iter().any(|event| {
                event["request"] == starts[1]["request"] && event["phase"] == "decoded"
            }));
        }
        let mut calls: Vec<_> = retries
            .iter()
            .map(|r| r["call"].as_u64().unwrap())
            .collect();
        calls.sort_unstable();
        calls.dedup();
        assert_eq!(calls.len(), 3);
        assert_eq!(transports.len(), 1);
        assert_eq!(transports[0]["phase"], "response_body");
        assert_eq!(transports[0]["role"], "dialogue");
        let retry = retries.iter().find(|r| r["role"] == "dialogue").unwrap();
        for key in ["call", "request", "attempt", "role"] {
            assert_eq!(transports[0][key], retry[key]);
        }
        assert_eq!(timeouts.len(), 1);
        let timeout = &timeouts[0];
        assert_eq!(timeout["phase"], "total");
        assert_eq!(timeout["role"], "progress");
        assert_eq!(timeout["attempt"], 1);
        assert!(!calls.contains(&timeout["call"].as_u64().unwrap()));
        let events: Vec<_> = timings
            .iter()
            .filter(|event| event["call"] == timeout["call"])
            .collect();
        assert!(events.iter().all(|event| {
            event["request"] == timeout["request"]
                && event["attempt"] == timeout["attempt"]
                && event["role"] == timeout["role"]
        }));
        let phases: Vec<_> = events
            .iter()
            .map(|event| event["phase"].as_str().unwrap())
            .collect();
        assert_eq!(
            phases,
            [
                "dispatch",
                "headers",
                "first_stream_chunk",
                "closed_without_decoded_response"
            ]
        );
        return;
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut broken = Reply::json(final_response("fixture-private-output"));
            broken.declared_length = Some(broken.body.len() + 10);
            let mut unavailable = Reply::json(json!({"error":"fixture-private-error"}));
            unavailable.status = 503;
            let mut dialogue = Server::start(vec![broken, Reply::json(final_response("ok"))]).await;
            let mut speech = Server::start(vec![unavailable, Reply::json(final_response("ok"))]).await;
            let observed = |server: &Server, role: &str| {
                provider(&server.url)
                    .with_usage_observer(std::sync::Arc::new(IgnoreUsage), role)
                    .with_timing_diagnostics()
                    .with_transient_retries()
            };
            let dialogue_provider = observed(&dialogue, "dialogue");
            let speech_provider = observed(&speech, "speech");
            let (first, second) = tokio::join!(
                dialogue_provider.complete(request()),
                speech_provider.complete(request())
            );
            assert!(first.is_ok());
            assert!(second.is_ok());
            for server in [&mut dialogue, &mut speech] {
                let first = server.next().await;
                let second = server.next().await;
                assert_eq!(first.body, second.body);
                assert_eq!(first.headers, second.headers);
                assert!(server.requests.try_recv().is_err());
            }

            let mut unavailable = Reply::json(json!({}));
            unavailable.status = 503;
            let mut disabled = Server::start(vec![unavailable, Reply::json(final_response("ok"))]).await;
            assert!(provider(&disabled.url)
                .with_usage_observer(std::sync::Arc::new(IgnoreUsage), "fixture-private-role")
                .with_transient_retries()
                .complete(request())
                .await
                .is_ok());
            let first = disabled.next().await;
            let second = disabled.next().await;
            assert_eq!(first.body, second.body);
            assert_eq!(first.headers, second.headers);
            assert!(disabled.requests.try_recv().is_err());

            // 响应已经完整到达，sink 等待让外层总期限先取消；不会经过传输错误映射。
            let mut reply = Reply::json(json!({}));
            reply.headers.push(("Content-Type".into(), "text/event-stream".into()));
            reply.body = format!(
                "data: {}\n\n",
                json!({"id":"fixture","choices":[{"index":0,"delta":{"content":"fixture-private-output"},"finish_reason":"stop"}]})
            ).into_bytes();
            let mut server = Server::start(vec![reply]).await;
            let p = OpenAiProvider::new(OpenAiConfig {
                responses_url: server.url.replace("/responses", "/chat/completions"),
                request_timeout: Duration::from_millis(150),
                ..OpenAiConfig::chat("fixture-model")
            }, "fixture-private-key")
            .unwrap()
            .with_usage_observer(std::sync::Arc::new(IgnoreUsage), "progress")
            .with_timing_diagnostics()
            .with_transient_retries();
            assert_eq!(p.stream(request(), &PendingSink).await.unwrap_err(), LlmError::ProviderTimeout);
            server.next().await;
            assert!(server.requests.try_recv().is_err());
        });
}
