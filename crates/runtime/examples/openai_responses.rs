//! 真实 Responses 验收：非敏感配置经配置插件读取，凭据只由宿主环境提供。
use eve_config_api::{
    CONFIG_PLUGIN_ID, CONFIG_SERVICE_ID, ConfigField, ConfigKind, ConfigSchema,
    ConfigServiceHandle, LLM_NAMESPACE, LlmRuntimeConfig, runtime_llm_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ContextAssembler, ContextService, ContextSnapshot, LlmError, LlmFuture, ResponseMode, Tool,
    ToolBinding, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolFuture,
    ToolOutput, ToolService, ToolValidationError, TurnEvent, TurnEventKind, TurnEventSink,
    TurnInput,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginFuture, PluginId, PluginManifest,
    ServiceId,
};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const OWNER: &str = "example.openai";
const CONTEXT: &str = "example.openai.context";
const TOOL: &str = "example.openai.echo";
const NAMESPACE: &str = "provider.openai";

fn openai_schema() -> ConfigSchema {
    let string = |default, environment: &str| {
        let mut field = ConfigField::new(ConfigKind::String, default);
        field.environment = Some(environment.into());
        field
    };
    let integer = |default, maximum, environment: &str| {
        let mut field = ConfigField::new(
            ConfigKind::Integer {
                minimum: Some(1),
                maximum: Some(maximum),
            },
            Some(json!(default)),
        );
        field.environment = Some(environment.into());
        field
    };
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (
                "base_url".into(),
                string(
                    Some(json!("https://api.openai.com/v1")),
                    "EVE_OPENAI_BASE_URL",
                ),
            ),
            ("model".into(), string(None, "EVE_OPENAI_MODEL")),
            (
                "reasoning_effort".into(),
                string(Some(json!("")), "EVE_OPENAI_REASONING_EFFORT"),
            ),
            (
                "timeout_seconds".into(),
                integer(120, 600, "EVE_OPENAI_TIMEOUT_SECONDS"),
            ),
            (
                "max_output_tokens".into(),
                integer(512, 16384, "EVE_OPENAI_MAX_OUTPUT_TOKENS"),
            ),
        ]),
    }
}

struct ExampleContext;
impl ContextAssembler for ExampleContext {
    fn assemble(&self, _input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "openai-smoke-1".into(),
                profile: String::new(),
                memories: vec![],
                history: vec![],
            })
        })
    }
}

struct EchoTool {
    receipt: Arc<Mutex<Option<String>>>,
}
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "回显文本并返回一枚本地生成的 receipt。调用后必须在最终回复保留 receipt。"
                .into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_some_and(|object| {
            object.len() == 1 && object.get("text").is_some_and(Value::is_string)
        }) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "参数必须只包含 text 字符串".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, _context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let receipt = format!(
                "EVE-{:x}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("系统时间有效")
                    .as_nanos()
            );
            *self.receipt.lock().expect("receipt 锁有效") = Some(receipt.clone());
            Ok(json!({"echo":call.arguments["text"],"receipt":receipt}))
        })
    }
}

struct ExamplePlugin {
    manifest: PluginManifest,
    receipt: Arc<Mutex<Option<String>>>,
}
impl Plugin for ExamplePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            context.provide_service(
                ServiceId::new(CONTEXT)?,
                ContextService(Arc::new(ExampleContext)),
            )?;
            context.provide_service(
                ServiceId::new(TOOL)?,
                ToolService(Arc::new(EchoTool {
                    receipt: self.receipt.clone(),
                })),
            )?;
            Ok(None)
        })
    }
}

