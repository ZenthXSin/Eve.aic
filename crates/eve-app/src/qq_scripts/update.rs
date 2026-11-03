//! 保存、预检及原子选择一个插件版本；不重建宿主或移动其他插件版本。
use super::*;
pub(super) fn tool(service: Arc<ScriptTools>) -> Arc<dyn Tool> {
    Arc::new(Update { service })
}
struct Update {
    service: Arc<ScriptTools>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    spec: ScriptSpec,
    migration: Option<StateMigration>,
}
impl Tool for Update {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name: "update_script_plugin".into(),
            description: "独立热更新脚本插件：一次保存不可变spec、预检依赖图/现有叶子权限/沙箱配置，再原子启用。Python/JavaScript无需编译，Rust仅隔离编译此插件且按内容缓存，不重编或重启Eve。spec格式与save_script_plugin相同；schema变化须显式migration，失败保持原启用版本；运行中的调用及依赖仍绑定旧版本，下次按新version调用。启用仅证明版本选择成功，不证明业务执行成功。返回previous可用activate_script_plugin回退，不重放副作用。".into(),
            argument_schema: json!({"type":"object","properties":{"spec":{"type":"object"},"migration":{"type":"object"}},"required":["spec"],"additionalProperties":false}),
            required_permissions: vec![], concurrency: Some(ToolConcurrency::Serial { scope:"eve.script-runtime".into() }) }
    }
    fn validate_arguments(&self, args: &Value) -> std::result::Result<(), ToolValidationError> {
        let validate = || -> Result<()> {
            if serde_json::to_vec(args).map_err(rejected)?.len() > 1024 * 1024 {
                return Err(rejected("插件参数超限"));
            }
            let args: Arguments = serde_json::from_value(args.clone()).map_err(rejected)?;
            args.spec.validate()
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
                .ok_or_else(|| rejected("热更新需要可信会话身份"))?;
            let previous = optional(self.service.store.selected(owner, &args.spec.name))?;
            let compiled = args.spec.language.is_rust();
            let version = match self
                .service
                .runtime
                .prepare_candidate(args.spec, context.clone())
                .await?
            {
                eve_script_runtime::CandidatePreparation::Ready(version) => version,
                eve_script_runtime::CandidatePreparation::CompilationFailed(report) => {
                    return Ok(
                        json!({"success":false,"hot_updated":false,"executed":false,"previous":previous,"compilation":report}),
                    );
                }
            };
            let bound = self.service.runtime.bind(owner, &version.reference)?;
            let preflight = self.service.runtime.preflight_candidate(&bound, &context)?;
            if context.is_cancel_requested()
                || context
                    .deadline()
                    .is_some_and(|d| std::time::Instant::now() >= d)
            {
                return Err(ToolExecutionError::Cancelled(
                    "插件热更新已取消或到期".into(),
                ));
            }
            self.service
                .store
                .activate(owner, &version.reference, args.migration)?;
            Ok(
                json!({"success":true,"hot_updated":true,"executed":false,"compiled":compiled,
                "host_rebuilt":false,"host_restarted":false,"previous":previous,"version":version.reference,"preflight":preflight}),
            )
        })
    }
}
