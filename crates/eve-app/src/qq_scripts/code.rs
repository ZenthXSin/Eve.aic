use super::*;

pub(super) fn tool(service: Arc<ScriptTools>) -> Arc<dyn Tool> {
    Arc::new(Code { service })
}
struct Code {
    service: Arc<ScriptTools>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    code: String,
    tools: Vec<String>,
    #[serde(default)]
    arguments: Value,
}
impl Tool for Code {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name: "run_tool_code".into(),
            description: "一次执行JavaScript工具编排，减少模型往返。声明tools工具名，用await tools.<name>(参数)调用；独立调用可Promise.all，宿主只并发允许并行的叶子，其余按顺序执行，失败停止。args为arguments，text(value)选择返回模型的结果（合计32KiB），不打印全部中间结果。代码运行在OS沙箱，共享原会话身份、审核、撤权、预算、取消与30秒期限；每个叶子有持久回执。不能调用任务/管理/升级/脚本入口；不能用代码授予权限。await每个工具Promise。重复相同代码复用不可变版本；失败或unknown不能盲目重放，按返回call_id用list_script_plugins核对。".into(),
            argument_schema: json!({"type":"object","properties":{"code":{"type":"string","maxLength":32768},"tools":{"type":"array","maxItems":32,"items":{"type":"string"}},"arguments":{}},"required":["code","tools"],"additionalProperties":false}),
            required_permissions: vec![], concurrency: Some(ToolConcurrency::Serial { scope:"eve.script-runtime".into() }) }
    }
    fn validate_arguments(&self, args: &Value) -> std::result::Result<(), ToolValidationError> {
        let validate = || -> Result<()> {
            let args: Arguments = serde_json::from_value(args.clone()).map_err(rejected)?;
            if args.tools.iter().any(|name| !is_synchronous_leaf(name)) {
                return Err(rejected("代码只能调用已装配的同步叶子"));
            }
            eve_script_runtime::tool_code_spec(&args.code, args.tools)?;
            if serde_json::to_vec(&args.arguments).map_err(rejected)?.len() > 1024 * 1024 {
                return Err(rejected("编排输入超限"));
            }
            Ok(())
        };
        validate().map_err(|e| ToolValidationError {
            message: e.to_string(),
        })
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments).map_err(rejected)?;
            let args: Arguments = serde_json::from_value(call.arguments).map_err(rejected)?;
            let owner = context
                .scope()
                .ok_or_else(|| rejected("代码需要可信会话身份"))?;
            let version = self
                .service
                .runtime
                .publish_candidate(
                    eve_script_runtime::tool_code_spec(&args.code, args.tools)?,
                    context.clone(),
                )
                .await?;
            let bound = self.service.runtime.bind(owner, &version.reference)?;
            self.service.runtime.preflight_candidate(&bound, &context)?;
            self.service
                .store
                .activate(owner, &version.reference, None)?;
            let identity =
                serde_json::to_vec(&(owner, context.turn_id(), &call.id)).map_err(rejected)?;
            let call_id = format!(
                "code-{}",
                ring::digest::digest(&ring::digest::SHA256, &identity)
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let receipt = self
                .service
                .runtime
                .run(&bound, &call_id, args.arguments, context)
                .await?;
            Ok(
                json!({"success":receipt.status==eve_script_runtime::ReceiptStatus::Succeeded,
                "status":receipt.status,"call_id":receipt.call_id,"version":receipt.version,
                "result":receipt.result,"error":receipt.error,"evidence":receipt.evidence}),
            )
        })
    }
}
