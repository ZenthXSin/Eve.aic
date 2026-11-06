#[allow(dead_code)]
#[path = "support/mod.rs"]
mod http_support;
#[path = "support/sse.rs"]
mod sse;

use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmFuture, LlmProvider, ModelRequest, ModelResponse,
    ModelSession, ModelTextSink, ToolCall, ToolDefinition, ToolResult,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider};
use http_support::{Captured, Reply, Server, final_response};
use serde_json::json;
use std::{collections::HashSet, process::Command, time::Duration};

fn scope() -> ContextScope {
    ContextScope {
        session_id: "private-conversation-fixture".into(),
        user_id: "private-user-fixture".into(),
    }
}

fn binding(role: &str) -> ModelSession {
    ModelSession::scoped(role, &scope())
        .unwrap()
        .with_turn("persisted-turn-31")
        .unwrap()
}

fn request(session: ModelSession, text: &str) -> ModelRequest {
    ModelRequest {
        session: Some(session),
        messages: vec![ChatMessage::text(ChatRole::User, text)],
        tools: vec![],
    }
}

fn provider(url: &str, protocol: OpenAiProtocol) -> OpenAiProvider {
    OpenAiProvider::new(
        OpenAiConfig {
            responses_url: url.into(),
            protocol,
            reasoning_effort: Some("high".into()),
            max_output_tokens: Some(8192),
            request_timeout: Duration::from_secs(15),
            ..OpenAiConfig::new("fixture-model")
        },
        "fixture-key",
    )
    .unwrap()
}

fn reply(protocol: OpenAiProtocol, text: &str) -> Reply {
    Reply::json(match protocol {
        OpenAiProtocol::Responses => final_response(text),
        OpenAiProtocol::ChatCompletions => json!({
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "content": text,
            }}],
        }),
    })
}

fn streamed_reply(protocol: OpenAiProtocol) -> Reply {
    if protocol == OpenAiProtocol::Responses {
        return sse::reply(&sse::events(vec![sse::text("{}")]));
    }
    let mut reply = Reply::json(json!({}));
    reply.headers = vec![("content-type".into(), "text/event-stream".into())];
    reply.chunked = true;
    reply.body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id": "fixture-chat", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": "{}"}, "finish_reason": "stop"}]}),
    )
    .into_bytes();
    reply
}

fn header<'a>(captured: &'a Captured, name: &str) -> Option<&'a str> {
    captured.headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

#[tokio::test]
async fn image_and_embedding_bypasses_transmit_bound_headers_without_raw_identity() {
    use eve_semantic_api::{EmbeddingProfile, EmbeddingProvider};
    let mut server = Server::start(vec![
        reply(OpenAiProtocol::Responses, "图片说明"),
        Reply::json(json!({"data": [{"index": 0, "embedding": [1.0, 0.5]}]})),
    ])
    .await;
    let vision = binding("vision");
    provider(&server.url, OpenAiProtocol::Responses)
        .describe_image_with_session("识图", "image/png", b"fixture-image", Some(&vision))
        .await
        .unwrap();
    assert_binding(&server.next().await, &vision);
    let embedding = binding("embedding-query");
    eve_llm_openai::OpenAiEmbeddings::new(
        &server.url,
        EmbeddingProfile {
            model: "embedding-fixture".into(),
            dimensions: 2,
        },
        "fixture-key",
        Duration::from_secs(5),
    )
    .unwrap()
    .embed_with_model_session(&["查询".into()], Some(&embedding))
    .await
    .unwrap();
    let captured = server.next().await;
    for (name, value) in [
        ("session-id", embedding.session_id()),
        ("thread-id", embedding.thread_id()),
        ("x-client-request-id", embedding.client_request_id()),
    ] {
        assert_eq!(header(&captured, name), Some(value));
    }
    assert!(captured.body.get("temperature").is_none());
    assert!(
        captured.body.get("prompt_cache_key").is_none(),
        "不向 embeddings schema 注入对话字段"
    );
    assert!(captured.headers.starts_with("POST /v1/embeddings HTTP/1.1"));
}

fn assert_binding(captured: &Captured, session: &ModelSession) {
    assert_eq!(header(captured, "session-id"), Some(session.session_id()));
    assert_eq!(header(captured, "thread-id"), Some(session.thread_id()));
    assert_eq!(
        header(captured, "x-client-request-id"),
        Some(session.client_request_id()),
    );
    assert_eq!(
        captured.body["prompt_cache_key"],
        session.prompt_cache_key()
    );
    for private in [&scope().session_id, &scope().user_id] {
        assert!(!captured.headers.contains(private));
        assert!(!captured.body.to_string().contains(private));
    }
    assert!(captured.body.get("session").is_none());
    assert!(captured.body.get("temperature").is_none());
}

