//! 32个隔离Rust编译产物与八scope热切换的RSS样本。显式EVE_PROFILE_RUSTC指向可信rustc。
use eve_llm_api::{ContextScope, SharedToolSteps, ToolCall, ToolExecutionContext, ToolFuture};
use eve_script_runtime::*;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
struct Deny;
impl AuthorizedToolDispatch for Deny {
    fn preflight_tool(&self, _: &str, _: &ToolExecutionContext) -> Result<()> {
        Err(eve_llm_api::ToolExecutionError::Rejected(
            "样本不允许宿主工具".into(),
        ))
    }
    fn preflight_call(&self, call: &ToolCall, context: &ToolExecutionContext) -> Result<()> {
        self.preflight_tool(&call.name, context)
    }
    fn dispatch(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async {
            Err(eve_llm_api::ToolExecutionError::Rejected(
                "样本不允许宿主工具".into(),
            ))
        })
    }
}
fn context(i: usize) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: format!("scope-{}", i % 8),
            user_id: format!("owner-{}", i % 8),
        }))
        .with_shared_steps(SharedToolSteps::new(4).unwrap())
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let rustc = std::env::var_os("EVE_PROFILE_RUSTC").ok_or("设置可信EVE_PROFILE_RUSTC")?;
    let root = tempfile::tempdir()?;
    let store = ScriptStore::open(root.path().join("store"))?;
    let runtime = ScriptRuntime::new(
        store.clone(),
        Arc::new(Deny),
        Bubblewrap::new("/usr/bin/python3", "/usr/bin/node").with_rustc(rustc),
        root.path().join("artifacts"),
    );
    for i in 0..32 {
        // 外部测量器监视剩余空间；门控文件只控制本样本停止，不影响运行器。
        if std::env::var_os("EVE_PROFILE_STOP_FILE")
            .is_some_and(|p| std::path::Path::new(&p).exists())
        {
            return Err("磁盘测量保护停止；未完成32产物样本".into());
        }
        let source = format!(
            r####"use std::io;fn main(){{let mut input=String::new();io::stdin().read_line(&mut input).unwrap();println!("{{}}",r###"{{"op":"return","value":{i}}}"###);}}"####
        );
        let version = runtime
            .publish_candidate(
                ScriptSpec {
                    name: "rust-profile".into(),
                    language: Language::Rust,
                    source,
                    dependencies: BTreeMap::new(),
                    tools: vec![],
                    state_schema: StateSchema {
                        version: 1,
                        fields: BTreeMap::new(),
                    },
                    initial_state: json!({}),
                },
                context(i),
            )
            .await?;
        let owner = context(i).scope().unwrap().clone();
        store.activate(&owner, &version.reference, None)?;
        let bound = runtime.bind(&owner, &version.reference)?;
        let receipt = runtime
            .run(&bound, &format!("profile-{i}"), json!({}), context(i))
            .await?;
        assert_eq!(receipt.status, ReceiptStatus::Succeeded);
        assert_eq!(receipt.result, Some(json!(i)));
        let rss = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .map(str::to_owned)
            });
        println!("{}", json!({"compiled":i+1,"rss":rss}));
    }
    Ok(())
}
