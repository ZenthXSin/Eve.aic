use eve_config_api::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn snapshot() -> ConfigSnapshot {
    let schema = model_roles_schema();
    ConfigSnapshot {
        namespace: schema.namespace,
        schema_version: schema.version,
        revision: 7,
        values: schema
            .fields
            .into_iter()
            .map(|(key, field)| (key, field.default.unwrap()))
            .collect(),
    }
}

fn enable(snapshot: &mut ConfigSnapshot, role: ModelRole) {
    snapshot.values.insert(role.field("enabled"), json!(true));
    snapshot.values.insert(role.field("provider"), json!("local"));
    snapshot.values.insert(role.field("model"), json!("shared-model"));
}

#[test]
fn default_schema_is_valid_and_roles_are_explicitly_disabled() {
    let schema = model_roles_schema();
    schema.validate().unwrap();
    let names: BTreeSet<_> = schema
        .fields
        .values()
        .map(|field| field.environment.as_deref().unwrap())
        .collect();
    assert_eq!(names.len(), schema.fields.len());
    let config = ModelRolesConfig::try_from(&snapshot()).unwrap();
    assert_eq!(config.revision(), 7);
    for role in ModelRole::ALL {
        assert!(config.profile(role).is_none());
        assert_eq!(
            config.require(role),
            Err(ConfigError::MissingValue(format!(
                "{MODELS_NAMESPACE}.{}",
                role.field("enabled")
            )))
        );
        assert_eq!(
            serde_json::to_value(role).unwrap(),
            Value::from(role.key())
        );
    }
}

#[test]
fn enabled_roles_can_share_models_and_keep_independent_limits() {
    let mut input = snapshot();
    for role in ModelRole::ALL {
        enable(&mut input, role);
    }
    input.values.insert("primary_timeout_ms".into(), json!(60_000));
    input.values.insert("auxiliary_small_max_output_tokens".into(), json!(128));
    input.values.insert("jev_credential_ref".into(), json!("vault:jev"));
    input.values.insert("semantic_dimensions".into(), json!(768));
    let config = ModelRolesConfig::try_from(&input).unwrap();
    assert_eq!(config.require(ModelRole::Primary).unwrap().timeout_ms, 60_000);
    let auxiliary = config.require(ModelRole::AuxiliarySmall).unwrap();
    assert_eq!(auxiliary.model, "shared-model");
    assert_eq!(auxiliary.max_output_tokens, Some(128));
    assert_eq!(
        config.require(ModelRole::Jev).unwrap().credential_ref.as_deref(),
        Some("vault:jev")
    );
    assert_eq!(
        config.require(ModelRole::Semantic).unwrap().semantic,
        Some(SemanticOptions::Embedding { dimensions: 768 })
    );
    input.values.insert("primary_model".into(), json!("changed"));
    assert_eq!(config.require(ModelRole::Primary).unwrap().model, "shared-model");
}

#[test]
fn enabled_roles_require_identifiers_without_leaking_values() {
    for role in ModelRole::ALL {
        for field in ["provider", "model"] {
            let mut input = snapshot();
            enable(&mut input, role);
            input.values.insert(role.field(field), json!(""));
            assert_eq!(
                ModelRolesConfig::try_from(&input),
                Err(ConfigError::MissingValue(format!(
                    "{MODELS_NAMESPACE}.{}",
                    role.field(field)
                )))
            );
        }
        for field in ["provider", "model", "credential_ref"] {
            let mut input = snapshot();
            input.values.insert(role.field(field), json!("secret\nvalue"));
            let error = ModelRolesConfig::try_from(&input).unwrap_err();
            assert!(matches!(error, ConfigError::InvalidValue(_)));
            assert!(!error.to_string().contains("secret"));
        }
    }
}

#[test]
fn semantic_operations_have_distinct_dimension_rules() {
    let mut input = snapshot();
    enable(&mut input, ModelRole::Semantic);
    assert!(ModelRolesConfig::try_from(&input).is_err());
    input.values.insert("semantic_operation".into(), json!("rerank"));
    let config = ModelRolesConfig::try_from(&input).unwrap();
    let profile = config.require(ModelRole::Semantic).unwrap();
    assert_eq!(profile.semantic, Some(SemanticOptions::Rerank));
    assert_eq!(profile.max_output_tokens, None);
    input.values.insert("semantic_dimensions".into(), json!(768));
    assert!(ModelRolesConfig::try_from(&input).is_err());
    input.values.insert("semantic_operation".into(), json!("chat"));
    assert_eq!(
        ModelRolesConfig::try_from(&input),
        Err(ConfigError::InvalidValue(format!(
            "{MODELS_NAMESPACE}.semantic_operation"
        )))
    );
}

#[test]
fn forged_snapshots_cannot_bypass_numeric_or_shape_checks() {
    for (field, value) in [
        ("primary_timeout_ms", json!(0)),
        ("auxiliary_small_max_concurrent_requests", json!(-1)),
        ("jev_max_output_tokens", json!(u64::from(u32::MAX) + 1)),
        ("semantic_dimensions", json!("768")),
        ("primary_enabled", json!("true")),
    ] {
        let mut input = snapshot();
        input.values.insert(field.into(), value);
        assert_eq!(
            ModelRolesConfig::try_from(&input),
            Err(ConfigError::InvalidValue(format!("{MODELS_NAMESPACE}.{field}")))
        );
    }
    let mut input = snapshot();
    input.values.insert("primary_api_key".into(), json!("secret"));
    assert_eq!(
        ModelRolesConfig::try_from(&input),
        Err(ConfigError::UnknownField(format!("{MODELS_NAMESPACE}.primary_api_key")))
    );
    input.values.remove("primary_api_key");
    input.values.remove("jev_enabled");
    assert_eq!(
        ModelRolesConfig::try_from(&input),
        Err(ConfigError::MissingValue(format!("{MODELS_NAMESPACE}.jev_enabled")))
    );
}

#[test]
fn foreign_namespaces_and_schema_versions_are_rejected() {
    let mut input = snapshot();
    input.schema_version = 2;
    assert_eq!(
        ModelRolesConfig::try_from(&input),
        Err(ConfigError::SchemaVersion {
            namespace: MODELS_NAMESPACE.into(),
            expected: 1,
            found: 2,
        })
    );
    input.namespace = LLM_NAMESPACE.into();
    assert_eq!(
        ModelRolesConfig::try_from(&input),
        Err(ConfigError::InvalidSchema(MODELS_NAMESPACE.into()))
    );
}
