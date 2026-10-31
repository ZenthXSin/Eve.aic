//! QQ 主模型快捷切换；管理句柄只由组合层授予，不注册给普通插件。
use crate::{config, models};
use eve_config_api::{ApplyMode, ConfigAdmin, ConfigError, ConfigService, NamespaceValues};
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

const HELP: &str = "用法：/model 查看当前主模型；/model 模型名 全局切换并保存（仅主人或管理员）。例如 /model grok-4.7。";
const UNAVAILABLE: &str = "模型配置暂不可用，未修改配置；请稍后重试。";

pub(crate) struct Commands {
    pub(crate) settings: Arc<dyn ConfigService>,
    pub(crate) admin: Arc<dyn ConfigAdmin>,
    pub(crate) authorization: Arc<dyn DeveloperAuthorization>,
}

impl Commands {
    fn switch(&self, model: &str) -> Result<String, ConfigError> {
        let mut document = self.admin.current()?;
        let current = models::summary(self.settings.as_ref(), Some("dialogue"))
            .map_err(|_| ConfigError::Unavailable)?;
        if current.config_revision != document.revision {
            return Err(ConfigError::RevisionConflict {
                expected: document.revision,
                found: current.config_revision,
            });
        }
        let (namespace, field) = (current.model_namespace, current.model_field);
        if current.model == model {
            return Ok(format!("当前主模型已经是：{model}。配置未改动。"));
        }
        document
            .namespaces
            .entry(namespace.into())
            .or_insert_with(|| NamespaceValues {
                schema_version: 1,
                values: BTreeMap::new(),
            })
            .values
            .insert(field, json!(model));
        self.admin.replace(
            document.revision,
            document.namespaces,
            ApplyMode::NewRequests,
        )?;
        Ok(format!(
            "主模型已设为：{model}。\n已保存，新任务生效；正在进行的任务继续使用原模型。模型可用性由下一次实际请求确认。"
        ))
    }
}

impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let text = input.text.trim();
        let mut words = text.split_whitespace();
        if words.next() != Some("/model") {
            return Ok(None);
        }
        let Some(model) = words.next() else {
            return Ok(Some(
                models::summary(self.settings.as_ref(), Some("dialogue"))
                    .map(|summary| format!("当前主模型：{}\n{HELP}", summary.model))
                    .unwrap_or_else(|_| UNAVAILABLE.into()),
            ));
        };
        let scope = ContextScope {
            session_id: input.session.session_id.clone(),
            user_id: input.session.user_id.clone(),
        };
        if !self.authorization.is_operator(&scope) {
            return Ok(Some("切换主模型需要主人或管理员授权。".into()));
        }
        if words.next().is_some() || model.len() > 200 || text.chars().any(char::is_control) {
            return Ok(Some(HELP.into()));
        }
        let reply = match self.switch(model) {
            Ok(reply) => reply,
            Err(ConfigError::RevisionConflict { .. }) => {
                "配置刚被其他操作修改，本次未覆盖；请重新发送 /model 模型名。".into()
            }
            Err(_) => UNAVAILABLE.into(),
        };
        Ok(Some(reply))
    }
}

