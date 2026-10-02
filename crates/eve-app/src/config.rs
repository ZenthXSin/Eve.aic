use crate::AppError;
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigSnapshot};
use eve_llm_openai::OpenAiConfig;
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

pub(crate) const OPENAI_NAMESPACE: &str = "provider.openai";
pub(crate) fn openai_schema() -> ConfigSchema {
    let string = |default, environment: &str| {
        let mut field = ConfigField::new(ConfigKind::String, Some(json!(default)));
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
        namespace: OPENAI_NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (
                "base_url".into(),
                string("https://ai.xn--rhqr8xvr4ahqsgka.com", "EVE_OPENAI_BASE_URL"),
            ),
            (
                "model".into(),
                string("deepseek-v4.1-flash", "EVE_OPENAI_MODEL"),
            ),
            ("protocol".into(), string("chat", "EVE_OPENAI_PROTOCOL")),
            (
                "reasoning_effort".into(),
                string("", "EVE_OPENAI_REASONING_EFFORT"),
            ),
            (
                "timeout_seconds".into(),
                integer(120, 600, "EVE_OPENAI_TIMEOUT_SECONDS"),
            ),
            (
                "max_output_tokens".into(),
                integer(2048, 16384, "EVE_OPENAI_MAX_OUTPUT_TOKENS"),
            ),
        ]),
    }
}
pub(crate) fn provider_config(snapshot: &ConfigSnapshot) -> Result<OpenAiConfig, AppError> {
    let model: String = snapshot.get("model")?;
    if model.trim().is_empty() {
        return Err("请配置主模型：EVE_OPENAI_MODEL 或 provider.openai.model。".into());
    }
    let base_url: String = snapshot.get("base_url")?;
    let protocol: String = snapshot.get("protocol")?;
    let config = match protocol.as_str() {
        "chat" => OpenAiConfig::chat(model),
        "responses" => OpenAiConfig::new(model),
        _ => return Err("provider.openai.protocol 必须为 chat 或 responses。".into()),
    };
    let mut config = config.with_base_url(&base_url)?;
    config.request_timeout = Duration::from_secs(snapshot.get("timeout_seconds")?);
    config.max_output_tokens = Some(snapshot.get("max_output_tokens")?);
    let effort: String = snapshot.get("reasoning_effort")?;
    config.reasoning_effort = if effort.is_empty() {
        (protocol == "chat").then(|| "none".into())
    } else {
        Some(effort)
    };
    Ok(config)
}
