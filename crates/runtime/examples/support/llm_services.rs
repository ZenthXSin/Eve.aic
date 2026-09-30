use eve_llm_api::{
    ContextAssembler, ContextService, ContextSnapshot, LlmFuture, Tool, ToolCall, ToolConcurrency,
    ToolDefinition, ToolExecutionContext, ToolFuture, ToolService, ToolValidationError, TurnInput,
};
use eve_plugin_api::{Cleanup, Plugin, PluginContext, PluginFuture, PluginManifest, ServiceId};
use serde_json::json;
use std::sync::Arc;

pub const OWNER: &str = "example.llm.owner";
pub const CONTEXT: &str = "example.llm.context";
pub const TOOL: &str = "example.llm.tool.echo";

struct ExampleContext;

impl ContextAssembler for ExampleContext {
    fn assemble(&self, _input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "demo-1".into(),
                profile: "示例用户".into(),
                memories: vec!["偏好中文回答".into()],
                history: vec![],
            })
        })
    }
}

struct EchoTool;

impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "回显输入文本".into(),
            argument_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }

    fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolValidationError> {
        arguments
            .get("text")
            .filter(|text| text.is_string())
            .map(|_| ())
            .ok_or_else(|| ToolValidationError {
                message: "text 必须是字符串".into(),
            })
    }

    fn execute(&self, call: ToolCall, _context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move { Ok(json!({"echo": call.arguments["text"].clone()})) })
    }
}

pub struct ServicePlugin {
    pub manifest: PluginManifest,
}

impl Plugin for ServicePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            context.provide_service(
                ServiceId::new(CONTEXT).expect("有效服务 ID"),
                ContextService(Arc::new(ExampleContext)),
            )?;
            context.provide_service(
                ServiceId::new(TOOL).expect("有效服务 ID"),
                ToolService(Arc::new(EchoTool)),
            )?;
            Ok(None)
        })
    }
}