struct ConsoleEvents(tokio::sync::Mutex<tokio::io::Stderr>);
impl TurnEventSink for ConsoleEvents {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            let message = match event.kind {
                TurnEventKind::TextDelta { text, .. } => text,
                TurnEventKind::ToolBatchStarted { calls } => {
                    format!("已收到 {} 个工具调用。\n", calls.len())
                }
                TurnEventKind::ToolResult { ordinal, .. } => {
                    format!("工具结果 {} 已收集。\n", ordinal + 1)
                }
                TurnEventKind::TurnCompleted { .. } => "\n轮次已生成。\n".into(),
                _ => return Ok(()),
            };
            self.0
                .lock()
                .await
                .write_all(message.as_bytes())
                .await
                .map_err(|_| LlmError::Backend("控制台进度投递失败".into()))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().unwrap_or_else(|| "text".into());
    if mode != "text" && mode != "tool" {
        return Err("首个参数必须为 text 或 tool".into());
    }
    let temporary = tempfile::tempdir()?;
    let directory = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    if args.next().is_some() {
        return Err("参数过多".into());
    }
    let api_key = std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "宿主缺少 EVE_OPENAI_API_KEY")?;
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    kernel.register(Box::new(ConfigPlugin::new(ConfigBootstrap::new(
        directory,
        vec![runtime_llm_schema(), openai_schema()],
    ))?))?;
    let owner = PluginId::new(OWNER)?;
    let mut manifest = PluginManifest::new(OWNER, "0.1.0")?;
    manifest.dependencies.push(PluginDependency {
        id: PluginId::new(CONFIG_PLUGIN_ID)?,
        requirement: Some("^0.1".into()),
    });
    let receipt = Arc::new(Mutex::new(None));
    kernel.register(Box::new(ExamplePlugin {
        manifest,
        receipt: receipt.clone(),
    }))?;
    let result = async {
        kernel.start(&owner).await?;
        let entry = registry.get(&ServiceId::new(CONFIG_SERVICE_ID)?)?.ok_or("配置服务缺失")?;
        let config = entry.value.downcast::<ConfigServiceHandle>().map_err(|_| "配置服务类型错误")?;
        let runtime_request = config.0.begin_request(LLM_NAMESPACE,1)?;
        let runtime = LlmRuntimeConfig::try_from(&config.0.read_request(&runtime_request)?)?;
        let provider_request = config.0.begin_request(NAMESPACE,1)?;
        let snapshot = config.0.read_request(&provider_request)?;
        let base_url:String = snapshot.get("base_url")?;
        let model:String = snapshot.get("model")?;
        let mut options = OpenAiConfig::new(model).with_base_url(&base_url)?;
        options.request_timeout = Duration::from_secs(snapshot.get("timeout_seconds")?);
        options.max_output_tokens = Some(snapshot.get("max_output_tokens")?);
        let effort:String = snapshot.get("reasoning_effort")?;
        options.reasoning_effort = if effort.is_empty() { None } else { Some(effort) };
        let timeout = options.request_timeout;
        let provider = OpenAiProvider::new(options,&api_key)?;
        let tool_mode = mode == "tool";
        let bindings = if tool_mode { vec![ToolBinding { name:"echo".into(), service_id:ServiceId::new(TOOL)?, expected_owner:owner.clone() }] } else { vec![] };
        let host = LlmHost::new(Arc::new(provider),registry.clone(),kernel.clone(),permissions,
            ContextBinding { service_id:ServiceId::new(CONTEXT)?, expected_owner:owner.clone() },bindings,
            LlmHostConfig { response_mode: if runtime.response_mode == "stream" { ResponseMode::Stream } else { ResponseMode::Complete }, provider_timeout:timeout, max_parallel_tool_calls:runtime.max_parallel_tool_calls,
                system_prompt:"你是 Eve 验收助手。严格遵循用户指定的测试步骤。需要调用工具时先只返回函数调用，不要同时输出文本。".into(), ..LlmHostConfig::default() })?;
        let prompt = if tool_mode {
            "请恰好调用一次 echo 工具，text 参数为 Eve 真实工具测试。不要自己生成 receipt。收到工具结果后，最终回复只输出工具返回的 receipt。"
        } else { "不要调用工具，只回复 EVE_TEXT_OK。" };
        let started = std::time::Instant::now();
        let output = host.run_turn_with_events(TurnInput { text:prompt.into() }, &ConsoleEvents(tokio::sync::Mutex::new(tokio::io::stderr()))).await.map_err(|failure| failure.error)?;
        if tool_mode {
            let receipt = receipt.lock().map_err(|_| "receipt 锁失效")?.clone().ok_or("模型未执行工具")?;
            if output.diagnostics.provider_requests != 2 || output.diagnostics.started_tools != 1
                || output.diagnostics.tool_results.len() != 1
                || !matches!(output.diagnostics.tool_results[0].output,ToolOutput::Success(_))
                || !output.text.contains(&receipt)
            { return Err("模型未完成一次真实工具调用及结果回传".into()); }
        } else if output.diagnostics.provider_requests != 1 || !output.text.contains("EVE_TEXT_OK") {
            return Err("模型未完成文本验收".into());
        }
        println!("{}",json!({"mode":mode.to_string_lossy(),"ok":true,"provider_requests":output.diagnostics.provider_requests,
            "started_tools":output.diagnostics.started_tools,"max_parallel_tool_calls":runtime.max_parallel_tool_calls,
            "elapsed_ms":started.elapsed().as_millis(),"text":output.text}));
        Ok::<_,Box<dyn std::error::Error>>(())
    }.await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    result?;
    stopped?;
    flushed?;
    Ok(())
}
