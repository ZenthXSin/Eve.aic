//! QQ 主模型快捷切换；管理句柄只由组合层授予，不注册给普通插件。
use crate::{config, models};
use eve_config_api::{
    ApplyMode, ConfigAdmin, ConfigError, ConfigService, MODELS_NAMESPACE, NamespaceValues,
};
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
        let provider = self.settings.snapshot(config::OPENAI_NAMESPACE, 1)?;
        if provider.revision != document.revision {
            return Err(ConfigError::RevisionConflict {
                expected: document.revision,
                found: provider.revision,
            });
        }
        let role = config::configured_role(&provider)
            .map_err(|_| ConfigError::InvalidValue("主模型选择".into()))?;
        let (namespace, field) = match role {
            Some(_) => (MODELS_NAMESPACE, "primary_model"),
            None => (config::OPENAI_NAMESPACE, "model"),
        };
        let current = models::summary(self.settings.as_ref(), Some("dialogue"))
            .map_err(|_| ConfigError::Unavailable)?;
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
            .insert(field.into(), json!(model));
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
