use crate::AppError;
use eve_config_api::{
    ConfigField, ConfigKind, ConfigSchema, ConfigSnapshot, ModelProfile, ModelRole,
};
use eve_llm_openai::OpenAiConfig;
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

pub(crate) const OPENAI_NAMESPACE: &str = "provider.openai";

pub(crate) struct CoreConfigValidator;
impl eve_config_api::ConfigValidator for CoreConfigValidator {
    fn validate(
        &self,
        snapshots: &BTreeMap<String, ConfigSnapshot>,
    ) -> eve_config_api::ConfigResult<()> {
        use eve_config_api::{
            ConfigError, LLM_NAMESPACE, LlmRuntimeConfig, MODELS_NAMESPACE, ModelRolesConfig,
        };
        let invalid = || ConfigError::InvalidValue("宿主配置".into());
        let runtime =
            LlmRuntimeConfig::try_from(snapshots.get(LLM_NAMESPACE).ok_or_else(invalid)?)?;
        if runtime.response_mode != "complete" {
            return Err(ConfigError::InvalidValue(format!(
                "{LLM_NAMESPACE}.response_mode"
            )));
        }
        eve_message_api::MessageConfig::try_from(
            snapshots
                .get(eve_message_api::MESSAGE_NAMESPACE)
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        let roles =
            ModelRolesConfig::try_from(snapshots.get(MODELS_NAMESPACE).ok_or_else(invalid)?)?;
        let primary = snapshots.get(OPENAI_NAMESPACE).ok_or_else(invalid)?;
        let role = configured_role(primary).map_err(|_| invalid())?;
        let profile = role.map(|r| roles.require(r)).transpose()?;
        provider_config(primary, profile).map_err(|_| invalid())?;
        if let Some(profile) = roles.profile(ModelRole::Jev)
            && (profile.provider != "jev"
                || profile.max_output_tokens.is_some()
                || profile
                    .credential_ref
                    .as_deref()
                    .is_some_and(|r| r != "env:EVE_JEV_API_KEY"))
        {
            return Err(invalid());
        }
        Ok(())
    }
}
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
            ("model_role".into(), string("", "EVE_OPENAI_MODEL_ROLE")),
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
pub(crate) fn configured_role(snapshot: &ConfigSnapshot) -> Result<Option<ModelRole>, AppError> {
    let role: String = snapshot.get("model_role")?;
    match role.as_str() {
        "" => Ok(None),
        "primary" => Ok(Some(ModelRole::Primary)),
        _ => Err("provider.openai.model_role 必须为空或 primary。".into()),
    }
}

pub(crate) fn provider_config(
    snapshot: &ConfigSnapshot,
    primary: Option<&ModelProfile>,
) -> Result<OpenAiConfig, AppError> {
    if let Some(profile) = primary {
        if profile.provider != "openai" {
            return Err("主模型角色当前只支持 provider=openai。".into());
        }
        if profile.credential_ref.is_some() {
            return Err(
                "主模型角色的凭据引用尚未接线；当前入口使用宿主 EVE_OPENAI_API_KEY。".into(),
            );
        }
        if profile.timeout_ms == 0 || profile.timeout_ms > 600_000 {
            return Err("主模型角色期限必须在 1 至 600000 毫秒之间。".into());
        }
    }
    let model: String = match primary {
        Some(profile) => profile.model.clone(),
        None => snapshot.get("model")?,
    };
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
    if let Some(profile) = primary {
        config.request_timeout = Duration::from_millis(profile.timeout_ms);
        config.max_output_tokens = profile.max_output_tokens;
    } else {
        config.request_timeout = Duration::from_secs(snapshot.get("timeout_seconds")?);
        config.max_output_tokens = Some(snapshot.get("max_output_tokens")?);
    }
    let effort: String = snapshot.get("reasoning_effort")?;
    config.reasoning_effort = if effort.is_empty() {
        (protocol == "chat").then(|| "none".into())
    } else {
        Some(effort)
    };
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_validator_accepts_supported_restart_settings_and_rejects_stream() {
        use eve_config_api::{
            ConfigValidator, LLM_NAMESPACE, model_roles_schema, runtime_llm_schema,
        };
        let schemas = [
            runtime_llm_schema(),
            model_roles_schema(),
            openai_schema(),
            eve_message_api::message_schema(),
        ];
        let mut snapshots: BTreeMap<_, _> = schemas
            .into_iter()
            .map(|schema| {
                let namespace = schema.namespace;
                let snapshot = ConfigSnapshot {
                    namespace: namespace.clone(),
                    schema_version: schema.version,
                    revision: 0,
                    values: schema
                        .fields
                        .into_iter()
                        .filter_map(|(id, field)| field.default.map(|value| (id, value)))
                        .collect(),
                };
                (namespace, snapshot)
            })
            .collect();
        CoreConfigValidator.validate(&snapshots).unwrap();
        let runtime = snapshots.get_mut(LLM_NAMESPACE).unwrap();
        runtime
            .values
            .insert("max_parallel_tool_calls".into(), json!(11));
        CoreConfigValidator.validate(&snapshots).unwrap();
        snapshots
            .get_mut(LLM_NAMESPACE)
            .unwrap()
            .values
            .insert("response_mode".into(), json!("stream"));
        assert!(CoreConfigValidator.validate(&snapshots).is_err());
    }

    fn snapshot() -> ConfigSnapshot {
        let schema = openai_schema();
        ConfigSnapshot {
            namespace: schema.namespace,
            schema_version: schema.version,
            revision: 0,
            values: schema
                .fields
                .into_iter()
                .map(|(name, field)| (name, field.default.unwrap()))
                .collect(),
        }
    }

    fn profile() -> ModelProfile {
        ModelProfile {
            provider: "openai".into(),
            model: "role-model".into(),
            credential_ref: None,
            timeout_ms: 1250,
            max_concurrent_requests: 1,
            max_output_tokens: None,
            semantic: None,
        }
    }

    #[test]
    fn primary_profile_overrides_model_deadline_and_output_limit() {
        let direct = snapshot();
        assert_eq!(configured_role(&direct).unwrap(), None);
        let legacy = provider_config(&direct, None).unwrap();
        assert_eq!(legacy.model, "deepseek-v4.1-flash");
        assert_eq!(legacy.request_timeout, Duration::from_secs(120));
        assert_eq!(legacy.max_output_tokens, Some(2048));
        let mut primary = profile();
        let selected = provider_config(&direct, Some(&primary)).unwrap();
        assert_eq!(selected.model, "role-model");
        assert_eq!(selected.request_timeout, Duration::from_millis(1250));
        assert_eq!(selected.max_output_tokens, None);
        primary.max_output_tokens = Some(96);
        assert_eq!(
            provider_config(&direct, Some(&primary))
                .unwrap()
                .max_output_tokens,
            Some(96)
        );
    }

    #[test]
    fn unresolved_reference_unknown_provider_and_unsupported_deadline_do_not_fall_back() {
        let direct = snapshot();
        let mut primary = profile();
        primary.credential_ref = Some("PRIVATE_REFERENCE_VALUE".into());
        let error = provider_config(&direct, Some(&primary))
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("PRIVATE_REFERENCE_VALUE"));
        primary.credential_ref = None;
        primary.provider = "unknown".into();
        assert!(provider_config(&direct, Some(&primary)).is_err());
        primary.provider = "openai".into();
        primary.timeout_ms = 600_001;
        assert!(provider_config(&direct, Some(&primary)).is_err());
        let mut selected = direct;
        selected
            .values
            .insert("model_role".into(), json!("primary"));
        assert_eq!(
            configured_role(&selected).unwrap(),
            Some(ModelRole::Primary)
        );
        selected.values.insert("model_role".into(), json!("jev"));
        assert!(configured_role(&selected).is_err());
    }
}
