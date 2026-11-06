//! 原 ResponsesClient 的本地 HTTP/SSE 验收；不接触真实模型或 QQ。
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmError, LlmFuture, LlmProvider, ModelRequest,
    ModelResponse, ModelSession, ModelTextSink, ModelUsage, ModelUsageObserver,
    ProviderFailureKind, ToolCall, ToolDefinition, ToolFailureCode, ToolResult,
};
use eve_llm_codex::{CodexConfig, CodexProvider, StreamTimeoutPolicy};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

#[derive(Clone)]
struct Captured {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
    split: usize,
    delay_headers: Duration,
    delay_body: Duration,
    chunked: bool,
}

impl Reply {
    fn sse(events: Vec<Value>) -> Self {
        Self {
            status: 200,
            body: events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
            headers: Vec::new(),
            split: 7,
            delay_headers: Duration::ZERO,
            delay_body: Duration::ZERO,
            chunked: false,
        }
    }

    fn text(text: &str) -> Self {
        Self::sse(completed_text(text))
    }

    fn error(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.into(),
            ..Self::sse(Vec::new())
        }
    }

    fn turn_token(mut self, value: &str) -> Self {
        self.headers
            .push(("x-codex-turn-state".into(), value.into()));
        self
    }
}

struct Fixture {
    base: String,
    captured: Arc<Mutex<Vec<Captured>>>,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let saved = captured.clone();
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                saved.lock().unwrap().push(request);
                tokio::time::sleep(reply.delay_headers).await;
                let length = if reply.chunked {
                    "transfer-encoding: chunked\r\n".to_owned()
                } else {
                    format!("content-length: {}\r\n", reply.body.len())
                };
                let custom: String = reply
                    .headers
                    .iter()
                    .map(|(key, value)| format!("{key}: {value}\r\n"))
                    .collect();
                let header = format!(
                    "HTTP/1.1 {} test\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{length}{custom}\r\n",
                    reply.status
                );
                if socket.write_all(header.as_bytes()).await.is_err() {
                    continue;
                }
                tokio::time::sleep(reply.delay_body).await;
                for chunk in reply.body.as_bytes().chunks(reply.split.max(1)) {
                    if reply.chunked
                        && socket
                            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                            .await
                            .is_err()
                    {
                        break;
                    }
                    if socket.write_all(chunk).await.is_err() {
                        break;
                    }
                    if reply.chunked && socket.write_all(b"\r\n").await.is_err() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                if reply.chunked {
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                }
                let _ = socket.shutdown().await;
            }
        });
        Self {
            base,
            captured,
            task,
        }
    }

    fn config(&self) -> CodexConfig {
        let mut config = CodexConfig::new("fixture-model")
            .with_base_url(&self.base)
            .unwrap();
        config.request_timeout = Duration::from_secs(4);
        config
    }

    fn provider(&self) -> CodexProvider {
        CodexProvider::anonymous(self.config()).unwrap()
    }

    fn captures(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> Captured {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut buf = [0u8; 4096];
        let count = socket.read(&mut buf).await.unwrap();
        assert_ne!(count, 0, "request headers ended early");
        bytes.extend_from_slice(&buf[..count]);
        assert!(bytes.len() <= 2 * 1024 * 1024);
    };
    let headers_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let path = headers_text
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let headers: BTreeMap<_, _> = headers_text
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers["content-length"].parse::<usize>().unwrap();
    while bytes.len() < header_end + length {
        let mut buf = [0u8; 4096];
        let count = socket.read(&mut buf).await.unwrap();
        assert_ne!(count, 0, "request body ended early");
        bytes.extend_from_slice(&buf[..count]);
    }
    Captured {
        path,
        headers,
        body: serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    }
}