#[test]
fn request_fingerprint_tracks_the_wire_body_and_configuration_but_not_request_number() {
    use eve_llm_api::ResponseMode;
    let body = request(binding("main").for_request(1), "正文");
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let config = OpenAiConfig {
            responses_url: if protocol == OpenAiProtocol::Responses {
                "http://127.0.0.1:9/v1/responses".into()
            } else {
                "http://127.0.0.1:9/v1/chat/completions".into()
            },
            protocol,
            reasoning_effort: Some("high".into()),
            max_output_tokens: Some(8192),
            ..OpenAiConfig::new("fixture-model")
        };
        let p = OpenAiProvider::anonymous(config.clone()).unwrap();
        let expected = p
            .request_fingerprint(&body, ResponseMode::Complete)
            .unwrap();
        assert!(expected.is_some());
        assert_eq!(
            expected,
            p.request_fingerprint(
                &request(binding("main").for_request(2), "正文"),
                ResponseMode::Complete,
            )
            .unwrap()
        );
        assert_ne!(
            expected,
            p.request_fingerprint(
                &request(binding("main").for_request(1), "新正文"),
                ResponseMode::Complete,
            )
            .unwrap()
        );
        assert_ne!(
            expected,
            p.request_fingerprint(&body, ResponseMode::Stream).unwrap()
        );
        for changed in [
            OpenAiConfig {
                reasoning_effort: Some("medium".into()),
                ..config.clone()
            },
            OpenAiConfig {
                max_output_tokens: Some(4096),
                ..config.clone()
            },
            OpenAiConfig {
                model: "other-model".into(),
                ..config.clone()
            },
            OpenAiConfig {
                responses_url: config.responses_url.replace(":9/", ":10/"),
                ..config.clone()
            },
        ] {
            let p = OpenAiProvider::anonymous(changed).unwrap();
            assert_ne!(
                expected,
                p.request_fingerprint(&body, ResponseMode::Complete)
                    .unwrap()
            );
        }
    }
}

struct DiscardText;
impl ModelTextSink for DiscardText {
    fn text_delta(&self, _: String) -> LlmFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn every_http_mode_transmits_explicit_binding_and_keeps_high_reasoning() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let mut server = Server::start(vec![
            reply(protocol, "{}"),
            reply(protocol, "{}"),
            streamed_reply(protocol),
            streamed_reply(protocol),
            streamed_reply(protocol),
        ])
        .await;
        let p = provider(&server.url, protocol);
        let session = binding("main");
        p.complete(request(session.for_request(1), "相同正文"))
            .await
            .unwrap();
        p.complete_json(request(session.for_request(2), "相同正文"))
            .await
            .unwrap();
        p.stream(request(session.for_request(3), "相同正文"), &DiscardText)
            .await
            .unwrap();
        let p = p.with_streamed_completions();
        p.complete(request(session.for_request(4), "相同正文"))
            .await
            .unwrap();
        p.complete_json(request(session.for_request(5), "相同正文"))
            .await
            .unwrap();
        for ordinal in 1..=5 {
            let captured = server.next().await;
            assert_binding(&captured, &session.for_request(ordinal));
            let effort = if protocol == OpenAiProtocol::Responses {
                &captured.body["reasoning"]["effort"]
            } else {
                &captured.body["reasoning_effort"]
            };
            assert_eq!(effort, "high");
            assert_eq!(captured.body["stream"], ordinal >= 3);
        }
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn shared_provider_keeps_concurrent_main_review_task_exploration_and_subagents_separate() {
    let parent = binding("main");
    let sessions = vec![
        parent.clone(),
        binding("review"),
        ModelSession::anchored("task", Some(&scope()), "persisted-task-a")
            .unwrap()
            .with_turn("persisted-task-a")
            .unwrap(),
        ModelSession::anchored("task", Some(&scope()), "persisted-task-b")
            .unwrap()
            .with_turn("persisted-task-b")
            .unwrap(),
        ModelSession::anchored("exploration", Some(&scope()), "persisted-exploration")
            .unwrap()
            .with_turn("persisted-exploration")
            .unwrap(),
        parent.fork("subagent", Some("trusted-child-a")).unwrap(),
        parent.fork("subagent", Some("trusted-child-b")).unwrap(),
        ModelSession::scoped(
            "main",
            &ContextScope {
                user_id: "other-private-user".into(),
                ..scope()
            },
        )
        .unwrap(),
    ];
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let mut server =
            Server::start(sessions.iter().map(|_| reply(protocol, "完成")).collect()).await;
        let p = std::sync::Arc::new(provider(&server.url, protocol));
        let mut running = tokio::task::JoinSet::new();
        for session in &sessions {
            let p = p.clone();
            let request = request(session.for_request(1), "完全相同的正文不得决定身份");
            running.spawn(async move { p.complete(request).await.unwrap() });
        }
        while let Some(result) = running.join_next().await {
            result.unwrap();
        }
        let mut observed = HashSet::new();
        for _ in &sessions {
            let captured = server.next().await;
            let id = header(&captured, "session-id").unwrap();
            let session = sessions
                .iter()
                .find(|session| session.session_id() == id)
                .unwrap();
            assert_binding(&captured, &session.for_request(1));
            assert!(observed.insert(id.to_owned()));
        }
        assert_eq!(observed.len(), sessions.len());
    }
}

