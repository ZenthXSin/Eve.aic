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
            assert_eq!(record["is_timeout"], false);
            assert!(record["is_connect"].is_boolean());
            assert!(record["io_kind"].is_null() || record["io_kind"].is_string());
            assert!(record["io_os_error"].is_null() || record["io_os_error"].is_i64());
            let phases: Vec<_> = timing
                .iter()
                .filter(|event| event["request"] == record["request"])
                .map(|event| event["phase"].as_str().unwrap())
                .collect();
            assert_eq!(phases.first(), Some(&"dispatch"));
            assert_eq!(phases.last(), Some(&"closed_without_decoded_response"));
            assert!(!phases.contains(&"decoded"));
            assert_eq!(phases.contains(&"headers"), phase != "before_headers");
        }
        let timeouts = records("EVE_MODEL_TIMEOUT ");
        assert_eq!(timeouts.len(), 1);
        assert_eq!(timeouts[0]["phase"], "first_response");
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
