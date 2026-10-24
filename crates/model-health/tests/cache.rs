use eve_llm_api::{
    ChatMessage, ChatRole, LlmFuture, LlmProvider, ModelRequest, ModelResponse, ModelUsage,
    ModelUsageObserver, ToolDefinition,
};
use eve_model_health::{HealthBook, PrefixProbe};
use std::sync::Arc;

struct Echo;
impl LlmProvider for Echo {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async { Ok(ModelResponse::Final { text: "好".into() }) })
    }
}

fn request(history: &[&str], tool: &str) -> ModelRequest {
    let mut messages = vec![ChatMessage::text(
        ChatRole::System,
        "固定系统提示".repeat(50),
    )];
    messages.extend(
        history
            .iter()
            .map(|text| ChatMessage::text(ChatRole::User, *text)),
    );
    ModelRequest {
        messages,
        tools: vec![ToolDefinition {
            name: tool.into(),
            description: "工具".into(),
            argument_schema: serde_json::json!({"type":"object"}),
            required_permissions: vec![],
            concurrency: None,
        }],
    }
}

#[tokio::test]
async fn prefix_probe_measures_reuse_and_locates_breaks() {
    let book = Arc::new(HealthBook::new());
    let probe = PrefixProbe::new(Arc::new(Echo), "dialogue", book.clone());
    probe.complete(request(&["一"], "echo")).await.unwrap();
    probe
        .complete(request(&["一", "二"], "echo"))
        .await
        .unwrap();
    let cache = book.snapshot()[0].1.cache.clone();
    assert_eq!(cache.local_requests, 2);
    let (total, reused) = cache.recent_local[1];
    assert!(reused > 0 && reused < total, "追加历史时前缀大部分复用");
    assert_eq!(cache.last_break.unwrap().first_changed_message, Some(2));

    // 改动开头的工具定义，整个前缀失效。
    probe
        .complete(request(&["一", "二"], "other"))
        .await
        .unwrap();
    let cache = book.snapshot()[0].1.cache.clone();
    assert_eq!(cache.recent_local[2].1, 0);
    assert!(cache.last_break.unwrap().tools_changed);

    // 完全相同的请求全部复用，不记为断点。
    probe
        .complete(request(&["一", "二"], "other"))
        .await
        .unwrap();
    let cache = book.snapshot()[0].1.cache.clone();
    assert_eq!(cache.recent_local[3].0, cache.recent_local[3].1);
    assert!(cache.recent_local_ratio().unwrap() > 0);
}

#[test]
fn reported_usage_accumulates_hits_and_counts_unreported() {
    let book = HealthBook::new();
    book.reported(
        "dialogue",
        ModelUsage {
            input_tokens: Some(1000),
            cached_input_tokens: Some(900),
            output_tokens: None,
        },
    );
    book.reported(
        "dialogue",
        ModelUsage {
            input_tokens: Some(1000),
            cached_input_tokens: Some(1500),
            output_tokens: None,
        },
    );
    book.reported(
        "dialogue",
        ModelUsage {
            input_tokens: Some(1000),
            cached_input_tokens: None,
            output_tokens: None,
        },
    );
    let cache = book.snapshot()[0].1.cache.clone();
    assert_eq!((cache.reported_requests, cache.unreported_requests), (2, 1));
    assert_eq!(cache.cached_tokens, 1900, "命中数不超过输入");
    assert_eq!(cache.recent_reported_ratio(), Some(9500));
}
