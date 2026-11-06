//! 组合层的实际 loopback 验收；不用生产配置、真实凭据或外部模型。
use super::*;
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, CredentialPurpose, ModelRequest, ModelResponse,
    ModelSession, ResponseMode, ToolCall, ToolExecutionContext,
};

fn codex_reply(item: Value) -> Reply {
    let mut reply = Reply::json(Value::Null);
    reply.headers = vec![("Content-Type".into(), "text/event-stream".into())];
    reply.body = [
        json!({"type":"response.output_item.done","item":item}),
        json!({"type":"response.completed","response":{"id":"synthetic-response"}}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>()
    .into_bytes();
    reply
}

fn codex_final() -> Reply {
    codex_reply(
        json!({"type":"message","id":"synthetic-final","role":"assistant",
        "phase":"final_answer","content":[{"type":"output_text","text":"done"}]}),
    )
}

fn codex_tool() -> Reply {
    let mut reply = codex_reply(json!({"type":"function_call","id":"synthetic-tool",
        "call_id":"codex-echo","name":"echo","arguments":"{\"text\":\"marker\"}"}));
    reply
        .headers
        .push(("x-codex-turn-state".into(), "synthetic-turn-token".into()));
    reply
}

fn base(server: &Server) -> &str {
    server.url.trim_end_matches("/responses")
}

fn header<'a>(captured: &'a http_support::Captured, name: &str) -> Option<&'a str> {
    captured.headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn request(role: &str) -> ModelRequest {
    ModelRequest {
        session: Some(
            ModelSession::anchored(role, None, "synthetic-persistent-job")
                .unwrap()
                .with_turn("turn-1")
                .unwrap()
                .for_request(1),
        ),
        messages: vec![ChatMessage::text(ChatRole::User, "synthetic input")],
        tools: vec![],
    }
}

fn switch(
    server: &Server,
    adapter: &str,
    protocol: &str,
) -> [(&'static str, &'static str, Value); 4] {
    [
        (MODELS_NAMESPACE, "primary_provider", json!(adapter)),
        (
            MODELS_NAMESPACE,
            "primary_model",
            json!(format!("{adapter}-model")),
        ),
        (config::OPENAI_NAMESPACE, "protocol", json!(protocol)),
        (config::OPENAI_NAMESPACE, "base_url", json!(base(server))),
    ]
}

#[tokio::test]
async fn adapter_switches_freeze_both_inflight_tool_rounds_and_restore_without_tool_replay() {
    let root = tempfile::tempdir().unwrap();
    let mut chat = Server::start(vec![
        tool_reply("chat"),
        final_reply("chat"),
        final_reply("chat"),
        final_reply("chat"),
    ])
    .await;
    let mut codex = Server::start(vec![codex_tool(), codex_final()]).await;
    let h = Harness::start_with_validator(root.path(), &chat.url, "chat", true).await;
    h.update(
        &[(config::OPENAI_NAMESPACE, "reasoning_effort", json!("omit"))],
        ApplyMode::Immediate,
    );
    let chat_gate = Arc::new(ToolGate::default());
    let first = h.submit("chat-first", chat_gate.clone());
    let first_request = chat.next().await;
    tokio::time::timeout(Duration::from_secs(5), chat_gate.entered.notified())
        .await
        .unwrap();
    h.update(&switch(&codex, "codex", "responses"), ApplyMode::Immediate);
    assert_eq!(
        summary(h.settings.as_ref(), Some("dialogue"))
            .unwrap()
            .adapter,
        "codex"
    );
    assert!(
        codex.requests.try_recv().is_err(),
        "配置和状态查询不能探测模型"
    );
    chat_gate.released.notify_one();
    let completed = h.done(&first).await;
    assert_eq!(completed.run.commit, CommitState::Completed);
    assert_eq!(completed.run.started_tools, Some(1));
    let old_follow = chat.next().await;
    assert!(old_follow.headers.starts_with("POST /v1/chat/completions "));
    assert_eq!(old_follow.body["model"], "model-a");

    let codex_gate = Arc::new(ToolGate::default());
    let second = h.submit("codex-second", codex_gate.clone());
    let codex_first = codex.next().await;
    assert_eq!(codex_first.body["model"], "codex-model");
    assert_eq!(codex_first.body["stream"], true);
    assert_eq!(codex_first.body["max_output_tokens"], 32);
    assert!(codex_first.body.get("temperature").is_none());
    assert!(codex_first.body.get("reasoning").is_none());
    assert_eq!(
        header(&first_request, "session-id"),
        header(&codex_first, "session-id")
    );
    assert_eq!(
        first_request.body["prompt_cache_key"],
        codex_first.body["prompt_cache_key"]
    );
    tokio::time::timeout(Duration::from_secs(5), codex_gate.entered.notified())
        .await
        .unwrap();
    h.update(&switch(&chat, "openai", "chat"), ApplyMode::NewRequests);
    codex_gate.released.notify_one();
    let completed = h.done(&second).await;
    assert_eq!(completed.run.commit, CommitState::Completed);
    assert_eq!(completed.run.started_tools, Some(1));
    let codex_follow = codex.next().await;
    assert_eq!(codex_follow.body["model"], "codex-model");
    assert_eq!(
        header(&codex_follow, "x-codex-turn-state"),
        Some("synthetic-turn-token")
    );
    assert_ne!(
        header(&codex_first, "x-client-request-id"),
        header(&codex_follow, "x-client-request-id")
    );
    let third = h.submit("chat-return", Arc::new(DiscardControlEvents));
    let completed = h.done(&third).await;
    assert_eq!(completed.run.commit, CommitState::Completed);
    assert_eq!(completed.run.started_tools, Some(0));
    let returned = chat.next().await;
    assert_eq!(returned.body["model"], "openai-model");
    assert!(returned.headers.starts_with("POST /v1/chat/completions "));
    assert_eq!(header(&returned, "x-codex-turn-state"), None);
    assert_eq!(
        returned.body["prompt_cache_key"],
        first_request.body["prompt_cache_key"]
    );
    h.stop().await;

    let restored = Harness::start_with_validator(root.path(), &chat.url, "chat", true).await;
    assert_eq!(
        summary(restored.settings.as_ref(), None).unwrap().adapter,
        "openai"
    );
    let fourth = restored.submit("after-restart", Arc::new(DiscardControlEvents));
    let completed = restored.done(&fourth).await;
    assert_eq!(completed.run.commit, CommitState::Completed);
    assert_eq!(completed.run.started_tools, Some(0));
    let resumed = chat.next().await;
    assert_eq!(
        user_texts(&resumed.body, "chat"),
        ["chat-first", "codex-second", "chat-return", "after-restart"]
    );
    assert_eq!(
        resumed.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        2
    );
    assert_eq!(
        header(&resumed, "session-id"),
        header(&first_request, "session-id")
    );
    assert!(chat.requests.try_recv().is_err());
    assert!(codex.requests.try_recv().is_err());
    restored.stop().await;
}

#[tokio::test]
async fn codex_chat_and_bad_endpoint_transactions_preserve_revision_and_prior_selection() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![final_reply("chat")]).await;
    let h = Harness::start_with_validator(root.path(), &server.url, "chat", true).await;
    let resolver = CoreModelResolver::new(h.settings.clone(), "synthetic-key".into());
    let selected = resolver.resolve().unwrap();
    let before = h.admin.current().unwrap();
    for changes in [
        vec![(MODELS_NAMESPACE, "primary_provider", json!("codex"))],
        vec![
            (MODELS_NAMESPACE, "primary_provider", json!("codex")),
            (config::OPENAI_NAMESPACE, "protocol", json!("responses")),
            (
                config::OPENAI_NAMESPACE,
                "base_url",
                json!("https://synthetic.example/v1/chat/completions"),
            ),
        ],
        vec![
            (MODELS_NAMESPACE, "primary_provider", json!("codex")),
            (config::OPENAI_NAMESPACE, "protocol", json!("responses")),
            (
                config::OPENAI_NAMESPACE,
                "base_url",
                json!("http://synthetic.example/v1"),
            ),
        ],
    ] {
        assert!(h.try_update(&changes, ApplyMode::Immediate).is_err());
        assert_eq!(h.admin.current().unwrap().revision, before.revision);
        assert_eq!(h.admin.current().unwrap().namespaces, before.namespaces);
        assert_eq!(
            summary(h.settings.as_ref(), None).unwrap().adapter,
            "openai"
        );
        assert!(server.requests.try_recv().is_err());
    }
    assert!(Arc::ptr_eq(
        &selected.provider,
        &resolver.resolve().unwrap().provider
    ));
    selected
        .provider
        .complete(request("dialogue"))
        .await
        .unwrap();
    assert!(
        server
            .next()
            .await
            .headers
            .starts_with("POST /v1/chat/completions ")
    );
    h.stop().await;
}

