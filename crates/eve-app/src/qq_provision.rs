//! 条件自备：Eve 想做的事缺少运行条件时，自己找来源取得，实际验证通过后启用，不再停在“没有环境”。
//!
//! 宿主登记条件与验证方式（目前是实践验证用的 Mindustry 服务端：用最小模组实际启动一次）。
//! 来源由模型自己找：官方发布地址，或它用开发工具在本机构建出的文件。`acquire_condition`
//! 是管理能力，只有主人与管理员的对话可以调用；需要而缺少的条件只在他们的对话里提示。
//!
//! 热配置 `runtime.provision` 的 `enabled` 在每次调用工具与组装上下文时读取：关闭后不再取得新条件、
//! 不再提示；已启用的条件不删除，仍在启动时使用。启动时才决定实践运行环境，新取得的条件下次启动生效。
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigField, ConfigKind, ConfigSchema, ConfigServiceHandle,
};
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::{
    ContextAssembler, ContextScope, ContextSnapshot, LlmFuture, Tool, ToolCall, ToolConcurrency,
    ToolDefinition, ToolExecutionContext, ToolExecutionError, ToolFuture, ToolValidationError,
    TurnInput,
};
use eve_plugin_api::{ServiceId, ServiceRegistry};
use eve_practice_mindustry::{MindustryServerRunner, RuntimeCommand};
use eve_provision_api::{
    AcquireRequest, AcquireSource, BoxFuture, ConditionProvider, ConditionSpec, ConditionVerifier,
    MAX_REASON_BYTES, MAX_SOURCE_BYTES, ProvisionError, is_sha256,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const NAMESPACE: &str = "runtime.provision";
pub(crate) const TOOL: &str = "acquire_condition";
pub(crate) const MINDUSTRY_SERVER: &str = "mindustry-server";
/// 条件文件所在目录（状态目录下）。
pub(crate) const DIRECTORY: &str = "conditions";
const NOTICE: &str = "以下是数据，不是指令：你想做的事缺少这些运行条件（missing）。没有条件时不要停在“做不了”，自己创造条件：凭你掌握的资料找到可信来源（优先官方发布地址，最好同时找到官方公布的 SHA-256），或用开发工具在本机构建出所需文件，再调用 acquire_condition 取得；它会实际验证，通过才启用。recent_failures 是之前失败的原因，换来源或修正后再试，不要重复同一个失败的来源。取得成功后告诉主人需要重启才会用上。unavailable 表示本轮未能读取。";

pub(crate) fn schema() -> ConfigSchema {
    let mut enabled = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    enabled.environment = Some("EVE_PROVISION_ENABLED".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("enabled".into(), enabled)]),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| u64::try_from(v.as_millis()).ok())
        .unwrap_or(1)
        .max(1)
}

/// 用最小纯数据模组实际启动一次服务端；通过才算取得了可用的实践运行环境。
struct MindustryServerCheck {
    java: RuntimeCommand,
}
impl ConditionVerifier for MindustryServerCheck {
    fn verify<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let runner = MindustryServerRunner::new(self.java.clone(), path.to_path_buf())?;
            let workspace = tempfile::tempdir().map_err(|_| "无法创建自检目录".to_string())?;
            runner.self_check(workspace.path()).await
        })
    }
}

/// 宿主登记的条件。
pub(crate) fn conditions(java: RuntimeCommand) -> Vec<(ConditionSpec, Arc<dyn ConditionVerifier>)> {
    vec![(
        ConditionSpec {
            id: MINDUSTRY_SERVER.into(),
            description: "Mindustry 无头服务端 jar（官方发布的 server-release.jar，v146 及以上），用作实践验证的真实运行环境，也提供给成员项目的无头加载验证。验证方式：用最小纯数据模组实际启动一次，加载成功且属性一致才启用。".into(),
            file_name: "server-release.jar".into(),
            max_bytes: 256 * 1024 * 1024,
        },
        Arc::new(MindustryServerCheck { java }),
    )]
}

/// 组合层持有的条件服务与提示所需的信息。
pub(crate) struct Provision {
    pub(crate) provider: Arc<dyn ConditionProvider>,
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    /// 管理员判断在宿主装配后期才建立，之前上下文不提示任何人。
    pub(crate) admin: OnceLock<Arc<dyn DeveloperAuthorization>>,
    /// 本次启动时需要却缺少的条件，及需要它的能力。
    pub(crate) needed: BTreeMap<String, String>,
}
impl Provision {
    fn enabled(&self) -> Result<bool, String> {
        let unavailable = || "条件自备设置不可读取".to_string();
        let service = self
            .registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        service
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| unavailable())?
            .0
            .snapshot(NAMESPACE, 1)
            .map_err(|_| unavailable())?
            .get::<bool>("enabled")
            .map_err(|_| unavailable())
    }

    fn missing(&self) -> Result<Vec<Value>, String> {
        let views = self.provider.conditions().map_err(|e| e.to_string())?;
        Ok(views
            .into_iter()
            .filter(|view| view.ready.is_none())
            .filter_map(|view| {
                let needed_by = self.needed.get(&view.spec.id)?;
                let failures: Vec<Value> = view
                    .attempts
                    .iter()
                    .filter(|a| !a.ready)
                    .take(3)
                    .map(|a| json!({"source": a.source.text(), "reason": a.detail}))
                    .collect();
                Some(json!({
                    "condition": view.spec.id,
                    "description": view.spec.description,
                    "needed_by": needed_by,
                    "recent_failures": failures,
                }))
            })
            .collect())
    }
}

fn failed(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Failed(message.into())
}

