//! 最小可运行的中文 Mock：插件提供上下文和工具，宿主完成一次工具调用闭环。

use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    LlmFuture, LlmProvider, ModelRequest, ModelResponse, ToolBinding, ToolCall, TurnInput,
};
use eve_plugin_api::{PluginId, PluginManifest, ServiceId};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[path = "support/llm_services.rs"]
mod llm_services;
use llm_services::{CONTEXT, OWNER, ServicePlugin, TOOL};

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
