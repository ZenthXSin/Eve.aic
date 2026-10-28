//! 完全本地模拟，不连接 QQ。cargo run -p eve-script-runtime --example local_plugins
use eve_llm_api::{ContextScope, SharedToolSteps, ToolCall, ToolExecutionContext, ToolFuture};
use eve_script_runtime::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

struct LocalDelivery;
impl AuthorizedToolDispatch for LocalDelivery {
    fn preflight_tool(&self, name: &str, context: &ToolExecutionContext) -> Result<()> {
        if name != "qq.send_file" || context.scope().is_none() {
            return Err(eve_llm_api::ToolExecutionError::Rejected("未授权".into()));
        }
        Ok(())
    }
    fn preflight_call(&self, call: &ToolCall, context: &ToolExecutionContext) -> Result<()> {
        self.preflight_tool(&call.name, context)?;
        if call.arguments["path"].as_str().is_none() {
            return Err(eve_llm_api::ToolExecutionError::Rejected(
                "缺少产物路径".into(),
            ));
        }
        Ok(())
    }
    fn dispatch(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.preflight_call(&call, &context)?;
            let text = std::fs::read_to_string(call.arguments["path"].as_str().unwrap())
                .map_err(|e| eve_llm_api::ToolExecutionError::Failed(e.to_string()))?;
            Ok(
                json!({"success":true,"simulated":true,"bytes":text.len(),"delivery_id":"local-only"}),
            )
        })
    }
}
fn runtime_path(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(format!("/usr/bin/{name}")))
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let store = ScriptStore::open(directory.path().join("ledger"))?;
    let owner = ContextScope {
        session_id: "local".into(),
        user_id: "demo".into(),
    };
    let child = store.publish(
        &owner,
        ScriptSpec {
            name: "report".into(),
            language: Language::Javascript,
            source: include_str!("plugins/report.js").into(),
            dependencies: BTreeMap::new(),
            tools: vec!["qq.send_file".into()],
            state_schema: StateSchema {
                version: 1,
                fields: BTreeMap::from([("count".into(), JsonType::Integer)]),
            },
            initial_state: json!({"count":0}),
        },
    )?;
    store.activate(&owner, &child.reference, None)?;
    let main = store.publish(
        &owner,
        ScriptSpec {
            name: "main".into(),
            language: Language::Python,
            source: include_str!("plugins/main.py").into(),
            dependencies: BTreeMap::from([("report".into(), child.reference)]),
            tools: vec![],
            state_schema: StateSchema {
                version: 1,
                fields: BTreeMap::new(),
            },
            initial_state: json!({}),
        },
    )?;
    store.activate(&owner, &main.reference, None)?;
    let runtime = ScriptRuntime::new(
        store.clone(),
        Arc::new(LocalDelivery),
        Bubblewrap::new("/usr/bin/python3", runtime_path("node")),
        directory.path().join("artifacts"),
    );
    let bound = runtime.bind(&owner, &main.reference)?;
    let context = ToolExecutionContext::new()
        .with_scope(Some(owner.clone()))
        .with_shared_steps(SharedToolSteps::new(8)?);
    let before = context.shared_steps().unwrap().remaining()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&runtime.preflight(&bound, &context)?)?
    );
    assert_eq!(before, context.shared_steps().unwrap().remaining()?);
    let receipt = runtime
        .run(
            &bound,
            "demo-call",
            json!({"title":"Python 调用 JS，再调用宿主交付"}),
            context,
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    assert_eq!(receipt.status, ReceiptStatus::Succeeded);
    assert!(
        directory
            .path()
            .join("artifacts/demo-call/report.txt")
            .is_file()
    );
    // 只输出实际证据；本例 LocalDelivery 从未连接 QQ。
    let _: Option<Value> = receipt.result;
    Ok(())
}
