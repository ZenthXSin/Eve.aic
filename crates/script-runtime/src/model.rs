use crate::reject;
use eve_llm_api::{ContextScope, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Result<T> = std::result::Result<T, ToolExecutionError>;
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Python,
    Javascript,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonType {
    String,
    Number,
    Integer,
    Boolean,
    Object,
    Array,
    Null,
}

/// 有界、明确支持的 schema 子集；不默默忽略 JSON Schema 关键字。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSchema {
    pub version: u32,
    pub fields: BTreeMap<String, JsonType>,
}
impl StateSchema {
    pub fn validate(&self, state: &Value) -> Result<()> {
        let object = state.as_object().ok_or_else(|| reject("状态必须是对象"))?;
        if self.version == 0
            || self.fields.len() > 64
            || object.len() != self.fields.len()
            || serde_json::to_vec(state).map_err(reject)?.len() > MAX_MESSAGE_BYTES
        {
            return Err(reject("状态 schema 或字段数无效"));
        }
        for (key, kind) in &self.fields {
            let value = object.get(key).ok_or_else(|| reject("状态缺少字段"))?;
            let valid = match kind {
                JsonType::String => value.is_string(),
                JsonType::Number => value.is_number(),
                JsonType::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
                JsonType::Boolean => value.is_boolean(),
                JsonType::Object => value.is_object(),
                JsonType::Array => value.is_array(),
                JsonType::Null => value.is_null(),
            };
            if !valid {
                return Err(reject(format!("状态字段类型错误: {key}")));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionRef {
    pub script: String,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptSpec {
    pub name: String,
    pub language: Language,
    pub source: String,
    /// 依赖别名绑定不可变版本；运行中不查找最新版本。
    pub dependencies: BTreeMap<String, VersionRef>,
    /// 唯一可通过 RPC 调用的宿主叶子工具；非声明工具直接拒绝。
    pub tools: Vec<String>,
    pub state_schema: StateSchema,
    pub initial_state: Value,
}
impl ScriptSpec {
    pub fn validate(&self) -> Result<()> {
        valid_name(&self.name)?;
        if self.source.is_empty()
            || self.source.len() > MAX_SOURCE_BYTES
            || self.dependencies.len() > 16
            || self.tools.len() > 32
        {
            return Err(reject("脚本内容或依赖超过上限"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for tool in &self.tools {
            valid_name(tool)?;
            if !seen.insert(tool) {
                return Err(reject("重复工具"));
            }
        }
        for (alias, reference) in &self.dependencies {
            valid_name(alias)?;
            valid_name(&reference.script)?;
        }
        self.state_schema.validate(&self.initial_state)
    }
}

pub(crate) fn valid_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    {
        return Err(reject("名称只允许字母、数字、点、下划线和连字符"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedVersion {
    pub reference: VersionRef,
    pub owner: ContextScope,
    pub spec: ScriptSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSnapshot {
    pub revision: u64,
    pub schema: StateSchema,
    pub value: Value,
}

/// 迁移由可信宿主显式提供；只迁移状态数据，不重放历史工具或外部副作用。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateMigration {
    pub from: VersionRef,
    pub expected_revision: u64,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Running,
    Succeeded,
    Rejected,
    Failed,
    Unknown,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub operation: String,
    pub target: String,
    pub status: ReceiptStatus,
    /// 宿主返回值的哈希，不把凭据或完整工具结果复制进账本。
    pub result_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub root_call_id: String,
    pub owner: ContextScope,
    pub version: VersionRef,
    pub request_digest: String,
    pub status: ReceiptStatus,
    pub result: Option<Value>,
    pub evidence: Vec<Evidence>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Preflight {
    pub root: VersionRef,
    pub versions: Vec<VersionRef>,
    pub tools: Vec<String>,
    pub dry_run: bool,
    pub sandbox: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Tool {
        name: String,
        arguments: Value,
    },
    Script {
        dependency: String,
        arguments: Value,
    },
    State {
        value: Value,
    },
    Artifact {
        path: String,
    },
    Write {
        path: String,
        content: String,
    },
    Return {
        value: Value,
    },
}
