//! 配置的公开定义层；不依赖 Kernel、文件系统或执行器。

mod models;
pub use models::*;

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, sync::Arc};

pub const CONFIG_PLUGIN_ID: &str = "eve.config";
pub const CONFIG_SERVICE_ID: &str = "eve.config.read.v1";
pub const LLM_NAMESPACE: &str = "runtime.llm";
pub const RESPONSE_MODE: &str = "response_mode";
pub const MAX_PARALLEL_TOOL_CALLS: &str = "max_parallel_tool_calls";
pub const DEFAULT_BACKUP_LIMIT: usize = 20;

pub type ConfigResult<T> = Result<T, ConfigError>;
pub type ConfigOverrides = BTreeMap<String, NamespaceValues>;

/// 宿主提供的业务校验；在提交之前检查完整候选配置，不发起模型或网络调用。
pub trait ConfigValidator: Send + Sync {
    fn validate(&self, snapshots: &BTreeMap<String, ConfigSnapshot>) -> ConfigResult<()>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigKind {
    Boolean,
    String,
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigField {
    pub kind: ConfigKind,
    pub default: Option<Value>,
    pub required: bool,
    pub environment: Option<String>,
    pub restart_required: bool,
    pub sensitive: bool,
    pub encrypted: bool,
}

impl ConfigField {
    pub fn new(kind: ConfigKind, default: Option<Value>) -> Self {
        Self {
            kind,
            default,
            required: true,
            environment: None,
            restart_required: false,
            sensitive: false,
            encrypted: false,
        }
    }

    pub fn validate_value(&self, path: &str, value: &Value) -> ConfigResult<()> {
        let valid = match &self.kind {
            ConfigKind::Boolean => value.is_boolean(),
            ConfigKind::String => value.is_string(),
            ConfigKind::Integer { minimum, maximum } => value.as_i64().is_some_and(|number| {
                minimum.is_none_or(|min| number >= min) && maximum.is_none_or(|max| number <= max)
            }),
        };
        if valid {
            Ok(())
        } else {
            Err(ConfigError::InvalidValue(path.into()))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSchema {
    pub namespace: String,
    pub version: u32,
    #[serde(deserialize_with = "unique_map")]
    pub fields: BTreeMap<String, ConfigField>,
}

impl ConfigSchema {
    pub fn validate(&self) -> ConfigResult<()> {
        if self.version == 0 || !valid_name(&self.namespace, true) || self.fields.is_empty() {
            return Err(ConfigError::InvalidSchema(self.namespace.clone()));
        }
        for (name, field) in &self.fields {
            let path = format!("{}.{}", self.namespace, name);
            if !valid_name(name, false) || (field.encrypted && !field.sensitive) {
                return Err(ConfigError::InvalidSchema(path));
            }
            if let ConfigKind::Integer { minimum, maximum } = field.kind
                && minimum.zip(maximum).is_some_and(|(min, max)| min > max)
            {
                return Err(ConfigError::InvalidSchema(path));
            }
            if let Some(environment) = &field.environment
                && (environment.is_empty()
                    || !environment.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                    }))
            {
                return Err(ConfigError::InvalidSchema(path));
            }
            if let Some(value) = &field.default {
                field.validate_value(&path, value)?;
            }
        }
        Ok(())
    }
}

fn valid_name(value: &str, dotted: bool) -> bool {
    let segments: Vec<_> = if dotted {
        value.split('.').collect()
    } else {
        vec![value]
    };
    segments.iter().all(|segment| {
        !segment.is_empty()
            && segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    })
}

pub fn runtime_llm_schema() -> ConfigSchema {
    let mut field = ConfigField::new(
        ConfigKind::Integer {
            minimum: Some(1),
            maximum: None,
        },
        Some(Value::from(10)),
    );
    field.environment = Some("EVE_LLM_MAX_PARALLEL_TOOL_CALLS".into());
    let mut mode = ConfigField::new(ConfigKind::String, Some(Value::from("complete")));
    mode.environment = Some("EVE_LLM_RESPONSE_MODE".into());
    ConfigSchema {
        namespace: LLM_NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (MAX_PARALLEL_TOOL_CALLS.into(), field),
            (RESPONSE_MODE.into(), mode),
        ]),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceValues {
    pub schema_version: u32,
    #[serde(deserialize_with = "unique_map")]
    pub values: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDocument {
    pub revision: u64,
    #[serde(deserialize_with = "unique_map")]
    pub namespaces: ConfigOverrides,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSnapshot {
    pub namespace: String,
    pub schema_version: u32,
    pub revision: u64,
    #[serde(deserialize_with = "unique_map")]
    pub values: BTreeMap<String, Value>,
}

impl ConfigSnapshot {
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> ConfigResult<T> {
        let path = format!("{}.{}", self.namespace, key);
        let value = self
            .values
            .get(key)
            .ok_or_else(|| ConfigError::MissingValue(path.clone()))?;
        serde_json::from_value(value.clone()).map_err(|_| ConfigError::InvalidValue(path))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmRuntimeConfig {
    pub max_parallel_tool_calls: usize,
    pub response_mode: String,
}

impl TryFrom<&ConfigSnapshot> for LlmRuntimeConfig {
    type Error = ConfigError;
    fn try_from(snapshot: &ConfigSnapshot) -> ConfigResult<Self> {
        if snapshot.namespace != LLM_NAMESPACE || snapshot.schema_version != 1 {
            return Err(ConfigError::InvalidSchema(LLM_NAMESPACE.into()));
        }
        let value: usize = snapshot.get(MAX_PARALLEL_TOOL_CALLS)?;
        if value == 0 {
            return Err(ConfigError::InvalidValue(format!(
                "{LLM_NAMESPACE}.{MAX_PARALLEL_TOOL_CALLS}"
            )));
        }
        let response_mode: String = snapshot.get(RESPONSE_MODE)?;
        if response_mode != "complete" && response_mode != "stream" {
            return Err(ConfigError::InvalidValue(format!(
                "{LLM_NAMESPACE}.{RESPONSE_MODE}"
            )));
        }
        Ok(Self {
            response_mode,
            max_parallel_tool_calls: value,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    Immediate,
    NewRequests,
}

/// 可序列化的请求读取上下文；服务重启后必须重新 begin_request。
/// 它携带公开配置快照，不是身份凭据或授权令牌。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRequest {
    session: String,
    initial: ConfigSnapshot,
    immediate_revision: u64,
}

impl ConfigRequest {
    #[doc(hidden)]
    pub fn new(session: String, initial: ConfigSnapshot, immediate_revision: u64) -> Self {
        Self {
            session,
            initial,
            immediate_revision,
        }
    }
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn initial(&self) -> &ConfigSnapshot {
        &self.initial
    }
    pub fn immediate_revision(&self) -> u64 {
        self.immediate_revision
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigChange {
    pub revision: u64,
    pub restart_required: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigBackup {
    pub document: ConfigDocument,
    pub pinned: bool,
}

/// 同步读取契约；适配器负责传输 DTO。消费者不依赖任何实现类型。
pub trait ConfigService: Send + Sync {
    fn snapshot(&self, namespace: &str, schema_version: u32) -> ConfigResult<ConfigSnapshot>;
    fn begin_request(&self, namespace: &str, schema_version: u32) -> ConfigResult<ConfigRequest>;
    fn read_request(&self, request: &ConfigRequest) -> ConfigResult<ConfigSnapshot>;
}

#[derive(Clone)]
pub struct ConfigServiceHandle(pub Arc<dyn ConfigService>);

/// 管理接口不随读取服务发布，只由组合层显式授予。
pub trait ConfigAdmin: Send + Sync {
    fn current(&self) -> ConfigResult<ConfigDocument>;
    fn replace(
        &self,
        expected_revision: u64,
        overrides: ConfigOverrides,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange>;
    fn backups(&self) -> ConfigResult<Vec<ConfigBackup>>;
    fn pin_backup(&self, revision: u64, pinned: bool) -> ConfigResult<()>;
    fn rollback(
        &self,
        expected_revision: u64,
        revision: u64,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error", content = "detail", rename_all = "snake_case")]
pub enum ConfigError {
    InvalidSchema(String),
    InvalidValue(String),
    MissingValue(String),
    UnknownNamespace(String),
    UnknownField(String),
    SchemaVersion {
        namespace: String,
        expected: u32,
        found: u32,
    },
    RevisionConflict {
        expected: u64,
        found: u64,
    },
    BackupNotFound(u64),
    UnsupportedSensitiveField(String),
    InvalidDocument(String),
    StaleRequest,
    Unavailable,
    Storage(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(path) => write!(f, "配置 Schema 无效：{path}"),
            Self::InvalidValue(path) => write!(f, "配置类型、格式或范围无效：{path}"),
            Self::MissingValue(path) => write!(f, "缺少必填配置：{path}"),
            Self::UnknownNamespace(path) => write!(f, "未知配置命名空间：{path}"),
            Self::UnknownField(path) => write!(f, "未知配置字段：{path}"),
            Self::SchemaVersion {
                namespace,
                expected,
                found,
            } => write!(
                f,
                "配置 {namespace} Schema 版本不兼容：需要 {expected}，实际 {found}"
            ),
            Self::RevisionConflict { expected, found } => {
                write!(f, "配置修订冲突：预期 {expected}，当前 {found}")
            }
            Self::BackupNotFound(revision) => write!(f, "找不到配置备份：{revision}"),
            Self::UnsupportedSensitiveField(path) => {
                write!(f, "本切片尚未提供密钥库，拒绝敏感配置：{path}")
            }
            Self::InvalidDocument(message) => write!(f, "配置文件无效：{message}"),
            Self::StaleRequest => write!(f, "配置服务已重启，请重新创建请求快照"),
            Self::Unavailable => write!(f, "配置服务不可用"),
            Self::Storage(message) => write!(f, "配置存储失败：{message}"),
        }
    }
}
impl std::error::Error for ConfigError {}

/// 拒绝重复映射键，避免 JSON 恢复时静默覆盖配置。
#[doc(hidden)]
pub fn unique_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueVisitor<V>(std::marker::PhantomData<V>);
    impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for UniqueVisitor<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("不含重复键的配置对象")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if values.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("配置对象包含重复键"));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(UniqueVisitor(std::marker::PhantomData))
}
