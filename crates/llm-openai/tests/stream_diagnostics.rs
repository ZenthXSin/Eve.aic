#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmFuture, LlmProvider, ModelRequest, ModelResponse,
    ModelSession, ModelTextSink, ProtocolFailureKind,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::{Value, json};
use std::{collections::BTreeSet, process::Command, sync::Mutex, time::Duration};
use support::{Reply, Server};

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn chunk() -> Value {
    json!({"id":"fixture-private-response","choices":[{
        "index":0,"delta":{},"finish_reason":null
    }]})
}
fn cases() -> Vec<(&'static str, String)> {
    let mut cases = vec![
        (
            "event_name",
            format!("event: fixture-private-event\n{}", event(chunk())),
        ),
        ("done_before_finish", "data: [DONE]\n\n".into()),
        (
            "invalid_json",
            "data: {\"fixture-private-key\":1,\"fixture-private-key\":2}\n\n".into(),
        ),
    ];
    for (detail, pointer, value) in [
        ("response_id_invalid", "/id", json!("")),
        (
            "response_id_changed",
            "/id",
            json!("fixture-private-other-id"),
        ),
        ("choices_shape", "/choices", Value::Null),
        ("choice_count", "/choices", json!([])),
        ("choice_index", "/choices/0/index", json!(1)),
        ("delta_shape", "/choices/0/delta", Value::Null),
        (
            "role",
            "/choices/0/delta",
            json!({"role":"fixture-private-role"}),
        ),
        (
            "content_shape",
            "/choices/0/delta",
            json!({"content":["fixture-private-content"]}),
        ),
        (
            "tool_calls_shape",
            "/choices/0/delta",
            json!({"tool_calls":{}}),
        ),
        ("finish_shape", "/choices/0/finish_reason", json!(1)),
    ] {
        let mut value_chunk = chunk();
        *value_chunk.pointer_mut(pointer).unwrap() = value;
        cases.push((detail, event(value_chunk)));
    }
    for (detail, call) in [
        ("tool_index", json!({"index":256})),
        ("tool_id_invalid", json!({"index":0,"id":""})),
        (
            "tool_type",
            json!({"index":0,"type":"fixture-private-tool-type"}),
        ),
        ("tool_function_shape", json!({"index":0,"function":[]})),
        (
            "tool_fragment_shape",
            json!({"index":0,"function":{"arguments":{}}}),
        ),
    ] {
        let mut value = chunk();
        value["choices"][0]["delta"] = json!({"tool_calls":[call]});
        cases.push((detail, event(value)));
    }
    let mut first = chunk();
    first["choices"][0]["delta"] = json!({"tool_calls":[{
        "index":0,"id":"fixture-private-tool-id","type":"function",
        "function":{"name":"echo","arguments":"{}"}
    }]});
    let mut changed = first.clone();
    changed["choices"][0]["delta"]["tool_calls"][0]["id"] =
        json!("fixture-private-changed-tool-id");
    cases.push((
        "tool_id_changed",
        format!("{}{}", event(first), event(changed)),
    ));
    let mut incomplete = chunk();
    incomplete["choices"][0]["delta"] = json!({"tool_calls":[{"index":0}]});
    incomplete["choices"][0]["finish_reason"] = json!("tool_calls");
    cases.push(("tool_batch_identity", event(incomplete)));
    cases
}
fn prefix() -> String {
    let mut value = chunk();
    value["choices"][0]["delta"] = json!({
        "role":"assistant","content":"fixture-private-draft",
        "reasoning_content":"fixture-private-reasoning"
    });
    event(value)
}
fn reply(body: String) -> Reply {
    let mut reply = Reply::json(json!({}));
    reply.headers = vec![("content-type".into(), "text/event-stream".into())];
    reply.chunked = true;
    reply.body = body.into_bytes();
    reply
}
fn request() -> ModelRequest {
    ModelRequest {
        session: Some(
            ModelSession::scoped(
                "dialogue",
                &ContextScope {
                    session_id: "fixture-private-session".into(),
                    user_id: "fixture-private-user".into(),
                },
            )
            .unwrap(),
        ),
        messages: vec![ChatMessage::text(ChatRole::User, "fixture-private-input")],
        tools: vec![],
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

#[test]
fn chat_stream_diagnostics_keep_rejection_and_private_fields_out_of_logs() {
    const CHILD: &str = "EVE_TEST_STREAM_DIAGNOSTICS";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "chat_stream_diagnostics_keep_rejection_and_private_fields_out_of_logs",
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
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        };
        let details = records("EVE_MODEL_STREAM_DIAGNOSTIC ");
        let timings = records("EVE_HTTP_TIMING ");
        let failures = records("EVE_MODEL_PROTOCOL_FAILURE ");
        assert_eq!(details.len(), cases().len() + 1);
        assert_eq!(failures.len(), 1);
        for field in ["call", "request", "attempt", "role", "kind"] {
            assert_eq!(details.last().unwrap()[field], failures[0][field]);
        }
        assert_eq!(details.last().unwrap()["detail"], "delta_shape");
        assert!(records("EVE_MODEL_RETRY ").is_empty());
        let mut requests = BTreeSet::new();
        for (detail, (expected, body)) in details.iter().zip(cases()) {
            assert_eq!(detail["kind"], "StreamInvalid");
            assert_eq!(detail["detail"], expected);
            assert_eq!(detail["received_bytes"], prefix().len() + body.len());
            assert_eq!(
                detail["event_index"],
                if expected == "tool_id_changed" { 3 } else { 2 }
            );
            assert_eq!(detail["attempt"], 1);
            assert!(detail["role"].is_null());
            assert!(requests.insert(detail["request"].as_u64().unwrap()));
            let closed = timings
                .iter()
                .find(|timing| {
                    timing["request"] == detail["request"]
                        && timing["phase"] == "closed_without_decoded_response"
                })
                .unwrap();
            for field in ["call", "request", "attempt", "role"] {
                assert_eq!(detail[field], closed[field]);
            }
            let mut names: Vec<_> = detail
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec![
                    "attempt",
                    "call",
                    "detail",
                    "event_index",
                    "kind",
                    "received_bytes",
                    "request",
                    "role"
                ]
            );
        }
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (_, body) in cases() {
                let mut server = Server::start(vec![reply(format!("{}{body}", prefix()))]).await;
                let provider = OpenAiProvider::new(
                    OpenAiConfig {
                        responses_url: server.url.clone(),
                        request_timeout: Duration::from_secs(3),
                        ..OpenAiConfig::chat("fixture-private-model")
                    },
                    "fixture-private-api-key",
                )
                .unwrap()
                .with_timing_diagnostics()
                .with_transient_retries();
                let sink = Sink::default();
                let error = provider.stream(request(), &sink).await.unwrap_err();
                assert_eq!(
                    error.protocol_failure(),
                    Some(ProtocolFailureKind::StreamInvalid)
                );
                assert_eq!(
                    error.to_string(),
                    "protocol error: Chat 流片段无效、终态缺失或标识不一致"
                );
                assert!(sink.0.lock().unwrap().is_empty());
                server.next().await;
                assert!(server.requests.try_recv().is_err());
            }
            // 后台 complete 使用 SSE 时，原协议故障日志也沿用同一次 HTTP 尝试。
            let mut malformed = chunk();
            malformed["choices"][0]["delta"] = Value::Null;
            let mut server = Server::start(vec![reply(event(malformed))]).await;
            let provider = OpenAiProvider::new(
                OpenAiConfig {
                    responses_url: server.url.clone(),
                    ..OpenAiConfig::chat("fixture-private-model")
                },
                "fixture-private-api-key",
            )
            .unwrap()
            .with_streamed_completions()
            .with_transient_retries();
            assert_eq!(
                provider
                    .complete(request())
                    .await
                    .unwrap_err()
                    .protocol_failure(),
                Some(ProtocolFailureKind::StreamInvalid)
            );
            server.next().await;
            assert!(server.requests.try_recv().is_err());
            // 正常终态不产生错误诊断，reasoning 不进入最终正文。
            let mut final_chunk = chunk();
            final_chunk["choices"][0]["delta"] =
                json!({"content":"正常正文","reasoning_content":"fixture-private-reasoning"});
            final_chunk["choices"][0]["finish_reason"] = json!("stop");
            let server = Server::start(vec![reply(format!(
                "{}data: [DONE]\n\n",
                event(final_chunk)
            ))])
            .await;
            let provider = OpenAiProvider::new(
                OpenAiConfig {
                    responses_url: server.url.clone(),
                    ..OpenAiConfig::chat("fixture-private-model")
                },
                "fixture-private-api-key",
            )
            .unwrap();
            let sink = Sink::default();
            assert_eq!(
                provider.stream(request(), &sink).await.unwrap(),
                ModelResponse::Final {
                    text: "正常正文".into()
                }
            );
            assert_eq!(*sink.0.lock().unwrap(), "正常正文");
        });
}
