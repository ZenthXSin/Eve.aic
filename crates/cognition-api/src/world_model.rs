//! 宿主绑定来源的环境证据、执行前预测和独立回执；不提供执行或授权能力。
use crate::{
    CognitionError, CognitionResult, ReadAccess, Source, SourceKind, Visibility, validate_id,
    validate_text,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const WORLD_MODEL_PLUGIN_ID: &str = "eve.world-model";
pub const WORLD_MODEL_STATE_KEY: &str = "world-model.v1";
pub const MAX_WORLD_RECORDS: usize = 256;
pub const MAX_WORLD_BYTES: usize = 4 * 1024 * 1024;

pub fn world_digest(value: &impl Serialize) -> CognitionResult<String> {
    let bytes = serde_json::to_vec(value).map_err(|_| CognitionError::InvalidInput)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
pub fn valid_world_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldScope {
    /// 同一能力在不同运行环境下分别学习；宿主应包括模型、工具、配置修订和授权范围。
    pub environment_sha256: String,
    pub capability: String,
    pub visibility: Visibility,
}
impl WorldScope {
    pub fn validate(&self) -> CognitionResult<()> {
        if !valid_world_digest(&self.environment_sha256) {
            return Err(CognitionError::InvalidInput);
        }
        validate_id(&self.capability)?;
        self.visibility.validate()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldObservation {
    pub id: String,
    pub scope: WorldScope,
    pub source: Source,
    pub summary: String,
    /// 原始证据摘要；不复制凭证、私信或完整工具输出到能力画像。
    pub evidence_sha256: String,
    pub at_ms: u64,
    pub expires_at_ms: u64,
}
impl WorldObservation {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.id)?;
        self.scope.validate()?;
        self.source.validate()?;
        validate_text(&self.summary)?;
        if !matches!(
            self.source.kind,
            SourceKind::Tool | SourceKind::Environment | SourceKind::User
        ) || !valid_world_digest(&self.evidence_sha256)
            || self.at_ms == 0
            || self.expires_at_ms <= self.at_ms
        {
            return Err(CognitionError::InvalidInput);
        }
        Ok(())
    }
}

/// 执行前由宿主固定的验收点；模型不能事后选择成功工具来冒充效果。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldCheck {
    pub tool: String,
    pub arguments_sha256: String,
    pub output_pointer: String,
    pub expected: Value,
}
impl WorldCheck {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.tool)?;
        if matches!(
            self.tool.as_str(),
            "echo" | "task_progress" | "dispatch_task" | "run_subagent"
        ) || !valid_world_digest(&self.arguments_sha256)
            || self.output_pointer.len() > 256
            || (!self.output_pointer.is_empty() && !self.output_pointer.starts_with('/'))
            || serde_json::to_vec(&self.expected)
                .map_err(|_| CognitionError::InvalidInput)?
                .len()
                > 4096
        {
            return Err(CognitionError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldPrediction {
    pub id: String,
    pub execution_id: String,
    pub scope: WorldScope,
    pub observation_ids: Vec<String>,
    pub check: WorldCheck,
    pub success_probability_percent: u8,
    pub created_at_ms: u64,
    pub deadline_ms: u64,
}
impl WorldPrediction {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.id)?;
        validate_id(&self.execution_id)?;
        self.scope.validate()?;
        self.check.validate()?;
        if self.success_probability_percent > 100
            || self.created_at_ms == 0
            || self.deadline_ms <= self.created_at_ms
            || self.observation_ids.len() > 16
        {
            return Err(CognitionError::InvalidInput);
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &self.observation_ids {
            validate_id(id)?;
            if !seen.insert(id) {
                return Err(CognitionError::InvalidInput);
            }
        }
        Ok(())
    }
}

/// 只能由拥有真实执行回执的宿主提交，不对模型工具目录发布。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldReceipt {
    pub execution_id: String,
    /// 宿主在取得回执时重新读取的环境指纹，不能沿用预测时缓存的配置。
    pub environment_sha256: String,
    pub source: Source,
    pub tool: String,
    pub arguments_sha256: String,
    pub output: Value,
    pub succeeded: bool,
    /// 是否最后一次影响产物的工具批次；检查后又修改产物不能算成功。
    pub final_batch: bool,
    pub at_ms: u64,
}
impl WorldReceipt {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.execution_id)?;
        validate_id(&self.tool)?;
        self.source.validate()?;
        if self.source.kind != SourceKind::Tool
            || !valid_world_digest(&self.environment_sha256)
            || !valid_world_digest(&self.arguments_sha256)
            || self.at_ms == 0
            || serde_json::to_vec(&self.output)
                .map_err(|_| CognitionError::InvalidInput)?
                .len()
                > 8192
        {
            return Err(CognitionError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorldEffect {
    Verified,
    Failed,
    Inconclusive,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldResolution {
    pub receipt: Option<WorldReceipt>,
    pub effect: WorldEffect,
    pub at_ms: u64,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldTrial {
    pub prediction: WorldPrediction,
    pub resolution: Option<WorldResolution>,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldModelSnapshot {
    pub revision: u64,
    pub observations: Vec<WorldObservation>,
    pub trials: Vec<WorldTrial>,
}

/// 对外只读投影不包含完整工具输出，过期观测的历史引用仅保留其 ID。
/// 该类型不是可恢复的持久化快照，也不能交回管理接口替换账本。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldTrialView {
    pub prediction: WorldPrediction,
    pub effect: Option<WorldEffect>,
    pub receipt_sha256: Option<String>,
    pub resolved_at_ms: Option<u64>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldModelView {
    pub revision: u64,
    pub observations: Vec<WorldObservation>,
    pub trials: Vec<WorldTrialView>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEstimate {
    pub scope: WorldScope,
    pub verified: u32,
    pub failed: u32,
    pub inconclusive: u32,
    pub pending: u32,
    /// Beta(1,1) 平滑后成功率，未知能力是 50，不能当成已验证能力。
    pub success_probability_percent: u8,
    /// 预测成功率与实际 0/100 的平均绝对偏差；没有确定结果时为 None。
    pub forecast_error_percent: Option<u8>,
    pub last_evidence_at_ms: Option<u64>,
}

pub trait WorldModelReader: Send + Sync {
    /// 只返回当前环境、未过期的观测及当前环境的历史预测；读权限由宿主绑定。
    fn snapshot(&self, environment_sha256: &str, now_ms: u64) -> CognitionResult<WorldModelView>;
    fn capabilities(&self, environment_sha256: &str) -> CognitionResult<Vec<CapabilityEstimate>>;
}
pub trait WorldModelAdmin: Send + Sync {
    fn snapshot(&self) -> CognitionResult<WorldModelSnapshot>;
    fn reader(&self, access: ReadAccess) -> CognitionResult<std::sync::Arc<dyn WorldModelReader>>;
    fn observe(&self, observation: WorldObservation) -> CognitionResult<()>;
    /// 必须在实际执行前调用并确认保存成功，相同 ID 不允许换内容。
    fn predict(&self, prediction: WorldPrediction) -> CognitionResult<()>;
    /// 空回执表示中断、无可用证据或执行不确定，不计成功也不计已知失败。
    fn resolve(
        &self,
        prediction_id: &str,
        receipt: Option<WorldReceipt>,
        at_ms: u64,
    ) -> CognitionResult<WorldEffect>;
}

macro_rules! redacted {
    ($($ty:ty),+) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) }
    })+ };
}
redacted!(
    WorldScope,
    WorldObservation,
    WorldCheck,
    WorldPrediction,
    WorldReceipt,
    WorldResolution,
    WorldTrial,
    WorldModelSnapshot,
    WorldModelView,
    WorldTrialView,
    CapabilityEstimate
);
