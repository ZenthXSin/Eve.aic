//! 最小可运行的中文 Mock：插件提供上下文和工具，宿主完成一次工具调用闭环。

use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ContextAssembler, ContextService, ContextSnapshot, LlmFuture, LlmProvider, ModelRequest,
    ModelResponse, Tool, ToolBinding, ToolCall, ToolConcurrency, ToolDefinition,
    ToolExecutionContext, ToolFuture, ToolService, ToolValidationError, TurnInput,
};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginFuture, PluginId, PluginManifest, ServiceId,
};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use serde_json::json;
use std::sync::{Arc, Mutex};

const OWNER: &str = "example.llm.owner";
const CONTEXT: &str = "example.llm.context";
const TOOL: &str = "example.llm.tool.echo";

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

struct MockProvider {
    requests: Mutex<usize>,
}

impl LlmProvider for MockProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        let mut requests = self.requests.lock().expect("请求计数锁中毒");
        *requests += 1;
        let round = *requests;
        Box::pin(async move {
            if round == 1 {
                Ok(ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "demo-call-1".into(),
                        name: "echo".into(),
                        arguments: json!({"text": "你好，Eve.aic"}),
                    }],
                })
            } else {
                let result = request
                    .messages
                    .last()
                    .and_then(|message| message.tool_results.first())
                    .map(|result| format!("工具结果：{:?}", result.output))
                    .unwrap_or_else(|| "没有工具结果".into());
                Ok(ModelResponse::Final { text: result })
            }
        })
    }
}

struct ServicePlugin {
    manifest: PluginManifest,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let owner = PluginId::new(OWNER)?;
    kernel.register(Box::new(ServicePlugin {
        manifest: PluginManifest::new(OWNER, "0.1.0")?,
    }))?;
    kernel.start(&owner).await?;

    let host = LlmHost::new(
        Arc::new(MockProvider {
            requests: Mutex::new(0),
        }),
        registry,
        kernel.clone(),
        permissions,
        ContextBinding {
            service_id: ServiceId::new(CONTEXT)?,
            expected_owner: owner.clone(),
        },
        vec![ToolBinding {
            name: "echo".into(),
            service_id: ServiceId::new(TOOL)?,
            expected_owner: owner.clone(),
        }],
        LlmHostConfig::default(),
    )?;
    let output = host
        .run_turn(TurnInput {
            text: "请回显问候语".into(),
        })
        .await
        .map_err(|failure| failure.error)?;
    println!("{}", output.text);
    kernel.stop(&owner).await?;
    kernel.unregister(&owner)?;
    Ok(())
}
