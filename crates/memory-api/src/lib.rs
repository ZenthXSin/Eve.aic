//! 带来源的交互记忆与明确偏好契约；身份绑定、明确意图判断和投递确认由可信宿主负责。
use eve_session_api::SessionSnapshot;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

pub const MEMORY_PLUGIN_ID: &str = "eve.memory";
pub const MEMORY_FORMAT_VERSION: u32 = 1;
pub const MAX_EVIDENCE: usize = 256;
pub const MAX_PREFERENCES: usize = 256;
/// 所有作用域的偏好历史版本总数；原始证据与历史永不自动淘汰。
pub const MAX_HISTORY: usize = 256;
pub const MAX_TEXT_BYTES: usize = 32_768;
pub const MAX_PREFERENCE_BYTES: usize = 4096;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
pub type MemoryResult<T> = Result<T, MemoryError>;

/// 不自动合并同名用户或跨通道身份。只有可信宿主能绑定读取句柄。
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryScope {
    pub channel: String,
    pub session_id: String,
    pub user_id: String,
}
impl MemoryScope {
    pub fn validate(&self) -> MemoryResult<()> {
        for id in [&self.channel, &self.session_id, &self.user_id] {
            validate_id(id)?;
        }
        Ok(())
    }
}

/// 可信宿主提交的原始用户命令；不是完成的模型回复或模型推断。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserStatement {
    pub evidence_id: String,
    pub message_id: String,
    pub text: String,
    pub at_ms: u64,
}

/// 必须是实际已持久保存的 Session 快照；QQ 等宿主还须先确认面向用户投递成功。
/// 此能力不发布给模型/通道。实现校验 Completed 与可信作用域，仅保留用户和最终回复。
#[derive(Clone)]
pub struct CompletedInteraction {
    pub evidence_id: String,
    pub message_id: String,
    pub at_ms: u64,
    pub snapshot: SessionSnapshot,
    pub turn_id: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum EvidenceSource {
    UserStatement {
        message_id: String,
        text: String,
    },
    CompletedInteraction {
        message_id: String,
        /// 首次导入时快照的修订；重放该轮次时不随新增 Session 轮次更新。
        session_revision: u64,
        turn_id: u64,
        user_text: String,
        assistant_text: String,
    },
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionEvidence {
    pub id: String,
    pub revision: u64,
    pub at_ms: u64,
    pub source: EvidenceSource,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreferenceStatus {
    Confirmed,
    Revoked,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceVersion {
    /// 此条偏好的版本，从 1 连续增加。
    pub revision: u64,
    pub evidence_id: String,
    pub at_ms: u64,
    pub text: String,
    pub status: PreferenceStatus,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preference {
    pub id: String,
    pub text: String,
    pub status: PreferenceStatus,
    pub revision: u64,
    pub history: Vec<PreferenceVersion>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySnapshot {
    pub scope: MemoryScope,
    /// 只属于本作用域；别的用户变更不会改变此修订。
    pub revision: u64,
    pub evidence: Vec<InteractionEvidence>,
    pub preferences: Vec<Preference>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreferenceEvidence {
    Existing(String),
    Statement(UserStatement),
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum PreferenceAction {
    /// 创建尚不存在的偏好 ID；不能覆盖已有或已撤销偏好。
    Confirm { id: String, text: String },
    /// 更正当前已确认偏好，保留所有旧版本；不能隐式恢复撤销记录。
    Correct { id: String, text: String },
    /// 撤销当前已确认偏好，保留最新正文、原始证据和历史。
    Revoke { id: String },
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceChange {
    /// 在当前作用域唯一；重放相同操作先于 CAS 检查，时间变化不算新操作。
    pub operation_id: String,
    pub at_ms: u64,
    pub evidence: PreferenceEvidence,
    pub action: PreferenceAction,
}

/// 已绑定作用域的读取能力；调用方不能自行提供其他用户身份。
pub trait MemoryService: Send + Sync {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot>;
}
#[derive(Clone)]
pub struct MemoryServiceHandle(pub Arc<dyn MemoryService>);

/// 宿主专用管理能力，不得发布到通用目录或传给模型/非可信通道。
pub trait MemoryAdmin: Send + Sync {
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>>;
    /// 相同来源重导入先于 CAS 判断，零写入、零修订；时间变化不算新来源。
    /// 不同正文占用同一 ID，或其他 ID 占用已有 message_id/完成轮次，均拒绝。
    /// 后续 Session 新增轮次不影响重放；首次来源快照修订保持原值。
    fn import_completed(
        &self,
        scope: &MemoryScope,
        expected_revision: u64,
        interaction: CompletedInteraction,
    ) -> MemoryResult<MemorySnapshot>;
    /// 明确意图必须由宿主确认；证据与偏好历史在一次状态提交中保存。
    /// 任何持久化错误都保留旧内存并关闭实例，因为后端可能已提交但未确认。
    fn update_preference(
        &self,
        scope: &MemoryScope,
        expected_revision: u64,
        change: PreferenceChange,
    ) -> MemoryResult<MemorySnapshot>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryError {
    InvalidInput,
    NotFound,
    Conflict,
    StaleRevision,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
}
impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "交互记忆输入无效",
            Self::NotFound => "当前作用域没有该记忆或偏好",
            Self::Conflict => "交互记忆来源或操作标识冲突",
            Self::StaleRevision => "交互记忆修订已变化",
            Self::Unavailable => "交互记忆服务不可用",
            Self::CorruptState => "交互记忆状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该交互记忆状态版本",
            Self::Storage => "交互记忆存储失败；须重新打开确认持久状态",
            Self::LimitReached => "交互记忆容量已满；保留原记录",
        })
    }
}
impl std::error::Error for MemoryError {}

pub fn validate_id(value: &str) -> MemoryResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}
pub fn validate_text(value: &str, limit: usize) -> MemoryResult<()> {
    if value.trim().is_empty() || value.len() > limit || value.contains('\0') {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}

macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
redacted_debug!(
    MemoryScope,
    UserStatement,
    CompletedInteraction,
    EvidenceSource,
    InteractionEvidence,
    PreferenceVersion,
    Preference,
    MemorySnapshot,
    PreferenceEvidence,
    PreferenceAction,
    PreferenceChange
);
