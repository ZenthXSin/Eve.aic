//! 最小上下文与本地 echo 插件；通过公开 Context 发布，不访问 Runtime 内部。
use eve_llm_api::*;
use eve_plugin_api::*;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const OWNER: &str = "eve.app.services";
pub(crate) const CONTEXT: &str = "eve.app.context";
pub(crate) const TOOL: &str = "eve.app.echo";
struct Context;
impl ContextAssembler for Context {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "eve-core-1".into(),
                profile: String::new(),
                memories: vec![],
                history: vec![],
            })
        })
    }
}
struct Echo;
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "在本地回显 text；只返回收到的文本，不访问文件或网络。".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_some_and(|object| {
            object.len() == 1
                && object
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.len() <= 32768)
        }) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "echo 参数必须只包含至多 32768 字节的 text 字符串".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move { Ok(json!({"echo":call.arguments["text"]})) })
    }
}
pub(crate) struct CoreServices {
    manifest: PluginManifest,
}
impl CoreServices {
    pub(crate) fn new() -> PluginResult<Self> {
        let mut manifest = PluginManifest::new(OWNER, env!("CARGO_PKG_VERSION"))?;
        for name in [
            eve_config_api::CONFIG_PLUGIN_ID,
            eve_session_api::SESSION_PLUGIN_ID,
        ] {
            manifest.dependencies.push(PluginDependency {
                id: PluginId::new(name)?,
                requirement: Some("^0.1".into()),
            });
        }
        Ok(Self { manifest })
    }
}
impl Plugin for CoreServices {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            context.provide_service(ServiceId::new(CONTEXT)?, ContextService(Arc::new(Context)))?;
            context.provide_service(ServiceId::new(TOOL)?, ToolService(Arc::new(Echo)))?;
            Ok(None)
        })
    }
}
