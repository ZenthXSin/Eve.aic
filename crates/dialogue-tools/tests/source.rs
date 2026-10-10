use eve_dialogue_tools::Services;
use eve_knowledge_api::{BoxFuture, FetchFailure, FetchedPage, SourceFetcher, SourcePolicy};
use eve_llm_api::{ContextScope, Tool, ToolCall, ToolExecutionContext};
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
    (services.tools().remove(0), calls)
}
fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "read_source".into(),
        arguments,
    }
}
#[tokio::test]
async fn scope_extra_identity_and_out_of_policy_urls_never_fetch() {
    assert!(Services::default().tools().is_empty());
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
