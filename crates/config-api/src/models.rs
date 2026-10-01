//! 四类模型角色的普通配置契约；不创建 Provider、不解析凭据、不调用模型。
use crate::{
    ConfigError, ConfigField, ConfigKind, ConfigResult, ConfigSchema, ConfigService, ConfigSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MODELS_NAMESPACE: &str = "runtime.models";
pub const MODELS_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Primary,
    AuxiliarySmall,
    Jev,
    Semantic,
}

impl ModelRole {
    pub const ALL: [Self; 4] = [
        Self::Primary,
        Self::AuxiliarySmall,
        Self::Jev,
        Self::Semantic,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::AuxiliarySmall => "auxiliary_small",
            Self::Jev => "jev",
            Self::Semantic => "semantic",
        }
    }

    pub fn field(self, suffix: &str) -> String {
        format!("{}_{}", self.key(), suffix)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticOptions {
    Embedding { dimensions: u32 },
    Rerank,
}

/// Provider/model 是装配标识；credential_ref 仅为引用，不能保存密钥正文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub credential_ref: Option<String>,
    pub timeout_ms: u64,
    pub max_concurrent_requests: usize,
    pub max_output_tokens: Option<u32>,
    pub semantic: Option<SemanticOptions>,
}

/// 一次读取捕获全部角色；之后不因配置服务的 Immediate 更新而变化。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRolesConfig {
    revision: u64,
    profiles: BTreeMap<ModelRole, ModelProfile>,
}

impl ModelRolesConfig {
    pub fn capture(service: &dyn ConfigService) -> ConfigResult<Self> {
        Self::try_from(&service.snapshot(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION)?)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// None 表示明确关闭；不会选择其他角色作为隐式回退。
    pub fn profile(&self, role: ModelRole) -> Option<&ModelProfile> {
        self.profiles.get(&role)
    }

    pub fn require(&self, role: ModelRole) -> ConfigResult<&ModelProfile> {
        self.profile(role)
            .ok_or_else(|| ConfigError::MissingValue(path(&role.field("enabled"))))
    }
}

/// 首版沿用标量字段，单命名空间保证四类角色不会混用不同配置修订。
pub fn model_roles_schema() -> ConfigSchema {
    let mut fields = BTreeMap::new();
    for role in ModelRole::ALL {
        let values = [
            ("enabled", ConfigKind::Boolean, Value::from(false)),
            ("provider", ConfigKind::String, Value::from("")),
            ("model", ConfigKind::String, Value::from("")),
            ("credential_ref", ConfigKind::String, Value::from("")),
            (
                "timeout_ms",
                ConfigKind::Integer {
                    minimum: Some(1),
                    maximum: None,
                },
                Value::from(30_000),
            ),
            (
                "max_concurrent_requests",
                ConfigKind::Integer {
                    minimum: Some(1),
                    maximum: Some(u32::MAX.into()),
                },
                Value::from(1),
            ),
        ];
        for (suffix, kind, default) in values {
            insert_field(&mut fields, role.field(suffix), kind, default);
        }
        if role != ModelRole::Semantic {
            insert_field(
                &mut fields,
                role.field("max_output_tokens"),
                ConfigKind::Integer {
                    minimum: Some(0),
                    maximum: Some(u32::MAX.into()),
                },
                Value::from(0),
            );
        }
    }
    insert_field(
        &mut fields,
        ModelRole::Semantic.field("operation"),
        ConfigKind::String,
        Value::from("embedding"),
    );
    insert_field(
        &mut fields,
        ModelRole::Semantic.field("dimensions"),
        ConfigKind::Integer {
            minimum: Some(0),
            maximum: Some(u32::MAX.into()),
        },
        Value::from(0),
    );
    ConfigSchema {
        namespace: MODELS_NAMESPACE.into(),
        version: MODELS_SCHEMA_VERSION,
        fields,
    }
}

fn insert_field(
    fields: &mut BTreeMap<String, ConfigField>,
    name: String,
    kind: ConfigKind,
    default: Value,
) {
    let mut field = ConfigField::new(kind, Some(default));
    field.environment = Some(format!("EVE_MODELS_{}", name.to_ascii_uppercase()));
    fields.insert(name, field);
}

fn path(field: &str) -> String {
    format!("{MODELS_NAMESPACE}.{field}")
}

impl TryFrom<&ConfigSnapshot> for ModelRolesConfig {
    type Error = ConfigError;