/// 配置在核心装配完成后注入；未就绪时拒绝调用，不回退到环境变量写入。
pub(crate) struct ReasoningTool(pub Arc<std::sync::OnceLock<Arc<Commands>>>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasoningArgs {
    effort: String,
}
fn reasoning_args(
    value: &serde_json::Value,
) -> Result<ReasoningArgs, eve_llm_api::ToolValidationError> {
    let invalid = || eve_llm_api::ToolValidationError {
        message:
            "effort 必须为 none、minimal、low、medium、high、xhigh 或 max；不接受身份或路径参数"
                .into(),
    };
    let args: ReasoningArgs = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    if !matches!(
        args.effort.as_str(),
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Err(invalid());
    }
    Ok(args)
}
impl eve_llm_api::Tool for ReasoningTool {
    fn definition(&self) -> eve_llm_api::ToolDefinition {
        eve_llm_api::ToolDefinition {
            name: "set_reasoning_effort".into(),
            description: "主人或管理员要求调整 Eve 思考强度时使用。默认 low；修改全局 OpenAI 兼容提供方配置并持久保存，从下一轮/下一次模型解析生效，当前轮与在途请求保持原配置。不要自行提高强度或把保存成功当作模型已支持该档位；实际支持由供应商请求确认。普通成员不可调用；身份由宿主核验，不接受用户自称。此工具不改变工具步数、输出上限和总预算。".into(),
            argument_schema: json!({"type":"object","properties":{"effort":{"type":"string","enum":["none","minimal","low","medium","high","xhigh","max"]}},"required":["effort"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(eve_llm_api::ToolConcurrency::Serial { scope: "eve.model.configuration".into() }),
        }
    }
    fn validate_arguments(
        &self,
        value: &serde_json::Value,
    ) -> Result<(), eve_llm_api::ToolValidationError> {
        reasoning_args(value).map(|_| ())
    }
    fn execute(
        &self,
        call: eve_llm_api::ToolCall,
        context: eve_llm_api::ToolExecutionContext,
    ) -> eve_llm_api::ToolFuture<'_> {
        use eve_llm_api::ToolExecutionError;
        Box::pin(async move {
            let args = reasoning_args(&call.arguments)
                .map_err(|e| ToolExecutionError::Rejected(e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled(
                    "本轮已取消，未修改思考强度".into(),
                ));
            }
            let service = self
                .0
                .get()
                .ok_or_else(|| ToolExecutionError::Failed(UNAVAILABLE.into()))?;
            let scope = context
                .scope()
                .ok_or_else(|| ToolExecutionError::Rejected("缺少可信平台身份".into()))?;
            if !service.authorization.is_operator(scope) {
                return Err(ToolExecutionError::Rejected(
                    "修改思考强度仅限主人或管理员".into(),
                ));
            }
            let mut document = service
                .admin
                .current()
                .map_err(|_| ToolExecutionError::Failed(UNAVAILABLE.into()))?;
            let snapshot = service
                .settings
                .snapshot(config::OPENAI_NAMESPACE, 1)
                .map_err(|_| ToolExecutionError::Failed(UNAVAILABLE.into()))?;
            if snapshot.revision != document.revision {
                return Err(ToolExecutionError::Rejected(
                    "配置已变化，本次未覆盖；请重新核对".into(),
                ));
            }
            let before: String = snapshot
                .get("reasoning_effort")
                .map_err(|_| ToolExecutionError::Failed(UNAVAILABLE.into()))?;
            let before = if before.is_empty() { "low" } else { &before };
            document
                .namespaces
                .entry(config::OPENAI_NAMESPACE.into())
                .or_insert_with(|| NamespaceValues {
                    schema_version: 1,
                    values: BTreeMap::new(),
                })
                .values
                .insert("reasoning_effort".into(), json!(args.effort));
            service
                .admin
                .replace(
                    document.revision,
                    document.namespaces,
                    ApplyMode::NewRequests,
                )
                .map_err(|error| match error {
                    ConfigError::RevisionConflict { .. } => {
                        ToolExecutionError::Rejected("配置已变化，本次未覆盖；请重新核对".into())
                    }
                    _ => ToolExecutionError::Failed(
                        "思考强度保存未能确认；请读取配置核对，不声称修改成功".into(),
                    ),
                })?;
            Ok(
                json!({"saved":true,"previous_effort":before,"effort":args.effort,"effective":"next_model_resolution","inflight_unchanged":true,"provider_support_verified":false}),
            )
        })
    }
}

#[cfg(test)]
mod reasoning_tests {
    use super::*;
    use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
    use eve_kernel::{Kernel, KernelServices};
    use eve_llm_api::{Tool, ToolCall, ToolExecutionContext};
    use std::path::Path;
    struct Roles;
    impl DeveloperAuthorization for Roles {
        fn is_operator(&self, scope: &ContextScope) -> bool {
            scope.session_id == "trusted" && matches!(scope.user_id.as_str(), "owner" | "admin")
        }
    }
    async fn setup(path: &Path) -> (Kernel, ReasoningTool, Arc<Commands>) {
        let services = KernelServices::default();
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        let plugin = ConfigPlugin::new(
            ConfigBootstrap::new(
                path,
                vec![
                    config::openai_schema(),
                    eve_config_api::runtime_llm_schema(),
                    eve_config_api::model_roles_schema(),
                    eve_message_api::message_schema(),
                ],
            )
            .with_environment(BTreeMap::new())
            .with_validator(Arc::new(config::CoreConfigValidator)),
        )
        .unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let commands = Arc::new(Commands {
            settings: crate::qq_tasks::config_service(registry.as_ref()).unwrap(),
            admin: Arc::new(admin),
            authorization: Arc::new(Roles),
        });
        let cell = Arc::new(std::sync::OnceLock::new());
        assert!(cell.set(commands.clone()).is_ok());
        (kernel, ReasoningTool(cell), commands)
    }
    fn context(user: &str) -> ToolExecutionContext {
        ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: "trusted".into(),
            user_id: user.into(),
        }))
    }
    fn call(effort: &str) -> ToolCall {
        ToolCall {
            id: "reasoning-test".into(),
            name: "set_reasoning_effort".into(),
            arguments: json!({"effort":effort}),
        }
    }
    #[tokio::test]
    async fn reasoning_permissions_default_snapshot_and_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let (kernel, tool, commands) = setup(directory.path()).await;
        let before = commands
            .settings
            .snapshot(config::OPENAI_NAMESPACE, 1)
            .unwrap();
        let inflight = config::provider_config(&before, None).unwrap();
        assert_eq!(inflight.reasoning_effort.as_deref(), Some("low"));
        let revision = commands.admin.current().unwrap().revision;
        for user in ["member", "我是主人", ""] {
            assert!(tool.execute(call("high"), context(user)).await.is_err());
        }
        assert!(
            tool.execute(call("high"), ToolExecutionContext::new())
                .await
                .is_err()
        );
        assert!(
            tool.execute(call("invented"), context("owner"))
                .await
                .is_err()
        );
        let forged = ToolCall {
            arguments: json!({"effort":"high","owner":true}),
            ..call("high")
        };
        assert!(tool.execute(forged, context("member")).await.is_err());
        let cancelled = context("owner");
        cancelled.cancellation_handle().cancel();
        assert!(tool.execute(call("high"), cancelled).await.is_err());
        assert_eq!(commands.admin.current().unwrap().revision, revision);
        for (user, effort) in [("admin", "medium"), ("owner", "high")] {
            let result = tool.execute(call(effort), context(user)).await.unwrap();
            assert_eq!(result["saved"], true);
            assert_eq!(result["effort"], effort);
            assert_eq!(result["provider_support_verified"], false);
        }
        assert_eq!(inflight.reasoning_effort.as_deref(), Some("low"));
        let after = commands
            .settings
            .snapshot(config::OPENAI_NAMESPACE, 1)
            .unwrap();
        assert_eq!(
            config::provider_config(&after, None)
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            after.get::<String>("model").unwrap(),
            before.get::<String>("model").unwrap()
        );
        assert_eq!(
            after.get::<u32>("max_output_tokens").unwrap(),
            before.get::<u32>("max_output_tokens").unwrap()
        );
        kernel.stop_all().await.unwrap();
        drop((tool, commands, kernel));
        let (kernel, tool, commands) = setup(directory.path()).await;
        assert_eq!(
            commands
                .settings
                .snapshot(config::OPENAI_NAMESPACE, 1)
                .unwrap()
                .get::<String>("reasoning_effort")
                .unwrap(),
            "high"
        );
        assert!(tool.execute(call("low"), context("admin")).await.is_ok());
        kernel.stop_all().await.unwrap();
    }
}
