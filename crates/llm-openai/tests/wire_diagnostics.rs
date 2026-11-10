//! 真实 loopback 请求与独立子进程 stderr：诊断必须对应最终 wire，且不改变请求。
#[allow(dead_code)]
mod support;

use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmProvider, ModelRequest, ModelSession, ModelUsage,
    ModelUsageObserver, ToolCall, ToolDefinition, ToolResult,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider};
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::{process::Command, sync::Arc, time::Duration};
use support::{Captured, Reply, Server, final_response};

const CHILD: &str = "EVE_TEST_WIRE_DIAGNOSTICS_CHILD";
const FIXTURE: &str = "WIRE_FIXTURE ";
const FINAL: &str = "EVE_MODEL_WIRE_FINAL ";
const HEADERS: &str = "EVE_MODEL_WIRE_HEADERS ";
const USAGE: &str = "EVE_MODEL_WIRE_USAGE ";

fn records(text: &str, tag: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix(tag))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn child(name: &str) -> Option<(String, Vec<Value>)> {
    if std::env::var_os(CHILD).is_some() {
        return None;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
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
    for private in ["fixture-private", "127.0.0.1", "http://", "/v1/", "Bearer"] {
        assert!(!stderr.contains(private), "诊断泄漏私密值类别 {private}");
    }
    let rows = records(&String::from_utf8(output.stdout).unwrap(), FIXTURE);
    for row in &rows {
        for header in [
            "session-id",
            "thread-id",
            "x-client-request-id",
            "authorization",
        ] {
            if let Some(value) = header_value(row["headers"].as_str().unwrap(), header) {
                assert!(!stderr.contains(value), "原请求标识不得写进诊断");
            }
        }
        if let Some(value) = row["body"]["prompt_cache_key"].as_str() {
            assert!(!stderr.contains(value));
        }
    }
    Some((stderr, rows))
}
fn save_checked_stderr(name: &str, stderr: &str) {
    if let Some(directory) = std::env::var_os("EVE_WIRE_TEST_ARTIFACTS") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        // 仅测试父进程在脱敏和 wire 关联断言全部通过后保存；不导出子进程原始 stdout。
        std::fs::write(directory.join(format!("{name}.stderr.log")), stderr).unwrap();
    }
}
fn run(future: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}
fn wire_endpoint(row: &Value) -> String {
    let headers = row["headers"].as_str().unwrap();
    let target = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    format!("http://{}{target}", header_value(headers, "host").unwrap())
}
fn sha(value: &str) -> String {
    digest(&SHA256, value.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn identifier(summary: &Value, actual: Option<&str>) {
    match actual {
        Some(actual) => {
            assert_eq!(summary["status"], "present");
            assert_eq!(summary["bytes"], actual.len());
            assert_eq!(summary["sha256"], sha(actual));
        }
        None => {
            assert_eq!(summary["status"], "missing");
            assert_eq!(summary["bytes"], 0);
        }
    }
}
fn digest_summary(summary: &Value, actual: &Value) {
    assert_eq!(summary["bytes"], serde_json::to_vec(actual).unwrap().len());
    let hash = summary["hmac_sha256"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    assert!(
        hash.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
}
fn assert_wire(record: &Value, row: &Value) {
    let body = &row["body"];
    let headers = row["headers"].as_str().unwrap();
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["event"], "request");
    assert_eq!(record["domain"], "eve.wire_diagnostics.v1");
    assert_eq!(record["process_nonce"].as_str().unwrap().len(), 32);
    assert!(record["call"].is_u64() && record["request"].is_u64());
    assert_eq!(record["role"], "dialogue");
    digest_summary(&record["destination"], &json!(wire_endpoint(row)));
    digest_summary(&record["body"], body);
    assert_eq!(
        header_value(headers, "content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        serde_json::to_vec(body).unwrap().len()
    );
    let input = if record["protocol"] == "chat" {
        &body["messages"]
    } else {
        &body["input"]
    };
    digest_summary(&record["input"], input);
    assert_eq!(record["message_count"], input.as_array().unwrap().len());
    for (index, actual) in input.as_array().unwrap().iter().enumerate() {
        digest_summary(&record["messages"][index], actual);
        assert_eq!(record["messages"][index]["index"], index);
    }
    let mut config = body.clone();
    for key in ["messages", "input", "tools", "prompt_cache_key"] {
        config.as_object_mut().unwrap().remove(key);
    }
    digest_summary(&record["config"], &config);
    if let Some(tools) = body.get("tools") {
        digest_summary(&record["tools"], tools);
        for (index, tool) in tools.as_array().unwrap().iter().enumerate() {
            digest_summary(&record["tools"]["items"][index], tool);
        }
    } else {
        assert_eq!(record["tools"]["status"], "missing");
    }
    for (field, name) in [
        ("session_id", "session-id"),
        ("thread_id", "thread-id"),
        ("client_request_id", "x-client-request-id"),
    ] {
        identifier(&record["identifiers"][field], header_value(headers, name));
    }
    identifier(
        &record["identifiers"]["prompt_cache_key"],
        body["prompt_cache_key"].as_str(),
    );
}
fn assert_identity(left: &Value, right: &Value) {
    for field in ["call", "request", "attempt", "role"] {
        assert_eq!(left[field], right[field], "{field}");
    }
}
fn assert_header_record(record: &Value, row: &Value) {
    identifier(&record["upstream_request_id"], row["upstream"].as_str());
    identifier(&record["oneapi_request_id"], row["oneapi"].as_str());
}
fn publish(label: &str, captured: Captured, upstream: &str, oneapi: &str) {
    // 仅测试 stdout 留下合成 wire；待验证的产品 stderr 不混入这些原件。
    println!(
        "\n{FIXTURE}{}",
        json!({"label":label,"body":captured.body,"headers":captured.headers,"upstream":upstream,"oneapi":oneapi})
    );
}
struct IgnoreUsage;
impl ModelUsageObserver for IgnoreUsage {
    fn reported(&self, _: &str, _: ModelUsage) {}
}
fn provider(url: &str, protocol: OpenAiProtocol, effort: &str) -> OpenAiProvider {
    OpenAiProvider::new(
        OpenAiConfig {
            responses_url: url.into(),
            protocol,
            reasoning_effort: Some(effort.into()),
            max_output_tokens: Some(4096),
            request_timeout: Duration::from_secs(5),
            ..OpenAiConfig::new("fixture-private-model")
        },
        "fixture-private-api-key",
    )
    .unwrap()
    .with_usage_observer(Arc::new(IgnoreUsage), "dialogue")
}
fn session(user: &str) -> ModelSession {
    ModelSession::scoped(
        "main",
        &ContextScope {
            session_id: "fixture-private-conversation".into(),
            user_id: user.into(),
        },
    )
    .unwrap()
    .with_turn("fixture-private-turn")
    .unwrap()
}
fn request() -> ModelRequest {
    ModelRequest {
        session: Some(session("fixture-private-user").for_request(1)),
        messages: vec![
            ChatMessage::text(ChatRole::System, "fixture-private-system"),
            ChatMessage::text(ChatRole::User, "fixture-private-user-message"),
        ],
        tools: vec![ToolDefinition {
            name: "fixture-private-tool".into(),
            description: "fixture-private-schema-description".into(),
            argument_schema: json!({"type":"object","properties":{"fixture-private-parameter":{"type":"string"}}}),
            required_permissions: vec![],
            concurrency: None,
        }],
    }
}
fn reply(protocol: OpenAiProtocol, cached: Option<u64>, usage: bool, upstream: &str) -> Reply {
    let mut value = if protocol == OpenAiProtocol::Responses {
        final_response("fixture-private-output")
    } else {
        json!({"id":"fixture-private-completion","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"fixture-private-output"}}]})
    };
    if usage {
        let (input, output, detail) = if protocol == OpenAiProtocol::Responses {
            ("input_tokens", "output_tokens", "input_tokens_details")
        } else {
            (
                "prompt_tokens",
                "completion_tokens",
                "prompt_tokens_details",
            )
        };
        value["usage"] = json!({});
        value["usage"][input] = json!(2048);
        value["usage"][output] = json!(7);
        if let Some(cached) = cached {
            value["usage"][detail] = json!({"cached_tokens":cached});
        }
    }
    let mut result = Reply::json(value);
    result.headers = vec![
        ("x-request-id".into(), upstream.into()),
        (
            "x-oneapi-request-id".into(),
            "fixture-private-oneapi".into(),
        ),
        (
            "x-secret-header".into(),
            "fixture-private-header-value".into(),
        ),
    ];
    result
}

#[test]
fn diagnostics_default_off_timing_independent_and_actual_wire_unchanged() {
    if let Some((stderr, rows)) =
        child("diagnostics_default_off_timing_independent_and_actual_wire_unchanged")
    {
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["body"], rows[1]["body"]);
        assert_eq!(rows[1]["body"], rows[2]["body"]);
        assert_eq!(rows[0]["headers"], rows[1]["headers"]);
        assert_eq!(rows[1]["headers"], rows[2]["headers"]);
        let finals = records(&stderr, FINAL);
        let heads = records(&stderr, HEADERS);
        let usage = records(&stderr, USAGE);
        assert_eq!(finals.len(), 1);
        assert_eq!(heads.len(), 1);
        assert_eq!(usage.len(), 1);
        assert_wire(&finals[0], &rows[2]);
        assert_identity(&finals[0], &heads[0]);
        assert_identity(&finals[0], &usage[0]);
        assert_header_record(&heads[0], &rows[2]);
        let timing = records(&stderr, "EVE_HTTP_TIMING ");
        assert!(!timing.is_empty());
        assert!(timing.iter().all(|row| row["call"] != finals[0]["call"]));
        save_checked_stderr(
            "diagnostics_default_off_timing_independent_and_actual_wire_unchanged",
            &stderr,
        );
        return;
    }
    run(async {
        let mut server = Server::start(
            (0..3)
                .map(|_| {
                    reply(
                        OpenAiProtocol::Responses,
                        Some(1024),
                        true,
                        "fixture-private-upstream",
                    )
                })
                .collect(),
        )
        .await;
        for mode in 0..3 {
            let mut provider = provider(&server.url, OpenAiProtocol::Responses, "high");
            if mode == 1 {
                provider = provider.with_timing_diagnostics();
            }
            if mode == 2 {
                provider = provider.with_wire_diagnostics();
            }
            provider.complete(request()).await.unwrap();
            publish(
                &mode.to_string(),
                server.next().await,
                "fixture-private-upstream",
                "fixture-private-oneapi",
            );
        }
        assert!(server.requests.try_recv().is_err());
    });
}

#[test]
fn plain_and_stream_tail_usage_preserve_positive_zero_missing_and_request_identity() {
    if let Some((stderr, rows)) =
        child("plain_and_stream_tail_usage_preserve_positive_zero_missing_and_request_identity")
    {
        let finals = records(&stderr, FINAL);
        let heads = records(&stderr, HEADERS);
        let usage = records(&stderr, USAGE);
        assert_eq!(rows.len(), 12);
        assert_eq!(finals.len(), 12);
        assert_eq!(heads.len(), 12);
        assert_eq!(usage.len(), 12);
        for chunk in finals.as_chunks::<4>().0 {
            assert!(
                chunk
                    .iter()
                    .all(|record| record["destination"] == chunk[0]["destination"])
            );
        }
        // 模式 1/2 都是 Chat，但目标端口不同；内容相同不能把不同目标分到同一组。
        assert_eq!(finals[4]["protocol"], finals[8]["protocol"]);
        assert_ne!(wire_endpoint(&rows[4]), wire_endpoint(&rows[8]));
        assert_ne!(
            finals[4]["destination"]["hmac_sha256"],
            finals[8]["destination"]["hmac_sha256"]
        );
        for (index, row) in rows.iter().enumerate() {
            assert_wire(&finals[index], row);
            assert_header_record(&heads[index], row);
            assert_identity(&finals[index], &heads[index]);
            assert_identity(&finals[index], &usage[index]);
            assert_eq!(finals[index]["attempt"], 1);
            let kind = index % 4;
            assert_eq!(usage[index]["usage_reported"], kind != 3);
            assert_eq!(
                usage[index]["input_tokens"],
                if kind == 3 { Value::Null } else { json!(2048) }
            );
            assert_eq!(
                usage[index]["output_tokens"],
                if kind == 3 { Value::Null } else { json!(7) }
            );
            assert_eq!(
                usage[index]["cached_input_tokens"],
                match kind {
                    0 => json!(1024),
                    1 => json!(0),
                    _ => Value::Null,
                }
            );
            assert_eq!(row["body"]["stream"], index >= 8);
            if index >= 8 {
                assert_eq!(row["body"]["stream_options"], json!({"include_usage":true}));
            } else {
                assert!(row["body"].get("stream_options").is_none());
            }
        }
        assert!(!stderr.contains("EVE_MODEL_RETRY "));
        assert!(!stderr.contains("EVE_HTTP_TIMING "));
        save_checked_stderr(
            "plain_and_stream_tail_usage_preserve_positive_zero_missing_and_request_identity",
            &stderr,
        );
        return;
    }
    run(async {
        // 保留三台现有 loopback 服务器到用例结束，避免操作系统复用临时端口。
        let mut active_servers = Vec::new();
        for mode in 0..3 {
            let protocol = if mode == 0 {
                OpenAiProtocol::Responses
            } else {
                OpenAiProtocol::ChatCompletions
            };
            let mut replies = vec![];
            for kind in 0..4 {
                let cached = match kind {
                    0 => Some(1024),
                    1 => Some(0),
                    _ => None,
                };
                let mut response = reply(protocol, cached, kind != 3, "fixture-private-upstream");
                if mode == 2 {
                    let plain: Value = serde_json::from_slice(&response.body).unwrap();
                    response
                        .headers
                        .push(("content-type".into(), "text/event-stream".into()));
                    response.chunked = true;
                    response.body.clear();
                    let terminal = format!(
                        "data: {}\n\n",
                        json!({"id":"fixture-private-completion","choices":[{"index":0,"delta":{"content":"fixture-private-output"},"finish_reason":"stop"}]})
                    );
                    let tail = if kind == 3 {
                        "data: [DONE]\n\n".into()
                    } else {
                        format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            json!({"id":"fixture-private-completion","choices":[],"usage":plain["usage"]})
                        )
                    };
                    response.delayed_chunks = vec![
                        (Duration::ZERO, terminal.into_bytes()),
                        (Duration::from_millis(20), tail.into_bytes()),
                    ];
                }
                replies.push(response);
            }
            let mut server = Server::start(replies).await;
            let mut provider = provider(&server.url, protocol, "high").with_wire_diagnostics();
            if mode == 2 {
                provider = provider.with_streamed_completions();
            }
            for kind in 0..4 {
                provider.complete(request()).await.unwrap();
                publish(
                    &format!("{mode}-{kind}"),
                    server.next().await,
                    "fixture-private-upstream",
                    "fixture-private-oneapi",
                );
            }
            assert!(server.requests.try_recv().is_err());
            active_servers.push(server);
        }
    });
}

#[test]
fn append_prefix_drift_scope_config_and_retry_match_actual_wire_without_leaks() {
    if let Some((stderr, rows)) =
        child("append_prefix_drift_scope_config_and_retry_match_actual_wire_without_leaks")
    {
        let finals = records(&stderr, FINAL);
        let heads = records(&stderr, HEADERS);
        let usage = records(&stderr, USAGE);
        assert_eq!(rows.len(), 18);
        assert_eq!(finals.len(), 18);
        assert_eq!(heads.len(), 18);
        assert_eq!(usage.len(), 16);
        for (index, row) in rows.iter().enumerate() {
            assert_wire(&finals[index], row);
            assert_header_record(&heads[index], row);
            assert_identity(&finals[index], &heads[index]);
        }
        for offset in [0, 9] {
            let f = &finals[offset..offset + 9];
            let r = &rows[offset..offset + 9];
            assert!(
                f.iter()
                    .all(|record| record["destination"] == f[0]["destination"])
            );
            let input = if offset == 0 { "input" } else { "messages" };
            for (a, b) in [(0, 1), (1, 2)] {
                let old = r[a]["body"][input].as_array().unwrap();
                let new = r[b]["body"][input].as_array().unwrap();
                assert!(new.len() > old.len());
                assert_eq!(old, &new[..old.len()]);
                assert_eq!(
                    &f[a]["messages"].as_array().unwrap()[..old.len()],
                    &f[b]["messages"].as_array().unwrap()[..old.len()]
                );
                assert_eq!(f[a]["config"], f[b]["config"]);
                assert_eq!(f[a]["tools"], f[b]["tools"]);
                assert_eq!(
                    f[a]["identifiers"]["session_id"],
                    f[b]["identifiers"]["session_id"]
                );
                assert_ne!(
                    f[a]["identifiers"]["client_request_id"],
                    f[b]["identifiers"]["client_request_id"]
                );
                assert_ne!(f[a]["body"]["hmac_sha256"], f[b]["body"]["hmac_sha256"]);
            }
            assert_ne!(r[2]["body"][input][0], r[3]["body"][input][0]);
            assert_eq!(f[2]["messages"][0]["bytes"], f[3]["messages"][0]["bytes"]);
            assert_ne!(
                f[2]["messages"][0]["hmac_sha256"],
                f[3]["messages"][0]["hmac_sha256"]
            );
            assert_eq!(f[2]["messages"][1], f[3]["messages"][1]);
            assert_ne!(r[0]["body"]["tools"], r[4]["body"]["tools"]);
            assert_ne!(f[0]["tools"]["hmac_sha256"], f[4]["tools"]["hmac_sha256"]);
            assert_eq!(f[0]["input"], f[4]["input"]);
            assert_eq!(f[0]["input"], f[5]["input"]);
            assert_eq!(f[0]["config"], f[5]["config"]);
            assert_ne!(
                f[0]["identifiers"]["session_id"],
                f[5]["identifiers"]["session_id"]
            );
            assert_eq!(f[0]["input"], f[6]["input"]);
            assert_ne!(f[0]["config"]["hmac_sha256"], f[6]["config"]["hmac_sha256"]);
            assert_eq!(r[7]["body"], r[8]["body"]);
            assert_eq!(r[7]["headers"], r[8]["headers"]);
            for field in ["body", "input", "config", "tools", "identifiers"] {
                assert_eq!(f[7][field], f[8][field]);
            }
            assert_eq!(f[7]["call"], f[8]["call"]);
            assert_ne!(f[7]["request"], f[8]["request"]);
            assert_eq!(f[7]["attempt"], 1);
            assert_eq!(f[8]["attempt"], 2);
            assert_ne!(
                heads[offset + 7]["upstream_request_id"],
                heads[offset + 8]["upstream_request_id"]
            );
            let reported: Vec<_> = usage.iter().filter(|u| u["call"] == f[7]["call"]).collect();
            assert_eq!(reported.len(), 1);
            assert_identity(&f[8], reported[0]);
            for item in f {
                let matched: Vec<_> = records(&stderr, "EVE_HTTP_TIMING ")
                    .into_iter()
                    .filter(|t| t["request"] == item["request"])
                    .collect();
                assert!(!matched.is_empty());
                for timing in matched {
                    assert_identity(item, &timing);
                }
            }
            for item in &f[..7] {
                assert_ne!(item["call"], f[7]["call"]);
            }
        }
        assert_eq!(records(&stderr, "EVE_MODEL_RETRY ").len(), 2);
        save_checked_stderr(
            "append_prefix_drift_scope_config_and_retry_match_actual_wire_without_leaks",
            &stderr,
        );
        return;
    }
    run(async {
        for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
            let mut replies: Vec<_> = (0..7)
                .map(|_| reply(protocol, Some(1024), true, "fixture-private-upstream"))
                .collect();
            let mut busy = reply(protocol, None, false, "fixture-private-upstream-retry-1");
            busy.status = 503;
            replies.extend([
                busy,
                reply(
                    protocol,
                    Some(1024),
                    true,
                    "fixture-private-upstream-retry-2",
                ),
            ]);
            let mut server = Server::start(replies).await;
            let p = provider(&server.url, protocol, "high")
                .with_wire_diagnostics()
                .with_timing_diagnostics();
            let first = request();
            let mut tool = first.clone();
            tool.session = Some(session("fixture-private-user").for_request(2));
            tool.messages.push(
                ChatMessage::assistant_tool_calls(vec![ToolCall {
                    id: "fixture-private-call".into(),
                    name: "fixture-private-tool".into(),
                    arguments: json!({"fixture-private-parameter":"fixture-private-argument"}),
                }])
                .unwrap(),
            );
            tool.messages.push(
                ChatMessage::tool_results(vec![
                    ToolResult::success(
                        "fixture-private-call",
                        json!({"value":"fixture-private-tool-result"}),
                    )
                    .unwrap(),
                ])
                .unwrap(),
            );
            let mut appended = tool.clone();
            appended.session = Some(session("fixture-private-user").for_request(3));
            appended.messages.push(ChatMessage::text(
                ChatRole::User,
                "fixture-private-additional-message",
            ));
            let mut system = appended.clone();
            system.messages[0] = ChatMessage::text(ChatRole::System, "fixture-private-systEm");
            let mut schema = first.clone();
            schema.tools[0].argument_schema["properties"]["fixture-private-new-parameter"] =
                json!({"type":"number"});
            let mut scope = first.clone();
            scope.session = Some(session("fixture-private-other-user").for_request(1));
            for (index, request) in [first.clone(), tool, appended, system, schema, scope]
                .into_iter()
                .enumerate()
            {
                p.complete(request).await.unwrap();
                publish(
                    &index.to_string(),
                    server.next().await,
                    "fixture-private-upstream",
                    "fixture-private-oneapi",
                );
            }
            provider(&server.url, protocol, "medium")
                .with_wire_diagnostics()
                .with_timing_diagnostics()
                .complete(first.clone())
                .await
                .unwrap();
            publish(
                "config",
                server.next().await,
                "fixture-private-upstream",
                "fixture-private-oneapi",
            );
            p.with_transient_retries().complete(first).await.unwrap();
            for attempt in 1..=2 {
                publish(
                    "retry",
                    server.next().await,
                    &format!("fixture-private-upstream-retry-{attempt}"),
                    "fixture-private-oneapi",
                );
            }
            assert!(server.requests.try_recv().is_err());
        }
    });
}