    fn try_from(snapshot: &ConfigSnapshot) -> ConfigResult<Self> {
        if snapshot.namespace != MODELS_NAMESPACE {
            return Err(ConfigError::InvalidSchema(MODELS_NAMESPACE.into()));
        }
        if snapshot.schema_version != MODELS_SCHEMA_VERSION {
            return Err(ConfigError::SchemaVersion {
                namespace: MODELS_NAMESPACE.into(),
                expected: MODELS_SCHEMA_VERSION,
                found: snapshot.schema_version,
            });
        }
        let schema = model_roles_schema();
        for (name, value) in &snapshot.values {
            let field = schema
                .fields
                .get(name)
                .ok_or_else(|| ConfigError::UnknownField(path(name)))?;
            field.validate_value(&path(name), value)?;
        }
        // 包括关闭的角色，拒绝不完整或伪造的快照。
        for name in schema.fields.keys() {
            if !snapshot.values.contains_key(name) {
                return Err(ConfigError::MissingValue(path(name)));
            }
        }
        let mut profiles = BTreeMap::new();
        for role in ModelRole::ALL {
            let enabled: bool = snapshot.get(&role.field("enabled"))?;
            let provider = identifier(snapshot, role, "provider", enabled)?;
            let model = identifier(snapshot, role, "model", enabled)?;
            let reference = identifier(snapshot, role, "credential_ref", false)?;
            let timeout_ms = snapshot.get(&role.field("timeout_ms"))?;
            let max_concurrent_requests = snapshot.get(&role.field("max_concurrent_requests"))?;
            let (max_output_tokens, semantic) = if role == ModelRole::Semantic {
                let operation: String = snapshot.get(&role.field("operation"))?;
                let dimensions: u32 = snapshot.get(&role.field("dimensions"))?;
                let options = match operation.as_str() {
                    "embedding" => {
                        if enabled && dimensions == 0 {
                            return Err(ConfigError::InvalidValue(path(&role.field("dimensions"))));
                        }
                        SemanticOptions::Embedding { dimensions }
                    }
                    "rerank" => {
                        if dimensions != 0 {
                            return Err(ConfigError::InvalidValue(path(&role.field("dimensions"))));
                        }
                        SemanticOptions::Rerank
                    }
                    _ => {
                        return Err(ConfigError::InvalidValue(path(&role.field("operation"))));
                    }
                };
                (None, Some(options))
            } else {
                let limit: u32 = snapshot.get(&role.field("max_output_tokens"))?;
                ((limit != 0).then_some(limit), None)
            };
            if enabled {
                profiles.insert(
                    role,
                    ModelProfile {
                        provider,
                        model,
                        credential_ref: (!reference.is_empty()).then_some(reference),
                        timeout_ms,
                        max_concurrent_requests,
                        max_output_tokens,
                        semantic,
                    },
                );
            }
        }
        Ok(Self {
            revision: snapshot.revision,
            profiles,
        })
    }
}

fn identifier(
    snapshot: &ConfigSnapshot,
    role: ModelRole,
    suffix: &str,
    required: bool,
) -> ConfigResult<String> {
    let field = role.field(suffix);
    let value: String = snapshot.get(&field)?;
    if required && value.is_empty() {
        return Err(ConfigError::MissingValue(path(&field)));
    }
    if value.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(ConfigError::InvalidValue(path(&field)));
    }
    Ok(value)
}