fn scope() -> ContextScope {
    ContextScope {
        session_id: "private-chat".into(),
        user_id: "private-user".into(),
    }
}
fn session() -> ModelSession {
    ModelSession::scoped("main", &scope())
        .unwrap()
        .with_turn("turn-1")
        .unwrap()
        .for_request(1)
}
fn request(session: ModelSession) -> ModelRequest {
    ModelRequest {
        session: Some(session),
        messages: vec![ChatMessage::text(ChatRole::User, "继续查看")],
        tools: vec![definition("echo")],
    }
}
fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "本地合成工具".into(),
        argument_schema: json!({"type":"object"}),
        required_permissions: Vec::new(),
        concurrency: None,
    }
}
fn message(id: &str, text: &str, phase: &str) -> Value {
    json!({"type":"message", "id":id, "role":"assistant", "phase":phase,
        "content":[{"type":"output_text", "text":text}]})
}
fn function(id: &str, call_id: &str, args: &str) -> Value {
    json!({"type":"function_call", "id":id, "call_id":call_id, "name":"echo", "arguments":args})
}
fn done(item: Value) -> Value {
    json!({"type":"response.output_item.done", "item":item})
}
fn completed() -> Value {
    json!({"type":"response.completed", "response":{"id":"response-1"}})
}
fn completed_text(text: &str) -> Vec<Value> {
    vec![
        done(message("message-1", text, "final_answer")),
        completed(),
    ]
}

