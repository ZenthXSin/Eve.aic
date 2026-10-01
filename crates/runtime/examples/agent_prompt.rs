//! 明确选择 AGENT.md，并实际运行离线模型与插件工具往返。
use eve_agent_prompt::FileAgentPrompt;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::*;
use eve_plugin_api::{PluginId, PluginManifest, ServiceId};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

#[path = "support/llm_services.rs"]
mod llm_services;
use llm_services::{CONTEXT, OWNER, ServicePlugin, TOOL};

struct CheckingProvider {
    prefix: String,
    requests: AtomicUsize,
}

impl LlmProvider for CheckingProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            request.validate()?;
            if request.messages[0].role != ChatRole::System
                || request.messages[0].text.as_deref() != Some(self.prefix.as_str())
                || request.tools.len() != 1
                || request.tools[0].name != "echo"
            {
                return Err(LlmError::Protocol("身份前缀或工具定义不符".into()));
            }
            match self.requests.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "agent-echo".into(),
                        name: "echo".into(),
                        arguments: json!({"text":"AGENT 文件工具往返"}),
                    }],
                }),
                1 if request.messages.last().is_some_and(|message| {
                    message.role == ChatRole::Tool
                        && message.tool_results.len() == 1
                        && matches!(&message.tool_results[0].output, ToolOutput::Success(value) if value["echo"] == "AGENT 文件工具往返")
                }) => Ok(ModelResponse::Final { text: "AGENT 文件与工具往返验证完成".into() }),
                _ => Err(LlmError::Protocol("工具结果或模型请求次数不符".into())),
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().map(std::path::PathBuf::from).unwrap_or_else(|| {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../AGENT.md"))
    });
    if args.next().is_some() {
        return Err("用法：agent_prompt [明确选择的 AGENT.md 路径]".into());
    }
    // 文件校验失败时，在 Provider 和插件启动之前结束。
    let config = LlmHostConfig::default().with_prompt_source(&FileAgentPrompt::new(path)?)?;
    let provider = Arc::new(CheckingProvider {
        prefix: format!("{}\noutput format: {}", config.system_prompt, config.output_format),
        requests: AtomicUsize::new(0),
    });
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let owner = PluginId::new(OWNER)?;
    kernel.register(Box::new(ServicePlugin { manifest: PluginManifest::new(OWNER, "0.1.0")? }))?;
    let result = async {
        kernel.start(&owner).await?;
        let host = LlmHost::new(
            provider.clone(), registry, kernel.clone(), permissions,
            ContextBinding { service_id: ServiceId::new(CONTEXT)?, expected_owner: owner.clone() },
            vec![ToolBinding { name: "echo".into(), service_id: ServiceId::new(TOOL)?, expected_owner: owner }],
            config,
        )?;
        let metadata = host.system_prompt_metadata().ok_or("缺少提示词来源")?;
        let output = host.run_turn(TurnInput { text: "执行一次 echo 并报告结果".into() }).await.map_err(|failure| failure.error)?;
        if output.diagnostics.provider_requests != 2 || output.diagnostics.started_tools != 1 {
            return Err("模型和工具往返次数不符".into());
        }
        println!("{}", json!({
            "source": "file",
            "revision": metadata.revision,
            "prompt_bytes": metadata.bytes,
            "provider_requests": output.diagnostics.provider_requests,
            "started_tools": output.diagnostics.started_tools,
            "reply": output.text,
        }));
        Ok::<_, Box<dyn std::error::Error>>(())
    }.await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    result?;
    stopped?;
    flushed?;
    Ok(())
}
