//! 跨窗口经验契约。公开实践经验、可信身份个人经验与原会话资料分层，不能通过正文声明扩大范围。
use crate::{MemoryResult, MemoryScope};
use serde::{Deserialize, Serialize};

pub const EXPERIENCE_MEMORY_PLUGIN_ID: &str = "eve.memory-experience";
pub const MAX_EXPERIENCES: usize = 256;
pub const MAX_EXPERIENCE_VERSIONS: usize = 1024;
pub const MAX_EXPERIENCE_TEXT_BYTES: usize = 2048;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ExperienceVisibility {
    Subject,
    Principal { principal: String },
    Scope { scope: MemoryScope },
}
/// 仅宿主构造；principal 必须来自认证映射，不能是昵称或正文。
#[derive(Clone)]
pub struct ExperienceReadContext {
    pub scope: MemoryScope,
    pub principal: Option<String>,
}
/// 宿主认证映射；不能按昵称或QQ openid相等猜测跨场景为同一个人。
pub trait ExperiencePrincipalResolver: Send + Sync {
    fn resolve(&self, scope: &MemoryScope) -> MemoryResult<Option<String>>;
}
impl<F> ExperiencePrincipalResolver for F
where
    F: Fn(&MemoryScope) -> MemoryResult<Option<String>> + Send + Sync,
{
    fn resolve(&self, scope: &MemoryScope) -> MemoryResult<Option<String>> {
        self(scope)
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ExperienceSource {
    /// 由可信宿主确认可公开的资料/实践回执。模型接口不得构造此变体。
    PublicArtifact { reference: String, revision: u64 },
    /// 引用只用于本 scope 追溯；不得将其原话提升为主体通用经验。
    ScopedEvidence {
        scope: MemoryScope,
        reference: String,
        revision: u64,
    },
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExperienceStatus {
    Unverified,
    Verified,
    Revoked,
    Archived,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceVersion {
    pub revision: u64,
    pub operation_id: String,
    pub at_ms: u64,
    /// 宿主设定的有效截止时间；旧记录无期限，模型不能续期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    pub text: String,
    pub source: ExperienceSource,
    pub status: ExperienceStatus,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceRecord {
    pub id: String,
    pub visibility: ExperienceVisibility,
    pub history: Vec<ExperienceVersion>,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceSnapshot {
    pub revision: u64,
    pub records: Vec<ExperienceRecord>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceHit {
    pub id: String,
    pub visibility: ExperienceVisibility,
    pub version: ExperienceVersion,
    pub score: usize,
}
/// 不包含管理接口的可替换读取契约，自动上下文只依赖此接口。
pub trait ExperienceRecall: Send + Sync {
    fn recall(
        &self,
        context: &ExperienceReadContext,
        query: &str,
        limit: usize,
    ) -> MemoryResult<Vec<ExperienceHit>>;
}
/// 只供可信宿主。save_local 的来源必须绑定当前scope，publish_public 不向模型开放。
pub trait ExperienceMemoryAdmin: ExperienceRecall {
    fn snapshot(&self) -> MemoryResult<ExperienceSnapshot>;
    fn save_local(
        &self,
        context: &ExperienceReadContext,
        expected_revision: u64,
        id: &str,
        version: ExperienceVersion,
    ) -> MemoryResult<()>;
    fn publish_public(
        &self,
        expected_revision: u64,
        id: &str,
        version: ExperienceVersion,
    ) -> MemoryResult<()>;
    /// 个人经验由宿主显式确认身份和跨场景分享意图后保存；模型只能保存scope经验。
    fn save_personal(
        &self,
        context: &ExperienceReadContext,
        expected_revision: u64,
        id: &str,
        version: ExperienceVersion,
    ) -> MemoryResult<()>;
    /// 更正保留旧版；更正内容不能自动继承已验证状态。撤销与归档立即停止自动召回。
    fn revise(
        &self,
        context: &ExperienceReadContext,
        expected_revision: u64,
        id: &str,
        version: ExperienceVersion,
    ) -> MemoryResult<()>;
    /// 验证/撤销公开经验由宿主执行，不经模型身份授权。
    fn revise_public(
        &self,
        expected_revision: u64,
        id: &str,
        version: ExperienceVersion,
    ) -> MemoryResult<()>;
}

impl ExperienceVersion {
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        self.expires_at_ms
            .is_some_and(|deadline| now_ms >= deadline)
    }
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok());
        now.is_none_or(|now| self.is_expired_at(now))
    }
}

impl ExperienceHit {
    /// 组合层校验可替换召回器的范围与状态，拒绝把另一个scope的正文接入模型。
    pub fn validate_for(&self, context: &ExperienceReadContext) -> MemoryResult<()> {
        use crate::{MemoryError, validate_id, validate_text};
        context.scope.validate()?;
        validate_id(&self.id)?;
        validate_id(&self.version.operation_id)?;
        validate_text(&self.version.text, MAX_EXPERIENCE_TEXT_BYTES)?;
        if self.version.is_expired()
            || self.score == 0
            || self.version.revision == 0
            || matches!(
                self.version.status,
                ExperienceStatus::Revoked | ExperienceStatus::Archived
            )
        {
            return Err(MemoryError::InvalidInput);
        }
        let authorized = match &self.visibility {
            ExperienceVisibility::Subject => {
                matches!(self.version.source, ExperienceSource::PublicArtifact { .. })
            }
            ExperienceVisibility::Principal { principal } => {
                validate_id(principal)?;
                context.principal.as_ref() == Some(principal)
                    && matches!(self.version.source, ExperienceSource::ScopedEvidence { .. })
            }
            ExperienceVisibility::Scope { scope } => {
                scope == &context.scope
                    && matches!(&self.version.source, ExperienceSource::ScopedEvidence { scope: source, .. } if source == scope)
            }
        };
        if !authorized {
            return Err(MemoryError::InvalidInput);
        }
        let (reference, revision) = match &self.version.source {
            ExperienceSource::PublicArtifact {
                reference,
                revision,
            } => (reference, revision),
            ExperienceSource::ScopedEvidence {
                scope,
                reference,
                revision,
            } => {
                scope.validate()?;
                (reference, revision)
            }
        };
        validate_id(reference)?;
        if *revision == 0 {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}