#[derive(Default)]
struct Usage(Mutex<Vec<ModelUsage>>);
impl ModelUsageObserver for Usage {
    fn reported(&self, _: &str, usage: ModelUsage) {
        self.0.lock().unwrap().push(usage);
    }
}
#[derive(Default)]
struct Sink(Mutex<String>);
impl ModelTextSink for Sink {
    fn text_delta(&self, text: String) -> LlmFuture<'_, ()> {
        self.0.lock().unwrap().push_str(&text);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn original_stream_preserves_unicode_and_reports_actual_cache_usage() {
    let events = vec![
        json!({"type":"response.created", "response":{"id":"response-1"}}),
        json!({"type":"response.output_item.added", "item":message("message-1", "", "final_answer")}),
        json!({"type":"response.output_text.delta", "delta":"第一段"}),
        json!({"type":"response.output_text.delta", "delta":"\n第二段"}),
        done(message("message-1", "第一段\n第二段", "final_answer")),
        json!({"type":"response.completed", "response":{"id":"response-1", "usage":{
            "input_tokens":1200, "input_tokens_details":{"cached_tokens":1024},
            "output_tokens":12, "total_tokens":1212
        }}}),
    ];
    let fixture = Fixture::start(vec![Reply::sse(events)]).await;
    let usage = Arc::new(Usage::default());
    let provider = fixture
        .provider()
        .with_usage_observer(usage.clone(), "main");
    let sink = Sink::default();
    let result = provider.stream(request(session()), &sink).await.unwrap();
    assert_eq!(
        result,
        ModelResponse::Final {
            text: "第一段\n第二段".into()
        }
    );
    assert_eq!(*sink.0.lock().unwrap(), "第一段\n第二段");
    assert_eq!(
        *usage.0.lock().unwrap(),
        vec![ModelUsage {
            input_tokens: Some(1200),
            cached_input_tokens: Some(1024),
            output_tokens: Some(12)
        }]
    );
    let captured = fixture.captures();
    assert_eq!(captured[0].path, "/v1/responses");
    assert_eq!(captured[0].headers["accept"], "text/event-stream");
    assert!(!captured[0].headers.contains_key("authorization"));
    assert_eq!(captured[0].body["stream"], true);
    assert_eq!(captured[0].body["store"], false);
    assert!(captured[0].body.get("temperature").is_none());
    assert!(captured[0].body.get("reasoning").is_none());
    assert!(!captured[0].body.to_string().contains("private-user"));
}

#[tokio::test]
async fn missing_cache_counter_stays_unknown_while_explicit_zero_is_zero() {
    let mut first = completed();
    first["response"]["usage"] = json!({"input_tokens":100,"output_tokens":5,"total_tokens":105});
    let mut second = first.clone();
    second["response"]["usage"]["input_tokens_details"] = json!({"cached_tokens":0});
    let fixture = Fixture::start(vec![
        Reply::sse(vec![done(message("m", "完成", "final_answer")), first]),
        Reply::sse(vec![done(message("m", "完成", "final_answer")), second]),
        Reply::text("完成"),
    ])
    .await;
    let usage = Arc::new(Usage::default());
    let provider = fixture
        .provider()
        .with_usage_observer(usage.clone(), "main");
    for ordinal in 1..=3 {
        provider
            .complete(request(session().for_request(ordinal)))
            .await
            .unwrap();
    }
    let usage = usage.0.lock().unwrap();
    assert_eq!(usage[0].cached_input_tokens, None);
    assert_eq!(usage[1].cached_input_tokens, Some(0));
    assert_eq!(usage[2], ModelUsage::default());
}

#[tokio::test]
async fn complete_function_batch_returns_only_after_done_and_keeps_original_order() {
    let fixture = Fixture::start(vec![Reply::sse(vec![
        done(json!({"type":"reasoning","id":"reasoning-1","summary":[],"encrypted_content":"opaque"})),
        json!({"type":"response.output_text.delta","delta":"查看资料"}),
        done(message("commentary", "查看资料", "commentary")),
        json!({"type":"response.function_call_arguments.delta","item_id":"function-b","delta":"{\"part\":"}),
        done(function("function-b", "call-b", "{\"part\":2}")),
        done(function("function-a", "call-a", "{\"part\":1}")),
        completed(),
    ])]).await;
    let sink = Sink::default();
    let result = fixture
        .provider()
        .stream(request(session()), &sink)
        .await
        .unwrap();
    assert!(sink.0.lock().unwrap().is_empty());
    assert_eq!(
        result,
        ModelResponse::ToolCalls {
            calls: vec![
                ToolCall {
                    id: "call-b".into(),
                    name: "echo".into(),
                    arguments: json!({"part":2})
                },
                ToolCall {
                    id: "call-a".into(),
                    name: "echo".into(),
                    arguments: json!({"part":1})
                },
            ]
        }
    );
}

#[tokio::test]
async fn failed_partial_stream_is_not_retried_or_reported_as_success() {
    let fixture = Fixture::start(vec![Reply::sse(vec![
        json!({"type":"response.output_text.delta","delta":"只是半句"}),
        done(function("function-a", "call-a", "{}")),
    ])])
    .await;
    let usage = Arc::new(Usage::default());
    let provider = fixture
        .provider()
        .with_transient_retries()
        .with_usage_observer(usage.clone(), "main");
    let error = provider.complete(request(session())).await.unwrap_err();
    assert_eq!(
        error.provider_failure().unwrap().kind,
        ProviderFailureKind::Incomplete
    );
    assert_eq!(fixture.captures().len(), 1);
    assert!(usage.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn retries_reuse_request_body_and_session_headers_without_replaying_stream() {
    let fixture = Fixture::start(vec![
        Reply::error(503, "temporary-provider-body"),
        Reply::text("恢复"),
    ])
    .await;
    let provider = fixture.provider().with_transient_retries();
    assert_eq!(
        provider.complete(request(session())).await.unwrap(),
        ModelResponse::Final {
            text: "恢复".into()
        }
    );
    let requests = fixture.captures();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
    for header in ["session-id", "thread-id", "x-client-request-id"] {
        assert_eq!(requests[0].headers[header], requests[1].headers[header]);
    }
}

#[tokio::test]
async fn tasks_review_turns_and_restored_provider_have_separate_stable_sessions() {
    let fixture = Fixture::start(vec![
        Reply::sse(vec![done(function("f", "c", "{}")), completed()]).turn_token("turn-token-one"),
        Reply::text("工具返回"),
        Reply::text("下轮"),
        Reply::text("审查"),
        Reply::text("任务甲"),
        Reply::text("任务乙"),
        Reply::text("恢复后"),
    ])
    .await;
    let provider = fixture.provider();
    let original = session();
    provider.complete(request(original.clone())).await.unwrap();
    let mut roundtrip = request(original.for_request(2));
    roundtrip.messages.push(
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: json!({}),
        }])
        .unwrap(),
    );
    roundtrip.messages.push(
        ChatMessage::tool_results(vec![ToolResult::success("c", json!({"ok":true})).unwrap()])
            .unwrap(),
    );
    provider.complete(roundtrip).await.unwrap();
    provider
        .complete(request(original.clone().with_turn("turn-2").unwrap()))
        .await
        .unwrap();
    provider
        .complete(request(original.fork("review", None).unwrap()))
        .await
        .unwrap();
    provider
        .complete(request(
            ModelSession::anchored("task", Some(&scope()), "task-a")
                .unwrap()
                .with_turn("initial")
                .unwrap(),
        ))
        .await
        .unwrap();
    provider
        .complete(request(
            ModelSession::anchored("task", Some(&scope()), "task-b")
                .unwrap()
                .with_turn("initial")
                .unwrap(),
        ))
        .await
        .unwrap();
    let restored: ModelSession =
        serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    drop(provider);
    fixture
        .provider()
        .complete(request(restored))
        .await
        .unwrap();
    let requests = fixture.captures();
    assert_eq!(requests[1].headers["x-codex-turn-state"], "turn-token-one");
    assert_ne!(
        requests[0].headers["x-client-request-id"],
        requests[1].headers["x-client-request-id"]
    );
    for index in [0, 2, 3, 4, 5, 6] {
        assert!(!requests[index].headers.contains_key("x-codex-turn-state"));
    }
    for index in [1, 2, 6] {
        assert_eq!(
            requests[0].headers["session-id"],
            requests[index].headers["session-id"]
        );
        assert_eq!(
            requests[0].headers["thread-id"],
            requests[index].headers["thread-id"]
        );
        assert_eq!(
            requests[0].body["prompt_cache_key"],
            requests[index].body["prompt_cache_key"]
        );
    }
    for left in [0, 3, 4, 5] {
        for right in [0, 3, 4, 5] {
            if left != right {
                assert_ne!(
                    requests[left].headers["session-id"],
                    requests[right].headers["session-id"]
                );
                assert_ne!(
                    requests[left].body["prompt_cache_key"],
                    requests[right].body["prompt_cache_key"]
                );
            }
        }
    }
}

