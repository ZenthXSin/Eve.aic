use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatMessage, ChatRole, LlmError, LlmProvider, ModelRequest, ModelResponse, ToolBinding,
    ToolOutput, TurnInput,
};
use eve_plugin_api::{PluginId, PluginManifest, ServiceId};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use std::sync::Arc;
use std::time::Duration;
mod llm_services;
use llm_services::{CONTEXT, OWNER, ServicePlugin, TOOL};

pub async fn run(provider: Arc<dyn LlmProvider>) -> Result<(), Box<dyn std::error::Error>> {
    let response = provider
        .complete(ModelRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "请用中文简短问候。")],
            tools: vec![],
        })
        .await?;
    let ModelResponse::Final { text } = response else {
        return Err(LlmError::Protocol("文本 smoke test 应返回最终回复".into()).into());
    };
    println!("文本验收：{text}");

    let services = KernelServices::default();
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let owner = PluginId::new(OWNER)?;
    kernel.register(Box::new(ServicePlugin {
        manifest: PluginManifest::new(OWNER, "0.1.0")?,
    }))?;
    kernel.start(&owner).await?;
    let context_id = ServiceId::new(CONTEXT)?;
    let tool_id = ServiceId::new(TOOL)?;
    let result = async {
        let host = LlmHost::new(
            provider,
            registry,
            kernel.clone(),
            permissions,
            ContextBinding {
                service_id: context_id,
                expected_owner: owner.clone(),
            },
            vec![ToolBinding {
                name: "echo".into(),
                service_id: tool_id,
                expected_owner: owner.clone(),
            }],
            LlmHostConfig {
                system_prompt: "使用 echo 工具回显用户指定文本，取得工具结果后再给出中文回答。"
                    .into(),
                provider_timeout: Duration::from_secs(60),
                ..LlmHostConfig::default()
            },
        )?;
        let output = host
            .run_turn(TurnInput {
                text: "请调用 echo 回显：你好，Eve.aic。".into(),
            })
            .await
            .map_err(|failure| failure.error)?;
        if output.diagnostics.started_tools != 1
            || output.diagnostics.provider_requests != 2
            || output.diagnostics.tool_results.len() != 1
            || !matches!(
                output.diagnostics.tool_results[0].output,
                ToolOutput::Success(_)
            )
        {
            return Err(LlmError::Protocol(
                "模型未完成一次工具调用，smoke test 不通过".into(),
            ));
        }
        println!(
            "工具验收：{}；请求次数：{}；工具次数：{}。",
            output.text, output.diagnostics.provider_requests, output.diagnostics.started_tools
        );
        Ok::<(), LlmError>(())
    }
    .await;
    // Provider 或协议失败也必须停止插件；保留业务原始错误。
    let stopped = kernel.stop(&owner).await;
    result?;
    stopped?;
    kernel.unregister(&owner)?;
    Ok(())
}
