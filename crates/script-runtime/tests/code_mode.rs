#[cfg(target_os = "linux")]
use eve_llm_api::{
    ContextScope, SharedToolSteps, ToolCall, ToolExecutionContext, ToolExecutionError, ToolFuture,
};
use eve_script_runtime::*;
#[cfg(target_os = "linux")]
use serde_json::json;
#[cfg(target_os = "linux")]
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(target_os = "linux")]
fn sandbox() -> Bubblewrap {
    fn path(name: &str) -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join(name))
            .find(|p| p.is_file())
            .unwrap()
            .canonicalize()
            .unwrap()
    }
    let mut sandbox = Bubblewrap::new(path("python3"), path("node"));
    sandbox.bwrap = path("bwrap");
    sandbox
}
#[cfg(target_os = "linux")]
fn owner() -> ContextScope {
    ContextScope {
        session_id: "code-session".into(),
        user_id: "owner".into(),
    }
}
#[cfg(target_os = "linux")]
fn context(limit: usize) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(owner()))
        .with_shared_steps(SharedToolSteps::new(limit).unwrap())
        .with_call_id("outer".into())
}
#[derive(Default)]
#[cfg(target_os = "linux")]
struct Dispatch {
    calls: Mutex<Vec<String>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}
#[cfg(target_os = "linux")]
impl AuthorizedToolDispatch for Dispatch {
    fn parallel_safe(&self, name: &str) -> bool {
        name == "read"
    }
    fn preflight_tool(&self, name: &str, ctx: &ToolExecutionContext) -> Result<()> {
        if !["read", "write"].contains(&name) || ctx.scope() != Some(&owner()) {
            return Err(ToolExecutionError::Rejected("denied".into()));
        }
        Ok(())
    }
    fn preflight_call(&self, call: &ToolCall, ctx: &ToolExecutionContext) -> Result<()> {
        self.preflight_tool(&call.name, ctx)
    }
    fn dispatch(&self, call: ToolCall, ctx: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.preflight_call(&call, &ctx)?;
            assert_eq!(ctx.call_id(), Some(call.id.as_str()));
            self.calls.lock().unwrap().push(call.id);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(
                json!({"success":call.arguments["fail"]!=true,"number":call.arguments["number"],"large_intermediate":"x".repeat(100000)}),
            )
        })
    }
}
#[cfg(target_os = "linux")]
async fn run(code: &str, tools: Vec<String>, limit: usize) -> (Receipt, Arc<Dispatch>, usize) {
    let root = tempfile::tempdir().unwrap();
    let store = ScriptStore::open(root.path().join("store")).unwrap();
    let dispatch = Arc::new(Dispatch::default());
    let runtime = ScriptRuntime::new(
        store.clone(),
        dispatch.clone(),
        sandbox(),
        root.path().join("artifacts"),
    );
    let spec = tool_code_spec(code, tools).unwrap();
    let saved = store.publish(&owner(), spec).unwrap();
    store.activate(&owner(), &saved.reference, None).unwrap();
    let bound = runtime.bind(&owner(), &saved.reference).unwrap();
    let ctx = context(limit);
    let receipt = runtime
        .run(&bound, "code-test", json!({}), ctx.clone())
        .await
        .unwrap();
    assert_eq!(store.receipt(&owner(), "code-test").unwrap(), receipt);
    let remaining = ctx.shared_steps().unwrap().remaining().unwrap();
    (receipt, dispatch, remaining)
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn code_parallel_reads_share_budget_identity_and_return_only_selected_output() {
    let (receipt, dispatch, remaining) = run("const values=await Promise.all([tools.read({number:2}),tools.read({number:3})]); text(values.map(x=>x.number));", vec!["read".into()], 3).await;
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
    assert_eq!(receipt.result, Some(json!({"output":[[2,3]]})));
    assert_eq!(receipt.evidence.len(), 2);
    assert_eq!(dispatch.peak.load(Ordering::SeqCst), 2);
    assert_eq!(remaining, 0);
    let calls = dispatch.calls.lock().unwrap();
    assert_ne!(calls[0], calls[1]);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn code_serial_failure_stops_later_writes_and_budget_rejection_executes_nothing() {
    let (receipt, dispatch, _) = run("await Promise.all([tools.write({fail:true}),tools.write({number:2})]); text('should not pass');", vec!["write".into()], 3).await;
    assert_eq!(receipt.status, ReceiptStatus::Failed, "{receipt:?}");
    assert_eq!(dispatch.calls.lock().unwrap().len(), 1);
    assert_eq!(receipt.evidence[1].operation, "tool_not_started");
    let (receipt, dispatch, remaining) = run(
        "await Promise.all([tools.read({}),tools.read({})]);",
        vec!["read".into()],
        2,
    )
    .await;
    assert_eq!(receipt.status, ReceiptStatus::Rejected);
    assert!(dispatch.calls.lock().unwrap().is_empty());
    assert_eq!(remaining, 1);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn code_intermediate_output_cannot_overflow_and_no_leaf_is_implicit() {
    let (receipt, dispatch, _) = run("text('x'.repeat(33000));", vec![], 1).await;
    assert_ne!(receipt.status, ReceiptStatus::Succeeded);
    assert!(dispatch.calls.lock().unwrap().is_empty());
    let (receipt, dispatch, _) = run("await tools.read({});", vec![], 1).await;
    assert_ne!(receipt.status, ReceiptStatus::Succeeded);
    assert!(dispatch.calls.lock().unwrap().is_empty());
}

#[test]
fn code_versions_are_stable_and_input_changes_do_not_require_new_source() {
    let a = tool_code_spec("text(args.number);", vec![]).unwrap();
    let b = tool_code_spec("text(args.number);", vec![]).unwrap();
    assert_eq!(a, b);
    assert_ne!(
        a.name,
        tool_code_spec("text(args.other);", vec![]).unwrap().name
    );
    assert!(tool_code_spec("", vec![]).is_err());
}
