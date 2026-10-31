//! 对话内的只读模型配置与准入观测；不解析凭据，不构造或探测 Provider。
use crate::{config, models, qq_model::Commands};
use eve_llm_api::{
    Tool, ToolCall, ToolDefinition, ToolExecutionContext, ToolExecutionError, ToolFuture,
    ToolValidationError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

pub(crate) struct StatusTool(pub Arc<OnceLock<Arc<Commands>>>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default = "default_purpose")]
    purpose: String,
}
fn default_purpose() -> String {
    "dialogue".into()
}
fn args(value: &Value) -> Result<String, ToolValidationError> {
    let invalid = || ToolValidationError {
        message: "purpose 必须为已登记的主/辅模型用途；不接受身份、地址或密钥参数".into(),
    };
    let args: Args = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    let purpose = args.purpose;
    if !config::MODEL_USES.contains(&purpose.as_str()) {
        return Err(invalid());
    }
    Ok(purpose)
}
fn unavailable() -> ToolExecutionError {
    ToolExecutionError::Failed("模型配置或准入观测不可用；未发送探测请求，也未修改配置".into())
}
impl Tool for StatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect_model_runtime".into(),
            description: "只读核对 Eve 某个主/辅模型用途的当前配置及共享请求准入状态，默认 dialogue。排查模型选择、输出限制或排队时使用；普通成员也可查询。不请求模型、不读密钥、不改配置。配置只代表下一次解析，在途请求仍可能使用旧模型；空观测不等于零，多个用途的同角色计数不可相加。此结果不证明供应商可用或提供计费数据。Jev/embedding 沿独立契约，不在此查询。".into(),
            argument_schema: json!({"type":"object","properties":{"purpose":{"type":"string","enum":config::MODEL_USES}},"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: None,
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        args(value).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let purpose = args(&call.arguments)
                .map_err(|error| ToolExecutionError::Rejected(error.message))?;
            if context.is_cancel_requested()
                || context
                    .deadline()
                    .is_some_and(|at| at <= std::time::Instant::now())
            {
                return Err(ToolExecutionError::Cancelled(
                    "调用已停止，未读取模型配置".into(),
                ));
            }
            if context.scope().is_none() {
                return Err(ToolExecutionError::Rejected("缺少可信平台身份".into()));
            }
            let service = self.0.get().ok_or_else(unavailable)?;
            let summary = models::summary(service.settings.as_ref(), Some(&purpose))
                .map_err(|_| unavailable())?;
            let admission = summary
                .role
                .map(|role| crate::model_concurrency::status(&service.settings, role))
                .transpose()
                .map_err(|_| unavailable())?
                .flatten();
            // 配置读取与观测不是同一事务：分别给修订，不把旧准入当作新配置已生效。
            Ok(json!({
                "purpose":purpose,
                "configured":{
                    "revision":summary.config_revision,"role":summary.role.map(|role| role.key()),
                    "model":summary.model,"protocol":summary.protocol,
                    "max_output_tokens":summary.max_output_tokens,
                    "max_concurrent_requests":summary.max_concurrent_requests,
                    "applies_to":"next_model_resolution"
                },
                "admission":{
                    "revision":admission.map(|state| state.revision),
                    "limit":admission.map(|state| state.limit),
                    "active":admission.map(|state| state.active),
                    "waiting":admission.map(|state| state.waiting),
                    "scope":"process_configuration_and_role"
                },
                "inflight_model_known":false,
                "provider_availability_verified":false,
                "configuration_written":false
            }))
        })
    }
}