#[tokio::test]
async fn primary_and_auxiliary_mixed_adapters_keep_purpose_credentials_limits_and_failures() {
    for primary_codex in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let mut rejected = Reply::json(json!({"error":"synthetic authentication failure"}));
        rejected.status = 401;
        let mut primary = Server::start(vec![if primary_codex {
            codex_final()
        } else {
            final_reply("chat")
        }])
        .await;
        let mut auxiliary = Server::start(vec![
            if primary_codex {
                final_reply("chat")
            } else {
                codex_final()
            },
            rejected,
        ])
        .await;
        let h = Harness::start_with_validator(
            root.path(),
            &primary.url,
            if primary_codex { "responses" } else { "chat" },
            true,
        )
        .await;
        h.update(
            &[
                (
                    MODELS_NAMESPACE,
                    "primary_provider",
                    json!(if primary_codex { "codex" } else { "openai" }),
                ),
                (MODELS_NAMESPACE, "auxiliary_small_enabled", json!(true)),
                (
                    MODELS_NAMESPACE,
                    "auxiliary_small_provider",
                    json!(if primary_codex { "openai" } else { "codex" }),
                ),
                (
                    MODELS_NAMESPACE,
                    "auxiliary_small_model",
                    json!("auxiliary-model"),
                ),
                (MODELS_NAMESPACE, "auxiliary_small_timeout_ms", json!(2000)),
                (
                    MODELS_NAMESPACE,
                    "auxiliary_small_max_output_tokens",
                    json!(19),
                ),
                (
                    MODELS_NAMESPACE,
                    "auxiliary_small_credential_ref",
                    json!(crate::model_credentials::AUXILIARY),
                ),
                (
                    config::OPENAI_NAMESPACE,
                    "auxiliary_base_url",
                    json!(base(&auxiliary)),
                ),
                (
                    config::OPENAI_NAMESPACE,
                    "auxiliary_protocol",
                    json!(if primary_codex { "chat" } else { "responses" }),
                ),
                (config::OPENAI_NAMESPACE, "reasoning_effort", json!("high")),
                (
                    config::MODEL_USES_NAMESPACE,
                    "summary_role",
                    json!("auxiliary_small"),
                ),
                (
                    config::MODEL_USES_NAMESPACE,
                    "summary_max_output_tokens",
                    json!(7),
                ),
            ],
            ApplyMode::Immediate,
        );
        let bank = Arc::new(eve_model_credentials::CredentialBank::default());
        bank.bind(
            CredentialPurpose::Primary,
            crate::model_credentials::PRIMARY,
            Some("primary-key".into()),
        )
        .unwrap();
        bank.bind(
            CredentialPurpose::Auxiliary,
            crate::model_credentials::AUXILIARY,
            Some("auxiliary-key".into()),
        )
        .unwrap();
        let main = CoreModelResolver::from_credentials(h.settings.clone(), Ok(bank.clone()))
            .resolve()
            .unwrap();
        let auxiliary_resolver =
            CoreModelResolver::from_credentials(h.settings.clone(), Ok(bank.clone()))
                .with_use("summary")
                .with_timeout_cap(Duration::from_millis(900));
        let small = auxiliary_resolver.resolve().unwrap();
        assert_eq!(
            small.provider_timeout,
            Duration::from_millis(if primary_codex { 900 } else { 1900 })
        );
        main.provider.complete(request("dialogue")).await.unwrap();
        let captured = primary.next().await;
        assert_eq!(
            header(&captured, "authorization"),
            Some("Bearer primary-key")
        );
        assert_eq!(
            captured.body[if primary_codex {
                "max_output_tokens"
            } else {
                "max_tokens"
            }],
            32
        );
        small.provider.complete(request("summary")).await.unwrap();
        let captured = auxiliary.next().await;
        assert_eq!(
            header(&captured, "authorization"),
            Some("Bearer auxiliary-key")
        );
        assert_eq!(
            captured.body[if primary_codex {
                "max_tokens"
            } else {
                "max_output_tokens"
            }],
            7
        );
        assert_eq!(captured.body["model"], "auxiliary-model");
        assert!(captured.body.get("temperature").is_none());
        if !primary_codex {
            assert_eq!(captured.body["reasoning"]["effort"], "high");
        }
        assert!(small.provider.complete(request("summary")).await.is_err());
        auxiliary.next().await;
        assert!(
            primary.requests.try_recv().is_err(),
            "辅助失败不能切回主实现"
        );
        assert!(auxiliary.requests.try_recv().is_err(), "401 不能重试");
        h.update(
            &[(
                config::MODEL_USES_NAMESPACE,
                "summary_max_output_tokens",
                json!(0),
            )],
            ApplyMode::Immediate,
        );
        assert_eq!(
            summary(h.settings.as_ref(), Some("summary"))
                .unwrap()
                .max_output_tokens,
            Some(19)
        );
        let wrong_bank = Arc::new(eve_model_credentials::CredentialBank::default());
        wrong_bank
            .bind(
                CredentialPurpose::Primary,
                crate::model_credentials::PRIMARY,
                Some("wrong-purpose".into()),
            )
            .unwrap();
        assert!(
            CoreModelResolver::from_credentials(h.settings.clone(), Ok(wrong_bank))
                .with_use("summary")
                .resolve()
                .is_err()
        );
        h.stop().await;
    }
}