#[tokio::test]
async fn transient_retries_reuse_the_same_headers_and_cache_key() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let failures = [429, 503].map(|status| {
            let mut reply = Reply::json(json!({"error": "temporary fixture failure"}));
            reply.status = status;
            reply
        });
        let mut server = Server::start(
            failures
                .into_iter()
                .chain([reply(protocol, "恢复")])
                .collect(),
        )
        .await;
        let session = binding("review").for_request(8);
        provider(&server.url, protocol)
            .with_transient_retries()
            .complete(request(session.clone(), "保留原请求"))
            .await
            .unwrap();
        let first = server.next().await;
        assert_binding(&first, &session);
        for _ in 0..2 {
            let next = server.next().await;
            assert_binding(&next, &session);
            assert_eq!(first.body, next.body);
            assert_eq!(first.headers, next.headers);
        }
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn tool_round_trip_keeps_session_while_the_request_id_advances() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let tool = ToolCall {
            id: "tool-one".into(),
            name: "echo".into(),
            arguments: json!({}),
        };
        let first_reply = Reply::json(if protocol == OpenAiProtocol::Responses {
            json!({"status": "completed", "output": [sse::call("tool-one", "echo", "{}")]})
        } else {
            json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null, "tool_calls": [{"id": "tool-one",
                    "type": "function", "function": {"name": "echo", "arguments": "{}"}}],
            }}]})
        });
        let mut server = Server::start(vec![first_reply, reply(protocol, "完成")]).await;
        let p = provider(&server.url, protocol);
        let session = binding("main");
        let mut first = request(session.for_request(1), "执行工具");
        first.tools.push(ToolDefinition {
            name: "echo".into(),
            description: "fixture".into(),
            argument_schema: json!({"type": "object"}),
            required_permissions: vec![],
            concurrency: None,
        });
        assert_eq!(
            p.complete(first.clone()).await.unwrap(),
            ModelResponse::ToolCalls {
                calls: vec![tool.clone()]
            }
        );
        let mut next = first;
        next.session = Some(session.for_request(2));
        next.messages
            .push(ChatMessage::assistant_tool_calls(vec![tool]).unwrap());
        next.messages.push(
            ChatMessage::tool_results(vec![
                ToolResult::success("tool-one", json!({"ok": true})).unwrap(),
            ])
            .unwrap(),
        );
        p.complete(next).await.unwrap();
        let a = server.next().await;
        let b = server.next().await;
        assert_binding(&a, &session.for_request(1));
        assert_binding(&b, &session.for_request(2));
        assert_eq!(a.body["prompt_cache_key"], b.body["prompt_cache_key"]);
        assert_ne!(
            header(&a, "x-client-request-id"),
            header(&b, "x-client-request-id")
        );
        assert_ne!(a.body, b.body);
    }
}

#[tokio::test]
async fn legacy_unbound_requests_do_not_invent_global_or_prompt_derived_ids() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let mut server = Server::start(vec![reply(protocol, "完成")]).await;
        let mut request = request(binding("main"), "session-id: forged identity");
        request.session = None;
        provider(&server.url, protocol)
            .complete(request)
            .await
            .unwrap();
        let captured = server.next().await;
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            assert!(header(&captured, name).is_none());
        }
        assert!(captured.body.get("prompt_cache_key").is_none());
    }
}

// 子进程走同一个集成测试二进制，读取的是父进程已序列化的宿主元数据，
// 并重建一次同样的可信锚点；不能依赖 Provider 实例或进程内静态状态。
#[test]
fn restored_session_http_child() {
    let Some(url) = std::env::var_os("EVE_SESSION_FIXTURE_URL") else {
        return;
    };
    let serialized = std::env::var("EVE_SESSION_FIXTURE_RECORD").unwrap();
    let restored: ModelSession = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored, binding("main").for_request(4));
    let protocol = if std::env::var("EVE_SESSION_FIXTURE_PROTOCOL").unwrap() == "chat" {
        OpenAiProtocol::ChatCompletions
    } else {
        OpenAiProtocol::Responses
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        provider(url.to_str().unwrap(), protocol)
            .complete(request(restored, "跨进程同一请求"))
            .await
            .unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serialized_binding_survives_a_real_process_restart_and_reaches_http_unchanged() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let mut server =
            Server::start(vec![reply(protocol, "完成"), reply(protocol, "恢复")]).await;
        let session = binding("main").for_request(4);
        provider(&server.url, protocol)
            .complete(request(session.clone(), "跨进程同一请求"))
            .await
            .unwrap();
        let before = server.next().await;
        let url = server.url.clone();
        let serialized = serde_json::to_string(&session).unwrap();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "restored_session_http_child", "--nocapture"])
                .env_clear()
                .env("EVE_SESSION_FIXTURE_URL", url)
                .env("EVE_SESSION_FIXTURE_RECORD", serialized)
                .env(
                    "EVE_SESSION_FIXTURE_PROTOCOL",
                    if protocol == OpenAiProtocol::ChatCompletions {
                        "chat"
                    } else {
                        "responses"
                    },
                )
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "子进程验收失败: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let after = server.next().await;
        assert_binding(&before, &session);
        assert_binding(&after, &session);
        assert_eq!(before.body, after.body);
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            assert_eq!(header(&before, name), header(&after, name));
        }
    }
}