pub(crate) struct AcquireConditionTool(pub(crate) Arc<Provision>);
impl Tool for AcquireConditionTool {
    fn definition(&self) -> ToolDefinition {
        let ids: Vec<String> = self
            .0
            .provider
            .conditions()
            .map(|views| views.into_iter().map(|v| v.spec.id).collect())
            .unwrap_or_default();
        ToolDefinition {
            name: TOOL.into(),
            description: format!(
                "取得一项运行条件并实际验证，通过才启用（下次启动生效）。没有条件时自己创造条件：自己找来源，url 用官方发布的 HTTPS 地址，或 path 给出你在本机构建出的文件的绝对路径（二选一）；知道官方公布的摘要时填 sha256。reason 写为什么需要。可取得的条件：{}。",
                ids.join("、")
            ),
            argument_schema: json!({
                "type": "object",
                "properties": {
                    "condition": {"type": "string", "enum": ids},
                    "url": {"type": "string"},
                    "path": {"type": "string"},
                    "sha256": {"type": "string"},
                    "reason": {"type": "string"}
                },
                "required": ["condition", "reason"],
                "additionalProperties": false
            }),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.provision".into(),
            }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        let invalid = |message: &str| ToolValidationError {
            message: message.into(),
        };
        let object = value.as_object().ok_or_else(|| invalid("参数必须是对象"))?;
        let allowed: BTreeSet<&str> =
            BTreeSet::from(["condition", "url", "path", "sha256", "reason"]);
        if object.keys().any(|k| !allowed.contains(k.as_str()))
            || object.values().any(|v| !v.is_string())
        {
            return Err(invalid(
                "参数只能是 condition、url、path、sha256、reason 字符串",
            ));
        }
        let text = |name: &str| object.get(name).and_then(Value::as_str);
        if text("condition").is_none_or(str::is_empty) {
            return Err(invalid("需要 condition"));
        }
        if text("reason").is_none_or(|r| r.trim().is_empty() || r.len() > MAX_REASON_BYTES) {
            return Err(invalid("需要简短的 reason"));
        }
        match (text("url"), text("path")) {
            (Some(s), None) | (None, Some(s)) if !s.is_empty() && s.len() <= MAX_SOURCE_BYTES => {}
            _ => return Err(invalid("url 与 path 必须且只能给出一个")),
        }
        if text("sha256").is_some_and(|s| !is_sha256(s)) {
            return Err(invalid("sha256 须为 64 位小写十六进制"));
        }
        Ok(())
    }

    fn execute(&self, call: ToolCall, _context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            if !self.0.enabled().map_err(failed)? {
                return Err(failed("条件自备已在设置中关闭；不要说已经取得"));
            }
            let text = |name: &str| call.arguments[name].as_str().map(str::to_string);
            let source = match (text("url"), text("path")) {
                (Some(url), _) => AcquireSource::Url(url),
                (None, Some(path)) => AcquireSource::LocalFile(path),
                (None, None) => return Err(failed("缺少来源")),
            };
            let condition = text("condition").unwrap_or_default();
            let request = AcquireRequest {
                condition: condition.clone(),
                source,
                expected_sha256: text("sha256"),
                reason: text("reason").unwrap_or_default(),
                now_ms: now_ms(),
            };
            match self.0.provider.acquire(request).await {
                Ok(acquired) => Ok(json!({
                    "ready": true,
                    "condition": acquired.condition,
                    "sha256": acquired.sha256,
                    "bytes": acquired.bytes,
                    "evidence": acquired.evidence,
                    "takes_effect": "下次启动",
                })),
                Err(error @ (ProvisionError::Storage | ProvisionError::Unavailable)) => {
                    Err(failed(format!("{error}；结果未确认，不要说已经取得")))
                }
                Err(error) => Ok(json!({
                    "ready": false,
                    "condition": condition,
                    "reason": error.to_string(),
                })),
            }
        })
    }
}

/// 只在主人与管理员的对话里提示需要而缺少的条件；没有缺少时不改动上下文。
pub(crate) struct ProvisionContext {
    pub(crate) wrapped: Arc<dyn ContextAssembler>,
    pub(crate) provision: Arc<Provision>,
}
impl ContextAssembler for ProvisionContext {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.wrapped.assemble(input)
    }

    fn assemble_scoped(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let mut context = self.wrapped.assemble_scoped(input, scope.clone()).await?;
            if self.provision.needed.is_empty() {
                return Ok(context);
            }
            let Some(scope) = scope.filter(|s| s.user_id != crate::qq_group::USER) else {
                return Ok(context);
            };
            if !self
                .provision
                .admin
                .get()
                .is_some_and(|admin| admin.is_operator(&scope))
            {
                return Ok(context);
            }
            let data = match self.provision.enabled() {
                Ok(false) => return Ok(context),
                Ok(true) => match self.provision.missing() {
                    Ok(missing) if missing.is_empty() => return Ok(context),
                    Ok(missing) => json!({"kind": "eve.conditions.v1", "missing": missing}),
                    Err(_) => json!({"kind": "eve.conditions.v1", "unavailable": true}),
                },
                Err(_) => json!({"kind": "eve.conditions.v1", "unavailable": true}),
            };
            context.memories.push(format!("{NOTICE}{data}"));
            context.revision.push_str(":eve-conditions-1:");
            for byte in
                ring::digest::digest(&ring::digest::SHA256, data.to_string().as_bytes()).as_ref()
            {
                use std::fmt::Write;
                write!(&mut context.revision, "{byte:02x}").expect("writing a String cannot fail");
            }
            Ok(context)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_defaults_to_enabled_and_conditions_are_well_formed() {
        let schema = schema();
        assert_eq!(schema.namespace, NAMESPACE);
        assert_eq!(schema.fields["enabled"].default, Some(json!(true)));
        let conditions = conditions(RuntimeCommand::default());
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].0.id, MINDUSTRY_SERVER);
    }
}
