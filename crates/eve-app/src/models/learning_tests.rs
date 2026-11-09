use super::*;

#[tokio::test]
async fn learning_deadlines_are_independent_live_settings_and_preserve_model_identity() {
    use eve_llm_api::{ChatMessage, ChatRole, ContextScope, ModelRequest, ModelSession};
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![final_reply("chat"), final_reply("chat")]).await;
    let h = Harness::start_with_validator(root.path(), &server.url, "chat", true).await;
    h.update(
        &[
            (config::OPENAI_NAMESPACE, "reasoning_effort", json!("high")),
            (
                config::MODEL_USES_NAMESPACE,
                "conversation_learning_max_output_tokens",
                json!(8192),
            ),
        ],
        ApplyMode::Immediate,
    );
    let ordinary = CoreModelResolver::new(h.settings.clone(), "learning-test-key".into())
        .with_use("conversation_learning");
    let maintenance = CoreModelResolver::new(h.settings.clone(), "learning-test-key".into())
        .with_use("conversation_learning")
        .with_conversation_maintenance();
    let first = ordinary.resolve().unwrap();
    let historical = maintenance.resolve().unwrap();
    assert_eq!(first.provider_timeout, Duration::from_secs(180));
    assert_eq!(historical.provider_timeout, Duration::from_secs(300));
    assert!(Arc::ptr_eq(
        &first.provider,
        &ordinary.resolve().unwrap().provider
    ));
    assert_eq!(
        CoreModelResolver::new(h.settings.clone(), "key".into())
            .with_use("task")
            .resolve()
            .unwrap()
            .provider_timeout,
        Duration::from_millis(2500),
        "其他用途仍保留所选模型期限"
    );
    let scope = ContextScope {
        session_id: "original-conversation".into(),
        user_id: "original-owner".into(),
    };
    let session = ModelSession::scoped("conversation-learning", &scope).unwrap();
    for (selection, turn) in [
        (&first, "ordinary-1"),
        (&historical, "conversation-revisit-v2-1-1"),
    ] {
        selection
            .provider
            .complete(ModelRequest {
                session: Some(session.clone().with_turn(turn).unwrap().for_request(1)),
                messages: vec![ChatMessage::text(ChatRole::User, "保留完整输入及旧经验")],
                tools: vec![],
            })
            .await
            .unwrap();
        let request = server.next().await;
        assert_eq!(request.body["model"], "model-a");
        assert_eq!(request.body["reasoning_effort"], "high");
        assert_eq!(request.body["max_tokens"], 8192);
        assert!(request.body.get("temperature").is_none());
        assert!(
            request
                .headers
                .to_lowercase()
                .contains(&format!("session-id: {}\r\n", session.session_id()))
        );
    }
    h.update(
        &[(
            config::MODEL_USES_NAMESPACE,
            "conversation_learning_timeout_ms",
            json!(31_000),
        )],
        ApplyMode::Immediate,
    );
    let shorter = ordinary.resolve().unwrap();
    assert_eq!(shorter.provider_timeout, Duration::from_secs(31));
    assert!(!Arc::ptr_eq(&first.provider, &shorter.provider));
    assert_eq!(
        maintenance.resolve().unwrap().provider_timeout,
        Duration::from_secs(300)
    );
    h.update(
        &[(
            config::MODEL_USES_NAMESPACE,
            "conversation_maintenance_timeout_ms",
            json!(42_000),
        )],
        ApplyMode::Immediate,
    );
    assert_eq!(
        maintenance.resolve().unwrap().provider_timeout,
        Duration::from_secs(42)
    );
    assert_eq!(
        ordinary.resolve().unwrap().provider_timeout,
        Duration::from_secs(31)
    );
    assert!(
        server.requests.try_recv().is_err(),
        "解析配置不产生额外模型请求"
    );
    h.stop().await;
}

#[tokio::test]
async fn learning_timeout_fields_reject_out_of_range_values() {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(vec![]).await;
    let h = Harness::start(root.path(), &server.url, "chat").await;
    for field in [
        "conversation_learning_timeout_ms",
        "conversation_maintenance_timeout_ms",
        "conversation_learning_stream_idle_timeout_ms",
    ] {
        for invalid in [0, 999, 600_001] {
            assert!(
                h.try_update(
                    &[(config::MODEL_USES_NAMESPACE, field, json!(invalid))],
                    ApplyMode::Immediate
                )
                .is_err()
            );
        }
        for valid in [1000, 600_000] {
            h.update(
                &[(config::MODEL_USES_NAMESPACE, field, json!(valid))],
                ApplyMode::Immediate,
            );
        }
    }
    h.stop().await;
}