#[tokio::test]
async fn codex_anonymous_auxiliary_and_legacy_openai_configuration_remain_independent() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![codex_final(), final_reply("chat")]).await;
    let h = Harness::start_with_validator(root.path(), &server.url, "chat", true).await;
    h.update(
        &[
            (config::OPENAI_NAMESPACE, "model_role", json!("")),
            (config::OPENAI_NAMESPACE, "model", json!("legacy-model")),
            (MODELS_NAMESPACE, "auxiliary_small_enabled", json!(true)),
            (MODELS_NAMESPACE, "auxiliary_small_provider", json!("codex")),
            (
                MODELS_NAMESPACE,
                "auxiliary_small_model",
                json!("anonymous-small"),
            ),
            (
                MODELS_NAMESPACE,
                "auxiliary_small_credential_ref",
                json!(crate::model_credentials::ANONYMOUS),
            ),
            (
                config::OPENAI_NAMESPACE,
                "auxiliary_base_url",
                json!(base(&server)),
            ),
            (
                config::OPENAI_NAMESPACE,
                "auxiliary_protocol",
                json!("responses"),
            ),
            (
                config::MODEL_USES_NAMESPACE,
                "summary_role",
                json!("auxiliary_small"),
            ),
        ],
        ApplyMode::Immediate,
    );
    let main = CoreModelResolver::new(h.settings.clone(), "legacy-key".into())
        .resolve()
        .unwrap();
    let small = CoreModelResolver::new(h.settings.clone(), "legacy-key".into())
        .with_use("summary")
        .resolve()
        .unwrap();
    small.provider.complete(request("summary")).await.unwrap();
    let anonymous = server.next().await;
    assert_eq!(header(&anonymous, "authorization"), None);
    assert_eq!(anonymous.body["model"], "anonymous-small");
    main.provider.complete(request("dialogue")).await.unwrap();
    let old = server.next().await;
    assert_eq!(old.body["model"], "legacy-model");
    assert_eq!(header(&old, "authorization"), Some("Bearer legacy-key"));
    assert!(old.headers.starts_with("POST /v1/chat/completions "));
    assert_eq!(
        summary(h.settings.as_ref(), None).unwrap().adapter,
        "openai"
    );
    h.stop().await;
}

