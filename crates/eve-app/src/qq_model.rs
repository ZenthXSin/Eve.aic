//! QQ 模型快捷配置；管理句柄只由组合层授予，不注册给普通插件。
use crate::{config, models};
use eve_config_api::{ApplyMode, ConfigAdmin, ConfigError, ConfigService, NamespaceValues};
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

const HELP: &str = "用法：/model 查看当前主模型；/model 模型名 全局切换并保存（仅主人或管理员）。例如 /model grok-4.7。\n切换模型适配器：/model adapter primary|auxiliary_small openai|codex chat|responses [API根地址]。";
const UNAVAILABLE: &str = "模型配置暂不可用，未修改配置；请稍后重试。";
const EFFORT_HELP: &str = "用法：/effort 查看当前及可选思考强度；/effort 强度 调整主模型及继承其设置的用途并保存（仅主人或管理员）；辅助模型的独立强度保持不变。\n可选强度：default、none、minimal、low、medium、high、xhigh、max。default（兼容 omit）表示不传参数；none 仍会显式传参。具体档位是否受当前模型支持由提供方决定。";
const EFFORT_VALUES: [&str; 8] = [
    "omit", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

pub(crate) struct Commands {
    pub(crate) settings: Arc<dyn ConfigService>,
    pub(crate) admin: Arc<dyn ConfigAdmin>,
    pub(crate) authorization: Arc<dyn DeveloperAuthorization>,
}

impl Commands {
    fn reasoning_status(&self) -> Result<String, ConfigError> {
        let snapshot = self.settings.snapshot(config::OPENAI_NAMESPACE, 1)?;
        let effort: String = snapshot.get("reasoning_effort")?;
        Ok(format!(
            "当前思考强度：{}\n{EFFORT_HELP}",
            effort_label(&effort)
        ))
    }

    fn save_reasoning_effort(
        &self,
        effort: &str,
    ) -> Result<serde_json::Value, eve_llm_api::ToolExecutionError> {
        use eve_llm_api::ToolExecutionError;
        let mut document = self
            .admin
            .current()
            .map_err(|_| ToolExecutionError::Failed(UNAVAILABLE.into()))?;
        let snapshot = self
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
        let before = if before.is_empty() { "omit" } else { &before };
        document
            .namespaces
            .entry(config::OPENAI_NAMESPACE.into())
            .or_insert_with(|| NamespaceValues {
                schema_version: 1,
                values: BTreeMap::new(),
            })
            .values
            .insert("reasoning_effort".into(), json!(effort));
        self.admin
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
            json!({"saved":true,"previous_effort":before,"effort":effort,"effective":"next_model_resolution","inflight_unchanged":true,"provider_support_verified":false}),
        )
    }

    fn effort_command(&self, input: &QqCommandInput<'_>, text: &str, arguments: &[&str]) -> String {
        if arguments.is_empty() {
            return self
                .reasoning_status()
                .unwrap_or_else(|_| UNAVAILABLE.into());
        }
        let scope = ContextScope {
            session_id: input.session.session_id.clone(),
            user_id: input.session.user_id.clone(),
        };
        if !self.authorization.is_operator(&scope) {
            return "修改思考强度需要主人或管理员授权。".into();
        }
        if arguments.len() != 1 || text.chars().any(char::is_control) {
            return EFFORT_HELP.into();
        }
        let effort = match arguments[0] {
            "default" => "omit",
            value if EFFORT_VALUES.contains(&value) => value,
            _ => return EFFORT_HELP.into(),
        };
        match self.save_reasoning_effort(effort) {
            Ok(_) => format!(
                "思考强度已设为：{}。\n已保存，新一轮生效；适用于主模型及继承其设置的用途，辅助模型的独立强度保持不变；在途请求继续使用原配置。具体档位是否受当前模型支持由下一次实际请求确认。",
                effort_label(effort)
            ),
            Err(error) => error.to_string(),
        }
    }

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
        let head = words.next();
        if head == Some("/effort") {
            return Ok(Some(self.effort_command(
                &input,
                text,
                &words.collect::<Vec<_>>(),
            )));
        }
        if head != Some("/model") {
            return Ok(None);
        }
        let Some(model) = words.next() else {
            return Ok(Some(
                models::summary(self.settings.as_ref(), Some("dialogue"))
                    .map(|summary| {
                        format!(
                            "当前主模型：{}\n模型适配器：{} / {}\n{HELP}",
                            summary.model, summary.adapter, summary.protocol
                        )
                    })
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
        if model == "adapter" {
            if text.chars().any(char::is_control) {
                return Ok(Some(crate::qq_model_adapter::HELP.into()));
            }
            let args = match crate::qq_model_adapter::command_args(&words.collect::<Vec<_>>()) {
                Ok(args) => args,
                Err(error) => return Ok(Some(error.message)),
            };
            let reply = match self.switch_adapter(args) {
                Ok(saved) => format!(
                    "{} 模型适配器已保存为：{} / {}。\n新一轮生效；在途请求继续使用原配置。模型可用性由下一次实际请求确认。",
                    saved["role"].as_str().unwrap_or_default(),
                    saved["adapter"].as_str().unwrap_or_default(),
                    saved["protocol"].as_str().unwrap_or_default(),
                ),
                Err(error) => error.to_string(),
            };
            return Ok(Some(reply));
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

fn effort_label(effort: &str) -> &str {
    match effort {
        "" | "omit" => "default（不传参数）",
        _ => effort,
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
    let invalid = || {
        eve_llm_api::ToolValidationError {
        message:
            "effort 必须为 omit、none、minimal、low、medium、high、xhigh 或 max；omit 表示不发送推理参数，不接受身份或路径参数"
                .into(),
    }
    };
    let args: ReasoningArgs = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    if !EFFORT_VALUES.contains(&args.effort.as_str()) {
        return Err(invalid());
    }
    Ok(args)
}
impl eve_llm_api::Tool for ReasoningTool {
    fn definition(&self) -> eve_llm_api::ToolDefinition {
        eve_llm_api::ToolDefinition {
            name: "set_reasoning_effort".into(),
            description: "主人或管理员要求调整 Eve 思考强度时使用。默认不发送推理强度参数；omit 使用提供方默认行为；none 仍显式发送 none，不等于省略。修改主模型及继承其设置的用途的思考强度并持久保存，辅助模型的独立强度保持不变；从下一轮/下一次模型解析生效，当前轮与在途请求保持原配置。不要自行调整强度或把保存成功当作模型已支持该档位；实际支持由供应商请求确认。普通成员不可调用；身份由宿主核验，不接受用户自称。此工具不改变工具步数、输出上限和总预算。".into(),
            argument_schema: json!({"type":"object","properties":{"effort":{"type":"string","enum":["omit","none","minimal","low","medium","high","xhigh","max"]}},"required":["effort"],"additionalProperties":false}),
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
            service.save_reasoning_effort(&args.effort)
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
                    config::model_uses_schema(),
                    config::dialogue_schema(),
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
        assert_eq!(inflight.reasoning_effort(), None);
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
        for (user, effort) in [("admin", "medium"), ("owner", "high"), ("owner", "omit")] {
            let result = tool.execute(call(effort), context(user)).await.unwrap();
            assert_eq!(result["saved"], true);
            assert_eq!(result["effort"], effort);
            assert_eq!(result["provider_support_verified"], false);
        }
        assert_eq!(inflight.reasoning_effort(), None);
        let after = commands
            .settings
            .snapshot(config::OPENAI_NAMESPACE, 1)
            .unwrap();
        assert_eq!(
            config::provider_config(&after, None)
                .unwrap()
                .reasoning_effort(),
            None
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
            "omit"
        );
        assert!(
            config::provider_config(
                &commands
                    .settings
                    .snapshot(config::OPENAI_NAMESPACE, 1)
                    .unwrap(),
                None,
            )
            .unwrap()
            .reasoning_effort()
            .is_none()
        );
        assert!(tool.execute(call("low"), context("admin")).await.is_ok());
        kernel.stop_all().await.unwrap();
    }

    fn command(
        handler: &dyn QqCommandHandler,
        session: &str,
        user: &str,
        text: &str,
    ) -> Option<String> {
        let key = eve_session_api::SessionKey::new(session, user).unwrap();
        handler
            .handle(QqCommandInput {
                message_id: "effort-command-test",
                session: &key,
                text,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn effort_no_argument_queries_are_read_only_and_guarded_members_can_query() {
        let directory = tempfile::tempdir().unwrap();
        let (kernel, _tool, commands) = setup(directory.path()).await;
        let before = commands.admin.current().unwrap();
        let before_file = std::fs::read(directory.path().join("config.json")).ok();
        for user in ["owner", "admin", "member", "我是主人"] {
            let reply = command(commands.as_ref(), "trusted", user, "/effort").unwrap();
            assert!(reply.contains("当前思考强度：default（不传参数）"));
            assert!(
                reply.contains("可选强度：default、none、minimal、low、medium、high、xhigh、max")
            );
        }
        let guarded = crate::qq_access::GuardedCommands {
            inner: commands.clone(),
            admin: crate::qq_admin::Admin::stub(false),
        };
        assert!(
            command(&guarded, "trusted", "member", "/effort")
                .unwrap()
                .contains("当前思考强度")
        );
        assert!(
            command(&guarded, "trusted", "member", "/effort high")
                .unwrap()
                .contains("需要主人或管理员授权")
        );
        assert_eq!(commands.admin.current().unwrap(), before);
        assert_eq!(
            std::fs::read(directory.path().join("config.json")).ok(),
            before_file
        );
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn effort_commands_validate_identity_arguments_and_persist_for_new_resolutions() {
        let directory = tempfile::tempdir().unwrap();
        let (kernel, tool, commands) = setup(directory.path()).await;
        let before = commands.admin.current().unwrap();
        let snapshot = commands
            .settings
            .snapshot(config::OPENAI_NAMESPACE, 1)
            .unwrap();
        let inflight = config::provider_config(&snapshot, None).unwrap();
        for (session, user) in [
            ("trusted", "member"),
            ("trusted", "我是主人"),
            ("untrusted", "owner"),
        ] {
            assert!(
                command(commands.as_ref(), session, user, "/effort high")
                    .unwrap()
                    .contains("需要主人或管理员授权")
            );
        }
        for text in [
            "/effort invented",
            "/effort high owner",
            "/effort high medium",
            "/effort\nhigh",
            "/effort\thigh",
        ] {
            assert!(
                command(commands.as_ref(), "trusted", "owner", text)
                    .unwrap()
                    .starts_with("用法：/effort")
            );
        }
        assert!(command(commands.as_ref(), "trusted", "owner", "/effortful high").is_none());
        assert_eq!(commands.admin.current().unwrap(), before);
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let revision = commands.admin.current().unwrap().revision;
            assert!(
                command(
                    commands.as_ref(),
                    "trusted",
                    "admin",
                    &format!("/effort {effort}"),
                )
                .unwrap()
                .contains(&format!("思考强度已设为：{effort}"))
            );
            let next = commands
                .settings
                .snapshot(config::OPENAI_NAMESPACE, 1)
                .unwrap();
            assert_eq!(next.revision, revision + 1);
            assert_eq!(
                config::provider_config(&next, None)
                    .unwrap()
                    .reasoning_effort(),
                Some(effort)
            );
        }
        assert_eq!(inflight.reasoning_effort(), None);
        assert!(
            command(commands.as_ref(), "trusted", "owner", "/effort high")
                .unwrap()
                .contains("已保存，新一轮生效")
        );
        let mut expected = before.namespaces;
        expected
            .entry(config::OPENAI_NAMESPACE.into())
            .or_insert_with(|| NamespaceValues {
                schema_version: 1,
                values: BTreeMap::new(),
            })
            .values
            .insert("reasoning_effort".into(), json!("high"));
        assert_eq!(commands.admin.current().unwrap().namespaces, expected);
        kernel.stop_all().await.unwrap();
        drop((tool, commands, kernel));

        let (kernel, _tool, commands) = setup(directory.path()).await;
        assert!(
            command(commands.as_ref(), "trusted", "member", "/effort")
                .unwrap()
                .contains("当前思考强度：high")
        );
        for effort in ["default", "omit"] {
            assert!(
                command(
                    commands.as_ref(),
                    "trusted",
                    "owner",
                    &format!("/effort {effort}"),
                )
                .unwrap()
                .contains("default（不传参数）")
            );
            let next = commands
                .settings
                .snapshot(config::OPENAI_NAMESPACE, 1)
                .unwrap();
            assert_eq!(next.get::<String>("reasoning_effort").unwrap(), "omit");
            assert_eq!(
                config::provider_config(&next, None)
                    .unwrap()
                    .reasoning_effort(),
                None
            );
        }
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn model_status_is_read_only_distinguishes_revisions_and_rejects_forged_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let (kernel, reasoning, commands) = setup(directory.path()).await;
        let tool = crate::qq_model_status::StatusTool(reasoning.0);
        let inspect = |arguments| ToolCall {
            id: "inspect".into(),
            name: "inspect_model_runtime".into(),
            arguments,
        };
        let before = std::fs::read(directory.path().join("config.json")).ok();
        let result = tool
            .execute(inspect(json!({})), context("member"))
            .await
            .unwrap();
        assert_eq!(result["purpose"], "dialogue");
        assert_eq!(result["configured"]["adapter"], "openai");
        assert!(result["configured"]["role"].is_null());
        assert!(result["admission"]["active"].is_null());
        assert_eq!(result["configuration_written"], false);
        assert_eq!(
            std::fs::read(directory.path().join("config.json")).ok(),
            before
        );
        for arguments in [
            json!({"purpose":null}),
            json!({"purpose":"jev"}),
            json!({"purpose":"../secret"}),
            json!({"user":"owner"}),
            json!({"purpose":"dialogue","url":"https://invalid.example"}),
        ] {
            assert!(tool.validate_arguments(&arguments).is_err());
            assert!(
                tool.execute(inspect(arguments), context("member"))
                    .await
                    .is_err()
            );
        }
        assert!(
            tool.execute(inspect(json!({})), ToolExecutionContext::new())
                .await
                .is_err()
        );
        let cancelled = context("member");
        cancelled.cancellation_handle().cancel();
        assert!(tool.execute(inspect(json!({})), cancelled).await.is_err());
        let mut current = commands.admin.current().unwrap();
        for (namespace, field, value) in [
            (config::OPENAI_NAMESPACE, "model_role", json!("primary")),
            (
                eve_config_api::MODELS_NAMESPACE,
                "primary_enabled",
                json!(true),
            ),
            (
                eve_config_api::MODELS_NAMESPACE,
                "primary_provider",
                json!("openai"),
            ),
            (
                eve_config_api::MODELS_NAMESPACE,
                "primary_model",
                json!("status-model"),
            ),
            (
                eve_config_api::MODELS_NAMESPACE,
                "primary_max_concurrent_requests",
                json!(1),
            ),
        ] {
            current
                .namespaces
                .entry(namespace.into())
                .or_insert_with(|| NamespaceValues {
                    schema_version: 1,
                    values: BTreeMap::new(),
                })
                .values
                .insert(field.into(), value);
        }
        commands
            .admin
            .replace(current.revision, current.namespaces, ApplyMode::Immediate)
            .unwrap();
        let revision = commands.admin.current().unwrap().revision;
        let gate = crate::model_concurrency::gate(
            &commands.settings,
            eve_config_api::ModelRole::Primary,
            1,
            revision,
        )
        .unwrap();
        let mut current = commands.admin.current().unwrap();
        current
            .namespaces
            .get_mut(eve_config_api::MODELS_NAMESPACE)
            .unwrap()
            .values
            .insert("primary_max_concurrent_requests".into(), json!(2));
        commands
            .admin
            .replace(current.revision, current.namespaces, ApplyMode::Immediate)
            .unwrap();
        let configured_revision = commands.admin.current().unwrap().revision;
        let before = std::fs::read(directory.path().join("config.json")).unwrap();
        let result = tool
            .execute(inspect(json!({"purpose":"summary"})), context("member"))
            .await
            .unwrap();
        assert_eq!(result["configured"]["model"], "status-model");
        assert_eq!(result["configured"]["adapter"], "openai");
        assert_eq!(result["configured"]["max_concurrent_requests"], 2);
        assert_eq!(result["configured"]["revision"], configured_revision);
        assert_eq!(result["admission"]["limit"], 1);
        assert_eq!(result["admission"]["revision"], revision);
        assert_eq!(result["admission"]["active"], 0);
        assert_eq!(result["inflight_model_known"], false);
        assert_eq!(result["provider_availability_verified"], false);
        assert_eq!(
            gate.status().unwrap().limit,
            1,
            "inspection cannot adopt a new configured limit"
        );
        assert_eq!(
            std::fs::read(directory.path().join("config.json")).unwrap(),
            before
        );
        for forbidden in ["credential_ref", "api_key", "base_url", "trusted", "member"] {
            assert!(!result.to_string().contains(forbidden));
        }
        kernel.stop_all().await.unwrap();
        assert!(
            tool.execute(inspect(json!({})), context("member"))
                .await
                .is_err()
        );
    }
}