// 父用例只将合成持久请求写入临时目录，再启动当前测试二进制。
// 独立标为 ignored，避免没有父夹具时被统计成一条已通过的恢复验收。
#[test]
#[ignore = "由真实子进程恢复用例显式启动"]
fn restored_codex_turn_state_child() {
    let path = std::env::var_os("EVE_CODEX_FIXTURE_RECORD").unwrap();
    let mut restored: ModelRequest = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(restored.session, Some(session().for_request(2)));
    let url = std::env::var("EVE_CODEX_FIXTURE_URL").unwrap();
    let mut config = CodexConfig::new("fixture-model")
        .with_base_url(&url)
        .unwrap();
    config.request_timeout = Duration::from_secs(4);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let provider = CodexProvider::anonymous(config)
                .unwrap()
                .with_transient_retries();
            let ModelResponse::ToolCalls { calls } =
                provider.complete(restored.clone()).await.unwrap()
            else {
                panic!("恢复后的合成响应必须包含完整工具批次");
            };
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "after-restart");
            restored.session = Some(restored.session.as_ref().unwrap().for_request(3));
            restored
                .messages
                .push(ChatMessage::assistant_tool_calls(calls).unwrap());
            restored.messages.push(
                ChatMessage::tool_results(vec![
                    ToolResult::success("after-restart", json!({"synthetic_receipt":true}))
                        .unwrap(),
                ])
                .unwrap(),
            );
            assert_eq!(
                provider.complete(restored).await.unwrap(),
                ModelResponse::Final {
                    text: "恢复成功".into()
                }
            );
        });
}