#[tokio::test]
async fn adapter_and_parameter_changes_update_recovery_fingerprint_without_network() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![]).await;
    let h = Harness::start_with_validator(root.path(), &server.url, "responses", true).await;
    let resolver =
        CoreModelResolver::new(h.settings.clone(), "synthetic-key".into()).with_use("task");
    let ordinary = request("task");
    let old = resolver.resolve().unwrap();
    let old_fingerprint = old
        .provider
        .request_fingerprint(&ordinary, ResponseMode::Complete)
        .unwrap()
        .unwrap();
    h.update(
        &[(MODELS_NAMESPACE, "primary_provider", json!("codex"))],
        ApplyMode::Immediate,
    );
    let codex = resolver.resolve().unwrap();
    let codex_fingerprint = codex
        .provider
        .request_fingerprint(&ordinary, ResponseMode::Complete)
        .unwrap()
        .unwrap();
    assert_ne!(old_fingerprint, codex_fingerprint);
    assert_eq!(
        codex
            .provider
            .request_fingerprint(&ordinary, ResponseMode::Stream)
            .unwrap(),
        Some(codex_fingerprint.clone())
    );
    h.update(
        &[(
            config::MODEL_USES_NAMESPACE,
            "task_max_output_tokens",
            json!(73),
        )],
        ApplyMode::Immediate,
    );
    let changed = resolver.resolve().unwrap();
    assert_ne!(
        changed
            .provider
            .request_fingerprint(&ordinary, ResponseMode::Complete)
            .unwrap(),
        Some(codex_fingerprint)
    );
    h.update(
        &[
            (MODELS_NAMESPACE, "primary_provider", json!("openai")),
            (
                config::MODEL_USES_NAMESPACE,
                "task_max_output_tokens",
                json!(0),
            ),
        ],
        ApplyMode::Immediate,
    );
    assert_eq!(
        resolver
            .resolve()
            .unwrap()
            .provider
            .request_fingerprint(&ordinary, ResponseMode::Complete)
            .unwrap(),
        Some(old_fingerprint)
    );
    assert!(server.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn image_tool_uses_selected_adapter_and_separate_catalog_with_isolated_session() {
    use eve_llm_api::Tool;
    for adapter in ["openai", "codex"] {
        let root = tempfile::tempdir().unwrap();
        let mut failed = Reply::json(json!({"error":"synthetic model failure"}));
        failed.status = 401;
        let mut server = Server::start(vec![
            if adapter == "codex" {
                codex_final()
            } else {
                final_reply("chat")
            },
            failed,
            Reply::json(json!({"data":[{"id":"next-vision"},{"id":"plain-model"}]})),
        ])
        .await;
        let protocol = if adapter == "codex" {
            "responses"
        } else {
            "chat"
        };
        let h = Harness::start_with_validator(root.path(), &server.url, protocol, true).await;
        h.update(
            &[
                (MODELS_NAMESPACE, "primary_provider", json!(adapter)),
                (config::OPENAI_NAMESPACE, "reasoning_effort", json!("omit")),
            ],
            ApplyMode::Immediate,
        );
        let image_tool = crate::qq_files::DescribeImageTool::new(
            Arc::new(crate::qq_files::AttachmentInbox::new()),
            "vision-key".into(),
        );
        assert!(image_tool.settings.set(h.settings.clone()).is_ok());
        let scope = ContextScope {
            session_id: "synthetic-image-session".into(),
            user_id: "synthetic-user".into(),
        };
        let parent = ModelSession::scoped("dialogue", &scope)
            .unwrap()
            .with_turn("parent-turn")
            .unwrap();
        let context = ToolExecutionContext::new()
            .with_scope(Some(scope))
            .with_model_session(parent.clone())
            .with_call_id("image-call-1".into());
        let image =
            || eve_llm_api::ImageInput::new(b"\x89PNG\r\n\x1a\nsynthetic".to_vec()).unwrap();
        let first = image_tool
            .describe_downloaded(&json!({"question":"look"}), image(), &context)
            .await
            .unwrap();
        assert_eq!(first["seen"], true);
        assert_eq!(first["adapter"], adapter);
        let captured = server.next().await;
        assert_eq!(
            header(&captured, "authorization"),
            Some("Bearer vision-key")
        );
        assert_ne!(captured.body["prompt_cache_key"], parent.prompt_cache_key());
        let vision_session = context.model_session_for("vision").unwrap().unwrap();
        assert_eq!(
            header(&captured, "session-id"),
            Some(vision_session.session_id())
        );
        assert!(captured.body.get("temperature").is_none());
        let image_url = if adapter == "codex" {
            assert_eq!(captured.body["stream"], true);
            captured.body["input"][0]["content"][1]["image_url"]
                .as_str()
                .unwrap()
        } else {
            captured.body["messages"][0]["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
        };
        assert!(image_url.starts_with("data:image/png;base64,"));
        let second_context = context.with_call_id("image-call-2".into());
        let second = image_tool
            .describe_downloaded(
                &json!({"model":"unsupported-model"}),
                image(),
                &second_context,
            )
            .await
            .unwrap();
        assert_eq!(second["seen"], false);
        assert_eq!(second["adapter"], adapter);
        assert_eq!(second["candidates"], json!(["next-vision", "plain-model"]));
        assert_eq!(server.next().await.body["model"], "unsupported-model");
        let catalog = server.next().await;
        assert!(catalog.headers.starts_with("GET /v1/models "));
        assert_eq!(header(&catalog, "session-id"), None);
        assert_eq!(header(&catalog, "authorization"), Some("Bearer vision-key"));
        assert!(server.requests.try_recv().is_err());
        assert!(
            image_tool
                .execute(
                    ToolCall {
                        id: "image-invalid".into(),
                        name: "describe_image".into(),
                        arguments: json!({})
                    },
                    ToolExecutionContext::new()
                )
                .await
                .is_err()
        );
        h.stop().await;
    }
}

#[tokio::test]
async fn image_capture_keeps_provider_and_credential_lease_across_configuration_switch() {
    let root = tempfile::tempdir().unwrap();
    let mut chat = Server::start(vec![final_reply("chat")]).await;
    let mut codex = Server::start(vec![codex_final()]).await;
    let h = Harness::start_with_validator(root.path(), &chat.url, "chat", true).await;
    let bank = eve_model_credentials::CredentialBank::default();
    bank.bind(
        CredentialPurpose::Primary,
        crate::model_credentials::PRIMARY,
        Some("old-key".into()),
    )
    .unwrap();
    let old = ImageModel::capture(h.settings.clone(), &bank).unwrap();
    h.update(&switch(&codex, "codex", "responses"), ApplyMode::Immediate);
    bank.replace(
        CredentialPurpose::Primary,
        crate::model_credentials::PRIMARY,
        1,
        "new-key".into(),
    )
    .unwrap();
    let new = ImageModel::capture(h.settings.clone(), &bank).unwrap();
    assert_ne!(old.cache_scope(), new.cache_scope());
    let image_request = || {
        let mut request = request("vision");
        request.messages[0]
            .images
            .push(eve_llm_api::ImageInput::new(b"GIF89asynthetic".to_vec()).unwrap());
        request
    };
    old.select("vision-old".into())
        .unwrap()
        .provider
        .complete(image_request())
        .await
        .unwrap();
    let captured = chat.next().await;
    assert_eq!(header(&captured, "authorization"), Some("Bearer old-key"));
    assert_eq!(captured.body["model"], "vision-old");
    new.select("vision-new".into())
        .unwrap()
        .provider
        .complete(image_request())
        .await
        .unwrap();
    let captured = codex.next().await;
    assert_eq!(header(&captured, "authorization"), Some("Bearer new-key"));
    assert_eq!(captured.body["model"], "vision-new");
    assert!(chat.requests.try_recv().is_err());
    assert!(codex.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn codex_summary_sse_deadline_applies_without_stream_flag_and_releases_shared_slot() {
    let root = tempfile::tempdir().unwrap();
    let mut delayed = codex_final();
    delayed.body_delay = Duration::from_secs(3);
    let mut server = Server::start(vec![delayed, codex_final()]).await;
    let h = Harness::start_with_validator(root.path(), &server.url, "responses", true).await;
    h.update(
        &[
            (MODELS_NAMESPACE, "primary_provider", json!("codex")),
            (
                MODELS_NAMESPACE,
                "primary_max_concurrent_requests",
                json!(1),
            ),
        ],
        ApplyMode::Immediate,
    );
    let resolver = CoreModelResolver::new(h.settings.clone(), "synthetic-key".into())
        .with_use("summary")
        .with_timeout_cap(Duration::from_millis(100));
    let selected = resolver.resolve().unwrap();
    assert_eq!(selected.provider_timeout, Duration::from_millis(1100));
    assert!(matches!(
        selected.provider.complete(request("summary")).await,
        Err(LlmError::ProviderTimeout)
    ));
    server.next().await;
    let status = crate::model_concurrency::status(&h.settings, ModelRole::Primary)
        .unwrap()
        .unwrap();
    assert_eq!(status.active, 0);
    assert_eq!(
        selected
            .provider
            .complete(request("summary"))
            .await
            .unwrap(),
        ModelResponse::Final {
            text: "done".into()
        }
    );
    server.next().await;
    assert!(server.requests.try_recv().is_err());
    h.stop().await;
}
