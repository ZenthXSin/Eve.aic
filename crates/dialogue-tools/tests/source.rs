use eve_dialogue_tools::{CapabilitiesContext, Services};
use eve_knowledge_api::{BoxFuture, FetchFailure, FetchedPage, SourceFetcher, SourcePolicy};
use eve_llm_api::{
    ContextAssembler, ContextScope, ContextSnapshot, LlmFuture, Tool, ToolCall,
    ToolExecutionContext, ToolExecutionError, TurnInput,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Fetcher {
    calls: Arc<AtomicUsize>,
    foreign: bool,
}
impl SourceFetcher for Fetcher {
    fn fetch<'a>(
        &'a self,
        _: &'a SourcePolicy,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(FetchedPage {
                final_url: if self.foreign {
                    "https://foreign.invalid/private".into()
                } else {
                    url.into()
                },
                content_type: "text/plain".into(),
                sha256: "a".repeat(64),
                byte_count: 18000,
                title: "资料".into(),
                text: "知识".repeat(1365),
                text_truncated: true,
                links: vec![],
            })
        })
    }
}
fn context() -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: "session-1".into(),
        user_id: "user-1".into(),
    }))
}
fn tool(foreign: bool) -> (Arc<dyn Tool>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let services = Services {
        source: Some((
            SourcePolicy::new(&["https://docs.invalid/approved/index".into()]).unwrap(),
            Arc::new(Fetcher {
                calls: calls.clone(),
                foreign,
            }),
        )),
        ..Services::default()
    };
    (services.tools().0.remove(0), calls)
}
fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "read_source".into(),
        arguments,
    }
}

struct FailedFetcher {
    calls: Arc<AtomicUsize>,
    failure: FetchFailure,
}
impl SourceFetcher for FailedFetcher {
    fn fetch<'a>(
        &'a self,
        _: &'a SourcePolicy,
        _: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Err(self.failure) })
    }
}

#[tokio::test]
async fn returned_source_transport_failure_is_unavailable_without_retry() {
    for failure in [
        FetchFailure::Timeout,
        FetchFailure::Network,
        FetchFailure::HttpStatus(503),
        FetchFailure::Empty,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let services = Services {
            source: Some((
                SourcePolicy::unrestricted_web(),
                Arc::new(FailedFetcher {
                    calls: calls.clone(),
                    failure,
                }),
            )),
            ..Services::default()
        };
        let tool = services.tools().0.remove(0);
        let result = tool
            .execute(call(json!({"url":"https://docs.invalid/page"})), context())
            .await
            .unwrap_err();
        let message = format!("来源页面读取失败（{failure:?}）；未自动重试");
        let expected = match failure {
            FetchFailure::Timeout | FetchFailure::Network => {
                ToolExecutionError::Unavailable(message)
            }
            _ => ToolExecutionError::Failed(message),
        };
        assert_eq!(result, expected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn scope_extra_identity_and_out_of_policy_urls_never_fetch() {
    assert!(Services::default().tools().0.is_empty());
    let (tool, calls) = tool(false);
    assert!(
        tool.execute(
            call(json!({"url":"https://docs.invalid/approved/index"})),
            ToolExecutionContext::new()
        )
        .await
        .is_err()
    );
    assert!(
        tool.execute(
            call(json!({"url":"https://docs.invalid/approved/index","owner":"other-user"})),
            context()
        )
        .await
        .is_err()
    );
    assert!(
        tool.execute(
            call(json!({"url":"https://docs.invalid/private"})),
            context()
        )
        .await
        .is_err()
    );
    let ctx = context();
    ctx.cancellation_handle().cancel();
    assert!(
        tool.execute(
            call(json!({"url":"https://docs.invalid/approved/index"})),
            ctx
        )
        .await
        .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn fetched_contract_is_checked_and_utf8_preview_is_explicit() {
    let (tool, calls) = tool(false);
    let result = tool
        .execute(
            call(json!({"url":"https://docs.invalid/approved/index"})),
            context(),
        )
        .await
        .unwrap();
    assert!(result["text"].as_str().unwrap().len() <= 8192);
    assert_eq!(result["text_truncated"], true);
    assert_eq!(result["source_kind"], "external_data");
    assert_eq!(result["persisted_knowledge"], false);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (foreign, _) = self::tool(true);
    assert!(
        foreign
            .execute(
                call(json!({"url":"https://docs.invalid/approved/index"})),
                context()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn open_web_tool_reads_a_host_outside_every_research_seed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let services = Services {
        source: Some((
            SourcePolicy::unrestricted_web(),
            Arc::new(Fetcher {
                calls: calls.clone(),
                foreign: false,
            }),
        )),
        ..Services::default()
    };
    let tool = services.tools().0.remove(0);
    assert!(tool.definition().description.contains("不设站点白名单"));
    let result = tool
        .execute(
            call(json!({"url":"https://another.example/arbitrary/page"})),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(result["url"], "https://another.example/arbitrary/page");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct FixedContext;
impl ContextAssembler for FixedContext {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "fixed-1".into(),
                pinned: vec!["existing host fact".into()],
                ..ContextSnapshot::default()
            })
        })
    }
}

#[tokio::test]
async fn capability_names_remain_discoverable_without_duplicating_full_tool_definitions() {
    let (read_source, _) = tool(false);
    let mut source = read_source.definition();
    source
        .description
        .push_str(&"long tool documentation".repeat(1000));
    let mut recall = source.clone();
    recall.name = "recall_group_messages".into();
    let definitions = vec![source, recall];
    let original_definitions = definitions.clone();
    let seeds = vec!["https://docs.invalid/approved/index".to_string()];
    let context = CapabilitiesContext {
        inner: Arc::new(FixedContext),
        definitions,
        source_seeds: seeds.clone(),
        runner: None,
    };

    let snapshot = context
        .assemble(TurnInput {
            text: "what can you do?".into(),
        })
        .await
        .unwrap();
    assert_eq!(snapshot.pinned[0], "existing host fact");
    assert_eq!(snapshot.pinned.len(), 2);
    let notice: Value = serde_json::from_str(&snapshot.pinned[1]).unwrap();
    assert_eq!(notice["kind"], "eve.host.capabilities");
    assert_eq!(
        notice["conversation_tools"],
        json!([
            {"name": "read_source"},
            {"name": "recall_group_messages"},
        ])
    );
    assert_eq!(notice["research_entry_pages"], json!(seeds));
    assert_eq!(notice["group_history"]["automatic_recent_raw_messages"], 20);
    assert_eq!(
        context.definitions, original_definitions,
        "request definitions retain complete descriptions and argument schemas"
    );
    assert!(
        snapshot.pinned[1].len() < 2048,
        "host facts stay small regardless of complete tool documentation size"
    );
}
