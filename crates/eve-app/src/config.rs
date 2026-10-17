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
    let mut semantic_base = string("", "EVE_OPENAI_SEMANTIC_BASE_URL");
    semantic_base.restart_required = true;
    ConfigSchema {
        namespace: OPENAI_NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (
                "base_url".into(),
                string("https://ai.xn--rhqr8xvr4ahqsgka.com", "EVE_OPENAI_BASE_URL"),
            ),
            ("semantic_base_url".into(), semantic_base),
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
                integer(
                    2048,
                    MAX_OUTPUT_TOKENS_LIMIT,
                    "EVE_OPENAI_MAX_OUTPUT_TOKENS",
                ),
            ),
        ]),
    }
}
/// 按用途的单次输出上限；0 表示沿用主模型的上限。随每次模型解析读取，保存后下一次请求生效。
pub(crate) const MODEL_USES_NAMESPACE: &str = "runtime.model_uses";
/// 用途名与健康记录、自主预算类别一致。
pub(crate) const MODEL_USES: [&str; 11] = [
    "dialogue",
    "plan",
    "interest",
    "research",
    "practice",
    "skill",
    "forge",
    "outreach",
    "clarify",
    "preference",
    "reflection",
];
/// 单次输出上限与 provider.openai.max_output_tokens 的上限一致。
const MAX_OUTPUT_TOKENS_LIMIT: i64 = 16384;

pub(crate) fn use_limit_field(name: &str) -> String {
    format!("{name}_max_output_tokens")
}

pub(crate) fn history_schema() -> ConfigSchema {
    let field = |default, minimum, maximum, environment: &str| {
        let mut value = ConfigField::new(
            ConfigKind::Integer {
                minimum: Some(minimum),
                maximum: Some(maximum),
            },
            Some(json!(default)),
        );
        value.environment = Some(environment.into());
        value
    };
    ConfigSchema {
        namespace: crate::history::NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (
                "max_turns".into(),
                field(16, 1, 256, "EVE_HISTORY_MAX_TURNS"),
            ),
            (
                "max_bytes".into(),
                field(65536, 1024, 8388608, "EVE_HISTORY_MAX_BYTES"),
            ),
        ]),
    }
}

pub(crate) fn model_uses_schema() -> ConfigSchema {
    let fields = MODEL_USES
        .into_iter()
        .map(|name| {
            let mut field = ConfigField::new(
                ConfigKind::Integer {
                    minimum: Some(0),
                    maximum: Some(MAX_OUTPUT_TOKENS_LIMIT),
                },
                Some(json!(0)),
            );
            field.environment = Some(format!(
                "EVE_MODEL_USE_{}_MAX_OUTPUT_TOKENS",
                name.to_ascii_uppercase()
            ));
            (use_limit_field(name), field)
        })
        .collect();
    ConfigSchema {
        namespace: MODEL_USES_NAMESPACE.into(),
        version: 1,
        fields,
    }
}

pub(crate) fn memory_digest_schema() -> ConfigSchema {
    let field = |default, minimum, maximum, environment: &str| {
        let mut value = ConfigField::new(
            ConfigKind::Integer {
                minimum: Some(minimum),
                maximum: Some(maximum),
            },
            Some(json!(default)),
        );
        value.environment = Some(environment.into());
        value
    };
    let mut enabled = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    enabled.environment = Some("EVE_MEMORY_DIGEST_ENABLED".into());
    ConfigSchema {
        namespace: crate::qq_conversation::NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            ("enabled".into(), enabled),
            (
                "max_episodes".into(),
                field(4, 0, 8, "EVE_MEMORY_DIGEST_MAX_EPISODES"),
            ),
            (
                "max_expression_examples".into(),
                field(3, 0, 4, "EVE_MEMORY_DIGEST_MAX_EXPRESSION_EXAMPLES"),
            ),
            (
                "max_bytes".into(),
                field(6144, 512, 16384, "EVE_MEMORY_DIGEST_MAX_BYTES"),
            ),
        ]),
    }
}

pub(crate) fn dialogue_schema() -> ConfigSchema {
    let mut field = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    field.environment = Some("EVE_DIALOGUE_NATURAL_STYLE".into());
    ConfigSchema {
        namespace: crate::dialogue::NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("natural_style".into(), field)]),
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

    #[test]
    fn every_composed_model_use_has_a_limit_field() {
        use crate::{qq_budget, qq_cognition};
        let schema = model_uses_schema();
        schema.validate().unwrap();
        // 宿主按这些名称解析模型；缺字段会让该用途每次解析都失败。
        for name in [
            "dialogue",
            "plan",
            "preference",
            qq_cognition::REFLECTION_USE,
            qq_budget::INTEREST,
            qq_budget::RESEARCH,
            qq_budget::PRACTICE,
            qq_budget::SKILL,
            qq_budget::FORGE,
            qq_budget::OUTREACH,
            qq_budget::CLARIFY,
        ] {
            let field = &schema.fields[&use_limit_field(name)];
            assert_eq!(field.default, Some(json!(0)), "{name} 默认沿用主模型");
            assert!(!field.restart_required, "{name} 保存后下一次请求生效");
        }
        assert_eq!(schema.fields.len(), MODEL_USES.len());
    }
}
