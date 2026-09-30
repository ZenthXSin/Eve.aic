use eve_llm_api::*;
use eve_plugin_api::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub const OWNER: &str = "example.session.services";
pub const CONTEXT: &str = "example.session.context";
pub const TOOL: &str = "example.session.tool";
struct Context;
impl ContextAssembler for Context {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "session-demo-1".into(),
                profile: "示例用户".into(),
                memories: vec!["偏好中文".into()],
                history: vec![],
            })
        })
    }
}
struct ReceiptTool(Arc<AtomicUsize>);
impl Tool for ReceiptTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "receipt".into(),
            description: "生成本地回执".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.get("text").is_some_and(Value::is_string) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "text 必须是字符串".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(json!({"receipt": call.arguments["text"]})) })
    }
}
pub struct ServicesPlugin {
    pub manifest: PluginManifest,
    pub executions: Arc<AtomicUsize>,
}
impl Plugin for ServicesPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let executions = self.executions.clone();
        Box::pin(async move {
            ctx.provide_service(ServiceId::new(CONTEXT)?, ContextService(Arc::new(Context)))?;
            ctx.provide_service(
                ServiceId::new(TOOL)?,
                ToolService(Arc::new(ReceiptTool(executions))),
            )?;
            Ok(None)
        })
    }
}