#[tokio::test]
async fn real_child_process_keeps_persisted_session_and_reacquires_its_own_turn_token() {
    let fixture = Fixture::start(vec![
        Reply::sse(vec![
            done(function("before", "before-restart", "{}")),
            completed(),
        ])
        .turn_token("before-process-token"),
        Reply::text("另一任务").turn_token("other-task-token"),
        Reply::text("结束前正文"),
        Reply::error(503, "synthetic-transient-error"),
        Reply::sse(vec![
            done(function("after", "after-restart", "{}")),
            completed(),
        ])
        .turn_token("restored-process-token"),
        Reply::text("恢复成功"),
    ])
    .await;
    let provider = fixture.provider();
    let original = session();
    let ModelResponse::ToolCalls { calls } =
        provider.complete(request(original.clone())).await.unwrap()
    else {
        panic!("首个合成响应必须包含完整工具批次");
    };
    provider
        .complete(request(
            ModelSession::anchored("task", Some(&scope()), "unrelated-task")
                .unwrap()
                .with_turn("initial")
                .unwrap(),
        ))
        .await
        .unwrap();
    let mut roundtrip = request(original.for_request(2));
    roundtrip
        .messages
        .push(ChatMessage::assistant_tool_calls(calls).unwrap());
    roundtrip.messages.push(
        ChatMessage::tool_results(vec![
            ToolResult::success("before-restart", json!({"synthetic_receipt":true})).unwrap(),
        ])
        .unwrap(),
    );
    let temporary = tempfile::tempdir().unwrap();
    let record = temporary.path().join("model-request.json");
    std::fs::write(&record, serde_json::to_vec(&roundtrip).unwrap()).unwrap();
    provider.complete(roundtrip).await.unwrap();
    drop(provider);

    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--ignored",
            "--exact",
            "restored_codex_turn_state_child",
            "--nocapture",
        ])
        .env_clear()
        .env("EVE_CODEX_FIXTURE_RECORD", &record)
        .env("EVE_CODEX_FIXTURE_URL", &fixture.base)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(12), child.output())
        .await
        .expect("本夹具子进程未在期限内退出")
        .unwrap();
    assert!(
        output.status.success(),
        "合成子进程失败: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = fixture.captures();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        requests[2].headers["x-codex-turn-state"],
        "before-process-token"
    );
    assert_eq!(
        requests[5].headers["x-codex-turn-state"],
        "restored-process-token"
    );
    for index in [0, 1, 3, 4] {
        assert!(!requests[index].headers.contains_key("x-codex-turn-state"));
    }
    for header in ["session-id", "thread-id"] {
        assert_ne!(requests[0].headers[header], requests[1].headers[header]);
        for index in [2, 3, 4, 5] {
            assert_eq!(requests[0].headers[header], requests[index].headers[header]);
        }
    }
    assert_ne!(
        requests[0].body["prompt_cache_key"],
        requests[1].body["prompt_cache_key"]
    );
    for index in [2, 3, 4, 5] {
        assert_eq!(
            requests[0].body["prompt_cache_key"],
            requests[index].body["prompt_cache_key"]
        );
    }
    for index in [3, 4] {
        assert_eq!(requests[2].body, requests[index].body);
        assert_eq!(
            requests[2].headers["x-client-request-id"],
            requests[index].headers["x-client-request-id"]
        );
    }
    assert_ne!(
        requests[4].headers["x-client-request-id"],
        requests[5].headers["x-client-request-id"]
    );
}

#[tokio::test]
async fn native_mapping_keeps_history_images_failures_and_explicit_reasoning() {
    let fixture = Fixture::start(vec![Reply::text("{\"ok\":true}")]).await;
    let mut config = fixture.config();
    config.reasoning_effort = Some("high".into());
    config.max_output_tokens = Some(1234);
    let provider = CodexProvider::new(config, "synthetic-test-key").unwrap();
    let image = eve_llm_api::ImageInput::new(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap();
    let mut model_request = request(session());
    model_request.messages = vec![
        ChatMessage::text(ChatRole::System, "规则"),
        ChatMessage::text(ChatRole::User, "观察图像").with_images(vec![image]),
        ChatMessage::internal_text("未发送的收尾"),
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: json!({"x":1}),
        }])
        .unwrap(),
        ChatMessage::tool_results(vec![
            ToolResult::failure("call-1", ToolFailureCode::TimedOut, "未完成").unwrap(),
        ])
        .unwrap(),
    ];
    model_request.tools = vec![definition("z"), definition("echo")];
    provider.complete_json(model_request).await.unwrap();
    let requests = fixture.captures();
    let body = &requests[0].body;
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["max_output_tokens"], 1234);
    assert!(body.get("temperature").is_none());
    assert_eq!(body["text"]["format"]["type"], "json_object");
    assert_eq!(body["tools"][0]["name"], "echo");
    assert!(body["tools"][0].get("required_permissions").is_none());
    assert_eq!(body["input"][1]["content"][1]["type"], "input_image");
    assert!(
        body["input"][2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("未发送")
    );
    assert_eq!(body["input"][3]["type"], "function_call");
    assert_eq!(body["input"][4]["type"], "function_call_output");
    let output: Value = serde_json::from_str(body["input"][4]["output"].as_str().unwrap()).unwrap();
    assert_eq!(output["error"]["code"], "TimedOut");
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer synthetic-test-key"
    );
}

