//! 适配器切换的用户入口；可信身份沿已有装配，跨命名空间只提交一次 CAS。
use crate::{config, qq_model::Commands};
use eve_config_api::{
    ApplyMode, ConfigError, MODELS_NAMESPACE, MODELS_SCHEMA_VERSION, ModelRole, ModelRolesConfig,
    NamespaceValues,
};
use eve_llm_api::{
    CredentialPurpose, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolValidationError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

pub(crate) const HELP: &str = "切换模型适配器：/model adapter primary|auxiliary_small openai|codex chat|responses [API根地址]。角色须已启用；Codex 只支持 responses；省略地址保留现有设置。";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdapterArgs {
    role: ModelRole,
    adapter: String,
    protocol: String,
    base_url: Option<String>,
}

fn args(value: &Value) -> Result<AdapterArgs, ToolValidationError> {
    let invalid = |message: &str| ToolValidationError {
        message: message.into(),
    };
    let args: AdapterArgs = serde_json::from_value(value.clone()).map_err(|_| invalid(HELP))?;
    if !matches!(args.role, ModelRole::Primary | ModelRole::AuxiliarySmall)
        || !matches!(args.adapter.as_str(), "openai" | "codex")
        || !matches!(args.protocol.as_str(), "chat" | "responses")
    {
        return Err(invalid(HELP));
    }
    if args.adapter == "codex" && args.protocol != "responses" {
        return Err(invalid("Codex 原适配器只支持 responses；未修改配置。"));
    }
    if let Some(base) = &args.base_url {
        if base.is_empty()
            || base.len() > 2048
            || base.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(invalid("API 地址必须是有效的非空 HTTP(S) 地址。"));
        }
        let url = reqwest::Url::parse(base)
            .map_err(|_| invalid("API 地址必须是有效的 HTTP(S) 地址。"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid("API 地址不能包含用户信息、查询或片段。"));
        }
    }
    Ok(args)
}

pub(crate) fn command_args(words: &[&str]) -> Result<AdapterArgs, ToolValidationError> {
    if !(3..=4).contains(&words.len()) {
        return Err(ToolValidationError {
            message: HELP.into(),
        });
    }
    let mut value = json!({"role":words[0],"adapter":words[1],"protocol":words[2]});
    if let Some(base) = words.get(3) {
        value["base_url"] = json!(base);
    }
    args(&value)
}

fn unavailable() -> ToolExecutionError {
    ToolExecutionError::Failed("模型配置暂不可用，未开始切换适配器。".into())
}

fn stale() -> ToolExecutionError {
    ToolExecutionError::Rejected("配置已被其他操作修改，本次未覆盖；请重新核对。".into())
}