#[test]
fn learning_stream_watchdog_ignores_dialogue_limits_and_preserves_completed_results() {
    const CHILD: &str = "EVE_TEST_LEARNING_STREAM_TIMEOUTS";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "models::tests::learning::learning_stream_watchdog_ignores_dialogue_limits_and_preserves_completed_results", "--nocapture"])
            .env(CHILD, "1")
            .env("EVE_OPENAI_STREAM_COMPLETIONS", "true")
            .env("EVE_MODEL_FIRST_RESPONSE_TIMEOUT_MS", "100")
            .env("EVE_MODEL_STREAM_IDLE_TIMEOUT_MS", "100")
            .env("EVE_MODEL_STREAM_TOTAL_TIMEOUT_MS", "100")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use eve_llm_api::{ChatMessage, ChatRole, ModelRequest, ModelResponse};
        for scenario in ["idle", "total", "complete"] {
            let root = tempfile::tempdir().unwrap();
            let mut reply = Reply::json(json!({}));
            reply.headers = vec![("Content-Type".into(), "text/event-stream".into())];
            reply.chunked = true;
            let ping = b": ping\n\n".to_vec();
            let terminal = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"complete\"},\"finish_reason\":\"stop\"}]}\n\n".to_vec();
            reply.delayed_chunks = match scenario {
                "idle" => vec![(Duration::from_millis(20), ping), (Duration::from_secs(5), terminal)],
                "total" => (0..8).map(|_| (Duration::from_millis(400), ping.clone())).collect(),
                _ => vec![(Duration::from_millis(1200), terminal), (Duration::from_secs(5), b"data: [DONE]\n\n".to_vec())],
            };
            let mut server = Server::start(vec![reply]).await;
            let h = Harness::start(root.path(), &server.url, "chat").await;
            let resolver = CoreModelResolver::new(h.settings.clone(), "key".into()).with_use("conversation_learning");
            let defaults = resolver.capture().unwrap();
            assert_eq!(defaults.stream_timeouts, Some(StreamTimeoutPolicy {first_response: Duration::from_secs(180), idle: Duration::from_secs(90), total: Duration::from_secs(180)}));
            let maintenance = CoreModelResolver::new(h.settings.clone(), "key".into()).with_use("conversation_learning").with_conversation_maintenance();
            assert_eq!(maintenance.capture().unwrap().stream_timeouts.unwrap().first_response, Duration::from_secs(300));
            assert_eq!(maintenance.resolve().unwrap().provider_timeout, Duration::from_secs(301));
            h.update(&[
                (config::MODEL_USES_NAMESPACE, "conversation_learning_timeout_ms", json!(2000)),
                (config::MODEL_USES_NAMESPACE, "conversation_learning_stream_idle_timeout_ms", json!(1000)),
            ], ApplyMode::Immediate);
            let selected = resolver.resolve().unwrap();
            assert_eq!(selected.provider_timeout, Duration::from_secs(3));
            let started = tokio::time::Instant::now();
            let result = tokio::time::timeout(selected.provider_timeout, selected.provider.complete(ModelRequest {
                session: None,
                messages: vec![ChatMessage::text(ChatRole::User, "完整请求")],
                tools: vec![],
            })).await.expect("适配器应先结束，不由外层抢先取消");
            if scenario == "complete" {
                assert_eq!(result.unwrap(), ModelResponse::Final {text: "complete".into()});
            } else {
                assert!(matches!(result, Err(LlmError::ProviderTimeout)));
                let elapsed = started.elapsed();
                if scenario == "idle" {
                    assert!(elapsed >= Duration::from_millis(900) && elapsed < Duration::from_millis(1900));
                } else {
                    assert!(elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_millis(2900));
                }
            }
            assert_eq!(server.next().await.body["stream"], true);
            assert!(server.requests.try_recv().is_err(), "超时和统计尾流不自动重放请求");
            h.stop().await;
        }
    });
}