#[tokio::test]
async fn unknown_native_tool_and_duplicate_arguments_never_return_partial_batch() {
    let fixture = Fixture::start(vec![
        Reply::sse(vec![
            done(function("a", "a", "{}")),
            done(json!({"type":"web_search_call","id":"native-tool"})),
            completed(),
        ]),
        Reply::sse(vec![
            done(function("b", "b", "{\"x\":1,\"x\":2}")),
            completed(),
        ]),
        Reply::sse(vec![
            done(function("c", "same", "{}")),
            done(function("d", "same", "{}")),
            completed(),
        ]),
    ])
    .await;
    let provider = fixture.provider();
    for ordinal in 1..=3 {
        let error = provider
            .complete(request(session().for_request(ordinal)))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            LlmError::Unsupported(_) | LlmError::Protocol(_)
        ));
    }
}

#[tokio::test]
async fn incomplete_text_mismatched_delta_and_missing_done_remain_failures() {
    let mut interrupted = completed();
    interrupted["type"] = json!("response.incomplete");
    interrupted["response"]["incomplete_details"] = json!({"reason":"interrupted"});
    let fixture = Fixture::start(vec![
        Reply::sse(vec![
            done(message("m", "部分答案", "final_answer")),
            interrupted,
        ]),
        Reply::sse(vec![
            json!({"type":"response.output_text.delta","delta":"原来"}),
            done(message("m", "不同", "final_answer")),
            completed(),
        ]),
        Reply::sse(vec![
            json!({"type":"response.output_item.added","item":function("pending","pending","")}),
            done(message("m", "完成", "final_answer")),
            completed(),
        ]),
    ])
    .await;
    let provider = fixture.provider();
    for ordinal in 1..=3 {
        assert!(
            provider
                .complete(request(session().for_request(ordinal)))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn response_limit_uses_original_transport_for_chunked_stream_and_error_body() {
    let mut streamed = Reply::text(&"大".repeat(1000));
    streamed.chunked = true;
    let fixture = Fixture::start(vec![streamed, Reply::error(500, &"x".repeat(4096))]).await;
    let mut config = fixture.config();
    config.max_response_bytes = 512;
    let provider = CodexProvider::anonymous(config).unwrap();
    for ordinal in 1..=2 {
        let error = provider
            .complete(request(session().for_request(ordinal)))
            .await
            .unwrap_err();
        assert_eq!(
            error.provider_failure().unwrap().kind,
            ProviderFailureKind::ResponseTooLarge
        );
    }
}

#[tokio::test]
async fn provider_error_keeps_status_without_raw_body_or_credentials() {
    let fixture = Fixture::start(vec![Reply::error(
        400,
        "secret-body-with-synthetic-test-key",
    )])
    .await;
    let provider = CodexProvider::new(fixture.config(), "synthetic-test-key")
        .unwrap()
        .with_transient_retries();
    let error = provider.complete(request(session())).await.unwrap_err();
    assert_eq!(error.provider_http_status(), Some(400));
    assert!(!error.to_string().contains("secret"));
    assert!(!error.to_string().contains("synthetic-test-key"));
    assert_eq!(fixture.captures().len(), 1);
}

#[tokio::test]
async fn first_response_deadline_includes_headers_and_silent_body() {
    for wait_headers in [true, false] {
        let mut reply = Reply::text("迟到");
        if wait_headers {
            reply.delay_headers = Duration::from_millis(250);
        } else {
            reply.delay_body = Duration::from_millis(250);
        }
        let fixture = Fixture::start(vec![reply]).await;
        let provider = fixture
            .provider()
            .with_stream_timeouts(StreamTimeoutPolicy {
                first_response: Duration::from_millis(80),
                idle: Duration::from_millis(80),
                total: Duration::from_secs(2),
            })
            .unwrap();
        assert_eq!(
            provider.complete(request(session())).await.unwrap_err(),
            LlmError::ProviderTimeout
        );
    }
}

#[tokio::test]
async fn sink_cancellation_does_not_publish_usage_or_complete_partial_text() {
    struct Closed;
    impl ModelTextSink for Closed {
        fn text_delta(&self, _: String) -> LlmFuture<'_, ()> {
            Box::pin(async { Err(LlmError::Cancelled) })
        }
    }
    let fixture = Fixture::start(vec![Reply::sse(vec![
        json!({"type":"response.output_text.delta","delta":"预览"}),
        done(message("m", "预览", "final_answer")),
        completed(),
    ])])
    .await;
    let usage = Arc::new(Usage::default());
    let provider = fixture
        .provider()
        .with_usage_observer(usage.clone(), "main");
    assert_eq!(
        provider
            .stream(request(session()), &Closed)
            .await
            .unwrap_err(),
        LlmError::Cancelled
    );
    assert!(usage.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_trusted_session_and_chat_endpoint_fail_before_network() {
    let fixture = Fixture::start(Vec::new()).await;
    let provider = fixture.provider();
    let mut model_request = request(session());
    model_request.session = None;
    assert!(matches!(
        provider.complete(model_request).await,
        Err(LlmError::Configuration(_))
    ));
    assert!(fixture.captures().is_empty());
    assert!(
        CodexConfig::new("m")
            .with_base_url("http://127.0.0.1/v1/chat/completions")
            .is_err()
    );
    let mut config = fixture.config();
    config.responses_url = "http://127.0.0.1/v1/chat/completions".into();
    assert!(CodexProvider::anonymous(config).is_err());
}

#[tokio::test]
async fn guard_rejects_skipped_malformed_items_and_incomplete_tool_batches() {
    let mut interrupted = completed();
    interrupted["type"] = json!("response.incomplete");
    interrupted["response"]["incomplete_details"] = json!({"reason":"interrupted"});
    let fixture = Fixture::start(vec![
        // 原 parser 会跳过第二个损坏的 done；Eve 不能执行剩余的第一把工具。
        Reply::sse(vec![
            done(function("good", "good", "{}")),
            done(json!({"type":"function_call","call_id":"broken","name":"echo","arguments":42})),
            completed(),
        ]),
        Reply::sse(vec![done(function("first", "first", "{}")), interrupted]),
        Reply::sse(vec![
            done(function("good", "good", "{}")),
            json!({"type":"response.failed","response":{"id":"response-1","error":{"code":"server_error"}}}),
            completed(),
        ]),
    ]).await;
    let provider = fixture.provider();
    for ordinal in 1..=3 {
        assert!(
            provider
                .complete(request(session().for_request(ordinal)))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn completed_output_cannot_hide_a_missing_or_changed_done_item() {
    let first = function("first", "first", "{}");
    let second = function("second", "second", "{}");
    let mut missing = completed();
    missing["response"]["output"] = json!([first.clone(), second]);
    let mut changed = completed();
    changed["response"]["output"] = json!([function("first", "first", "{\"changed\":true}")]);
    let mut empty = completed();
    empty["response"]["output"] = json!([]);
    let fixture = Fixture::start(vec![
        Reply::sse(vec![done(first.clone()), missing]),
        Reply::sse(vec![done(first.clone()), changed]),
        Reply::sse(vec![done(first), empty]),
    ])
    .await;
    let usage = Arc::new(Usage::default());
    let provider = fixture
        .provider()
        .with_usage_observer(usage.clone(), "main");
    for ordinal in 1..=3 {
        assert!(matches!(
            provider
                .complete(request(session().for_request(ordinal)))
                .await,
            Err(LlmError::Protocol(_))
        ));
    }
    assert!(usage.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn matching_completed_output_keeps_original_done_order_and_allows_annotations() {
    let first = function("first", "first", "{\"position\":1}");
    let second = function("second", "second", "{\"position\":2}");
    let mut commentary = message("commentary", "正在观察", "commentary");
    commentary["content"][0]["annotations"] = json!([]);
    let mut completion = completed();
    completion["response"]["output"] = json!([commentary.clone(), first.clone(), second.clone()]);
    let fixture = Fixture::start(vec![Reply::sse(vec![
        done(commentary),
        done(second),
        done(first),
        completion,
    ])])
    .await;
    let sink = Sink::default();
    let ModelResponse::ToolCalls { calls } = fixture
        .provider()
        .stream(request(session()), &sink)
        .await
        .unwrap()
    else {
        panic!("完整输出应保持原函数批次");
    };
    assert_eq!(
        calls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "first"]
    );
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn normal_text_with_citation_annotations_is_still_delivered() {
    let mut item = message("m", "资料中的文字", "final_answer");
    item["content"][0]["annotations"] = json!([
        {"type":"url_citation","url":"https://example.invalid/source","title":"合成资料","start_index":0,"end_index":4}
    ]);
    let fixture = Fixture::start(vec![Reply::sse(vec![done(item), completed()])]).await;
    assert_eq!(
        fixture
            .provider()
            .complete(request(session()))
            .await
            .unwrap(),
        ModelResponse::Final {
            text: "资料中的文字".into()
        }
    );
}

#[tokio::test]
async fn completed_text_without_deltas_reaches_the_host_text_sink() {
    let fixture = Fixture::start(vec![Reply::text("只发送完整正文")]).await;
    let sink = Sink::default();
    assert_eq!(
        fixture
            .provider()
            .stream(request(session()), &sink)
            .await
            .unwrap(),
        ModelResponse::Final {
            text: "只发送完整正文".into()
        }
    );
    assert_eq!(*sink.0.lock().unwrap(), "只发送完整正文");
}

#[tokio::test]
async fn commentary_deltas_are_not_mixed_into_final_host_text() {
    let fixture = Fixture::start(vec![Reply::sse(vec![
        json!({"type":"response.output_text.delta","delta":"正在查看资料"}),
        done(message("commentary", "正在查看资料", "commentary")),
        json!({"type":"response.output_text.delta","delta":"实际正文"}),
        done(message("final", "实际正文", "final_answer")),
        completed(),
    ])])
    .await;
    let sink = Sink::default();
    assert_eq!(
        fixture
            .provider()
            .stream(request(session()), &sink)
            .await
            .unwrap(),
        ModelResponse::Final {
            text: "实际正文".into()
        }
    );
    assert_eq!(*sink.0.lock().unwrap(), "实际正文");
}

#[tokio::test]
async fn contradictory_or_negative_cache_usage_is_not_published() {
    let fixture = Fixture::start(vec![
        Reply::sse(vec![done(message("m","完成","final_answer")), json!({"type":"response.completed","response":{"id":"response-1","usage":{
            "input_tokens":10,"input_tokens_details":{"cached_tokens":11},"output_tokens":2,"total_tokens":12
        }}})]),
        Reply::sse(vec![done(message("m","完成","final_answer")), json!({"type":"response.completed","response":{"id":"response-1","usage":{
            "input_tokens":10,"input_tokens_details":{"cached_tokens":-1},"output_tokens":2,"total_tokens":12
        }}})]),
    ]).await;
    let usage = Arc::new(Usage::default());
    let provider = fixture
        .provider()
        .with_usage_observer(usage.clone(), "main");
    for ordinal in 1..=2 {
        assert!(matches!(
            provider
                .complete(request(session().for_request(ordinal)))
                .await,
            Err(LlmError::Protocol(_))
        ));
    }
    assert!(usage.0.lock().unwrap().is_empty());
}
