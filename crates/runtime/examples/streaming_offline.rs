//! 真实 loopback SSE → 内置宿主 → 插件工具 → 会话提交，无外网和真实凭据。
#[path = "../tests/support/session.rs"]
mod fixture;
#[path = "../../llm-openai/tests/support/mod.rs"]
#[allow(dead_code)]
mod http_support;
#[path = "../../llm-openai/tests/support/sse.rs"]
mod sse;
use eve_llm_api::*;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use eve_runtime::LlmHostConfig;
use fixture::*;
use serde_json::json;
use std::sync::{Arc, Mutex, atomic::Ordering};

#[derive(Default)]
struct Events(Mutex<Vec<&'static str>>);
impl TurnEventSink for Events {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            let kind = match event.kind {
                TurnEventKind::ProviderStarted { .. } => "request",
                TurnEventKind::TextDelta { .. } => "delta",
                TurnEventKind::ResponseCompleted { .. } => "response",
                TurnEventKind::ToolBatchStarted { .. } => "batch",
                TurnEventKind::ToolResult { .. } => "tool",
                TurnEventKind::TurnCompleted { .. } => "completed",
                TurnEventKind::SessionSaved => "saved",
                TurnEventKind::Failed { .. } => "failed",
            };
            if event.turn_id != Some(1) {
                return Err(LlmError::Protocol("事件轮次 ID 不符".into()));
            }
            self.0.lock().unwrap().push(kind);
            Ok(())
        })
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut server = http_support::Server::start(vec![
        sse::reply(&sse::events(vec![sse::call(
            "receipt-id",
            "receipt",
            r#"{"text":"中文回执"}"#,
        )])),
        sse::reply(&sse::events(vec![sse::text("中文回执已完成")])),
    ])
    .await;
    let provider = OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            ..OpenAiConfig::new("fixture")
        },
        "fake-key",
    )?;
    let rig = Rig::new(
        Arc::new(provider),
        Arc::new(FaultStore::default()),
        LlmHostConfig {
            response_mode: ResponseMode::Stream,
            ..LlmHostConfig::default()
        },
    )
    .await;
    let events = Events::default();
    let result = rig
        .host
        .run_turn_with_events(input("one", "生成中文回执"), &events)
        .await;
    let first = server.next().await.body;
    let second = server.next().await.body;
    let output = result?;
    let names = events.0.lock().unwrap().clone();
    let n = first["input"].as_array().ok_or("input 缺失")?.len();
    if names[..5] != ["request", "response", "batch", "tool", "request"]
        || names[names.len() - 3..] != ["response", "completed", "saved"]
        || !names.contains(&"delta")
        || rig.starts.load(Ordering::SeqCst) != 1
        || rig.snapshot("one").history() != output.output.transcript
        || first["stream"] != true
        || second["stream"] != true
        || first["tools"] != second["tools"]
        || first["input"] != json!(second["input"].as_array().unwrap()[..n])
        || second["input"][n]["call_id"] != "receipt-id"
        || second["input"][n + 1]["call_id"] != "receipt-id"
    {
        return Err("SSE、事件、工具回执、缓存前缀或持久化不符".into());
    }
    rig.stop().await;
    println!(
        "{}",
        json!({"ok":true,"turn_id":output.turn_id,"provider_requests":output.output.diagnostics.provider_requests,"tool_executions":rig.starts.load(Ordering::SeqCst),"events":names,"reply":output.output.text})
    );
    Ok(())
}