impl Commands {
    pub(crate) fn switch_adapter(&self, args: AdapterArgs) -> Result<Value, ToolExecutionError> {
        let mut document = self.admin.current().map_err(|_| unavailable())?;
        let mut provider = self
            .settings
            .snapshot(config::OPENAI_NAMESPACE, 1)
            .map_err(|_| unavailable())?;
        let roles = self
            .settings
            .snapshot(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION)
            .map_err(|_| unavailable())?;
        if provider.revision != document.revision || roles.revision != document.revision {
            return Err(stale());
        }
        let roles = ModelRolesConfig::try_from(&roles).map_err(|_| unavailable())?;
        let mut profile = roles
            .require(args.role)
            .map_err(|_| {
                ToolExecutionError::Rejected(
                    "请先启用并配置对应模型角色，再切换模型适配器。".into(),
                )
            })?
            .clone();
        let previous_adapter = profile.provider.clone();
        profile.provider = args.adapter.clone();
        let auxiliary = args.role == ModelRole::AuxiliarySmall;
        let protocol_field = if auxiliary {
            "auxiliary_protocol"
        } else {
            "protocol"
        };
        let base_field = if auxiliary {
            "auxiliary_base_url"
        } else {
            "base_url"
        };
        let mut changes = vec![
            (
                MODELS_NAMESPACE,
                args.role.field("provider"),
                json!(args.adapter),
            ),
            (
                config::OPENAI_NAMESPACE,
                protocol_field.into(),
                json!(args.protocol),
            ),
        ];
        if !auxiliary {
            changes.push((
                config::OPENAI_NAMESPACE,
                "model_role".into(),
                json!("primary"),
            ));
        }
        if let Some(base) = &args.base_url {
            // 地址只来自这次可信管理员操作，不推断新 host 或探测备用地址。
            // 辅助独立地址仍须显式辅助/匿名引用，由共同配置入口检查。
            changes.push((config::OPENAI_NAMESPACE, base_field.into(), json!(base)));
        }
        for (namespace, field, value) in &changes {
            if *namespace == config::OPENAI_NAMESPACE {
                provider.values.insert(field.clone(), value.clone());
            }
        }
        let purpose = if auxiliary {
            CredentialPurpose::Auxiliary
        } else {
            CredentialPurpose::Primary
        };
        let candidate = config::provider_config_for(&provider, Some(&profile), purpose)
            .map_err(|error| ToolExecutionError::Rejected(error.to_string()))?;
        // 同时检查 OpenAI 分支的实际地址边界，不创建客户端或读取密钥。
        let url = reqwest::Url::parse(candidate.endpoint()).map_err(|_| unavailable())?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        let anonymous = auxiliary
            && profile.credential_ref.as_deref() == Some(crate::model_credentials::ANONYMOUS);
        if !(url.scheme() == "https" || url.scheme() == "http" && (loopback || anonymous))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ToolExecutionError::Rejected(
                "模型地址必须沿现有凭据传输约束，使用 HTTPS 或受支持的本地 HTTP。".into(),
            ));
        }
        for (namespace, field, value) in changes {
            document
                .namespaces
                .entry(namespace.into())
                .or_insert_with(|| NamespaceValues {
                    schema_version: 1,
                    values: BTreeMap::new(),
                })
                .values
                .insert(field, value);
        }
        let change = self
            .admin
            .replace(
                document.revision,
                document.namespaces,
                ApplyMode::NewRequests,
            )
            .map_err(|error| match error {
                ConfigError::RevisionConflict { .. } => stale(),
                ConfigError::Storage(_) | ConfigError::Unavailable => ToolExecutionError::Failed(
                    "适配器配置保存未能确认；请读取配置核对，不声称修改成功。".into(),
                ),
                _ => ToolExecutionError::Rejected(
                    "配置组合无效，未修改配置；请核对所选角色及共用该协议的用途。".into(),
                ),
            })?;
        Ok(json!({
            "saved":true,"revision":change.revision,"role":args.role.key(),
            "previous_adapter":previous_adapter,"adapter":args.adapter,
            "protocol":args.protocol,"model":candidate.model(),
            "effective":"next_model_resolution","inflight_unchanged":true,
            "provider_availability_verified":false
        }))
    }
}

pub(crate) struct AdapterTool(pub Arc<OnceLock<Arc<Commands>>>);

impl Tool for AdapterTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "set_model_adapter".into(),
            description: "切换 Eve 已启用主/辅角色的模型适配器，一次保存实现、协议和可选 API 地址。primary 或 auxiliary_small 可选 openai（chat/responses）与 codex（仅 responses）。仅在主人或管理员要求切换时使用，身份由宿主核验；不接受身份或密钥参数。省略 base_url 保留地址，地址变更沿已有凭据引用；辅助独立地址须先配置辅助或匿名引用。主角色同时激活 primary，辅助角色不改变用途路由。模型名、思考强度与输出限制保持；新一轮生效，在途请求保留已选配置。保存不探测模型，不能据回执宣称上游可用。".into(),
            argument_schema: json!({"type":"object","properties":{
                "role":{"type":"string","enum":["primary","auxiliary_small"]},
                "adapter":{"type":"string","enum":["openai","codex"]},
                "protocol":{"type":"string","enum":["chat","responses"]},
                "base_url":{"type":"string","minLength":1,"maxLength":2048}
            },"required":["role","adapter","protocol"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.model.configuration".into(),
            }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        args(value).map(|_| ())
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let args = args(&call.arguments)
                .map_err(|error| ToolExecutionError::Rejected(error.message))?;
            if context.is_cancel_requested()
                || context
                    .deadline()
                    .is_some_and(|at| at <= std::time::Instant::now())
            {
                return Err(ToolExecutionError::Cancelled(
                    "调用已停止，未切换模型适配器。".into(),
                ));
            }
            let service = self.0.get().ok_or_else(unavailable)?;
            let scope = context
                .scope()
                .ok_or_else(|| ToolExecutionError::Rejected("缺少可信平台身份。".into()))?;
            if !service.authorization.is_operator(scope) {
                return Err(ToolExecutionError::Rejected(
                    "切换模型适配器仅限主人或管理员。".into(),
                ));
            }
            service.switch_adapter(args)
        })
    }
}

#[cfg(test)]
#[path = "qq_model_adapter_tests.rs"]
mod tests;
