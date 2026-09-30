#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
#[path = "../examples/support/llm_services.rs"]
mod llm_services;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{LlmError, ToolBinding, ToolFailureCode, ToolOutput, TurnInput};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use eve_plugin_api::{PluginId, PluginManifest, ServiceId};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use http_support::{Reply, Server, final_response};
use llm_services::{CONTEXT, OWNER, ServicePlugin, TOOL};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

async fn host(url: &str, timeout: Duration) -> (Kernel, LlmHost) {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let owner = PluginId::new(OWNER).unwrap();
    kernel
        .register(Box::new(ServicePlugin {
            manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
        }))
        .unwrap();
    kernel.start(&owner).await.unwrap();
    let provider = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: url.into(),
            ..OpenAiConfig::new("fixture-model")
        },
        "fake-key",
    )
    .unwrap();
    let host = LlmHost::new(
        Arc::new(provider),
        registry,
        kernel.clone(),
        permissions,
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: owner.clone(),
        },
        vec![ToolBinding {
            name: "echo".into(),
            service_id: ServiceId::new(TOOL).unwrap(),
            expected_owner: owner,
        }],
        LlmHostConfig {
            provider_timeout: timeout,
            ..LlmHostConfig::default()
        },
    )
    .unwrap();
    (kernel, host)
}
fn input() -> TurnInput {
    TurnInput {
        text: "请回显".into(),
    }
}
fn tool_response() -> serde_json::Value {
    json!({"status":"completed", "error":null, "output":[
        {"type":"function_call", "call_id":"second", "name":"echo", "arguments":"{\"text\":\"中文\"}"},
        {"type":"function_call", "call_id":"first", "name":"echo", "arguments":"{}"}
    ]})
}
async fn stop(kernel: Kernel) {
    let owner = PluginId::new(OWNER).unwrap();
    tokio::time::timeout(Duration::from_secs(2), kernel.stop(&owner))
        .await
        .unwrap()
        .unwrap();
    kernel.unregister(&owner).unwrap();
}

#[tokio::test]
async fn http_provider_runs_plugin_tool_loop_and_returns_ordered_success_and_failure() {
    let mut server = Server::start(vec![
        Reply::json(tool_response()),
        Reply::json(final_response("已处理")),
    ])
    .await;
    let (kernel, host) = host(&server.url, Duration::from_secs(2)).await;
    let output = host.run_turn(input()).await.unwrap();
    assert_eq!(output.text, "已处理");
    assert_eq!(output.diagnostics.provider_requests, 2);
    assert_eq!(output.diagnostics.started_tools, 1);
    assert_eq!(output.diagnostics.tool_results[0].call_id, "second");
    assert_eq!(output.diagnostics.tool_results[1].call_id, "first");
    assert!(matches!(
        output.diagnostics.tool_results[1].output,
        ToolOutput::Failure {
            code: ToolFailureCode::InvalidArguments,
            ..
        }
    ));
    let captured = server.next().await;
    assert!(captured.headers.starts_with("POST /v1/responses HTTP/1.1"));
    let first = captured.body;
    let second = server.next().await.body;
    assert_eq!(first["tools"], second["tools"]);
    let first_input = first["input"].as_array().unwrap();
    let second_input = second["input"].as_array().unwrap();
    assert_eq!(first_input, &second_input[..first_input.len()]);
    let n = first_input.len();
    assert_eq!(second_input[n]["call_id"], "second");
    assert_eq!(second_input[n + 1]["call_id"], "first");
    assert_eq!(second_input[n + 2]["call_id"], "second");
    assert_eq!(second_input[n + 3]["call_id"], "first");
    let success: serde_json::Value =
        serde_json::from_str(second_input[n + 2]["output"].as_str().unwrap()).unwrap();
    let failure: serde_json::Value =
        serde_json::from_str(second_input[n + 3]["output"].as_str().unwrap()).unwrap();
    assert_eq!(success, json!({"echo":"中文"}));
    assert_eq!(failure["error"]["code"], "InvalidArguments");
    stop(kernel).await;
}

#[tokio::test]
async fn unsupported_output_prevents_tool_execution_and_second_provider_request() {
    let mut response = tool_response();
    response["output"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"reasoning", "summary":[]}));
    let mut server = Server::start(vec![Reply::json(response)]).await;
    let (kernel, host) = host(&server.url, Duration::from_secs(2)).await;
    let failure = host.run_turn(input()).await.unwrap_err();
    assert!(matches!(failure.error, LlmError::Unsupported(_)));
    assert_eq!(failure.diagnostics.started_tools, 0);
    assert_eq!(failure.diagnostics.provider_requests, 1);
    server.next().await;
    assert!(server.requests.try_recv().is_err());
    stop(kernel).await;
}

#[tokio::test]
async fn host_timeout_drops_http_request_and_releases_lifecycle_admission() {
    let mut reply = Reply::json(final_response("过时回复"));
    reply.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![reply]).await;
    let (kernel, host) = host(&server.url, Duration::from_millis(100)).await;
    let failure = host.run_turn(input()).await.unwrap_err();
    assert_eq!(failure.error, LlmError::ProviderTimeout);
    assert_eq!(failure.diagnostics.started_tools, 0);
    server.next().await;
    stop(kernel).await;
}

#[tokio::test]
async fn cancellation_during_http_releases_lifecycle_admission() {
    let mut reply = Reply::json(final_response("过时回复"));
    reply.body_delay = Duration::from_secs(1);
    let mut server = Server::start(vec![reply]).await;
    let (kernel, host) = host(&server.url, Duration::from_secs(3)).await;
    let running = tokio::spawn(async move { host.run_turn(input()).await });
    server.next().await;
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    stop(kernel).await;
    assert!(server.requests.try_recv().is_err());
}
