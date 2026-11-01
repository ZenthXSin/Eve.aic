//! 带来源的交互记忆与明确偏好契约；身份绑定、明确意图判断和投递确认由可信宿主负责。
mod scan;
pub use scan::*;
mod context_load;
pub use context_load::*;
mod experience;
pub use experience::*;
mod unified;
pub use unified::*;
mod archive;
pub use archive::*;
mod continuity;
mod distillation;
pub use continuity::*;
mod recall;
mod summary;
pub use distillation::*;
pub use summary::*;

pub use recall::*;

use eve_session_api::SessionSnapshot;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

pub const MEMORY_PLUGIN_ID: &str = "eve.memory";
pub const MEMORY_FORMAT_VERSION: u32 = 1;
/// 热层全局证据容量；仍受 16 MiB 字节上限约束。装配归档存储后，较早的完成交互
/// 移入不可变分块而不是被清理；未装配时满即拒绝新增。
pub const MAX_EVIDENCE: usize = 4096;
/// 可信作用域（通道 + 会话 + 用户）总数。群聊每位成员各占一个作用域；满后新作用域的
/// 交流不写入记忆（返回 `LimitReached`，原记录不变），已有作用域照常采集。
pub const MAX_SCOPES: usize = 1024;
pub const MAX_PREFERENCES: usize = 256;
/// 所有作用域的偏好历史版本总数；原始证据与历史永不自动淘汰。
pub const MAX_HISTORY: usize = 256;
pub const MAX_TEXT_BYTES: usize = 32_768;
pub const MAX_PREFERENCE_BYTES: usize = 4096;
/// 热层文档的序列化字节上限；归档分块不计入。
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
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
    /// 热层证据：近期完成交互、全部偏好命令及仍被偏好引用的证据。
    pub evidence: Vec<InteractionEvidence>,
    pub preferences: Vec<Preference>,
    /// 已移入归档分块的完成交互数；它们的修订仍计入 `revision`，
    /// 原文经 `MemoryService::archived` 按修订顺序读取。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub archived_evidence: u64,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl MemorySnapshot {
    /// 不在热层的证据是否可能已归档：修订有效、热层没有同 ID 或同修订的不同记录。
    /// 只用于消费者核对自己保存的旧来源副本；原文核对须经 `MemoryService::archived`。
    pub fn may_be_archived(&self, evidence: &InteractionEvidence) -> bool {
        self.archived_evidence > 0
            && evidence.revision > 0
            && evidence.revision <= self.revision
            && matches!(evidence.source, EvidenceSource::CompletedInteraction { .. })
            && !self
                .evidence
                .iter()
                .any(|hot| hot.id == evidence.id || hot.revision == evidence.revision)
    }
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
    /// 轻量修订查询，只用于唤醒/调度，不能代替正文和归档完整校验。
    fn revision(&self) -> MemoryResult<u64> {
        self.snapshot().map(|s| s.revision)
    }
    fn snapshot(&self) -> MemoryResult<MemorySnapshot>;
    /// 一致性逐页证据视图。默认兼容旧实现，先取得完整快照；内置归档实现覆盖为逐块读取。
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        Ok(Box::new(scan::SnapshotCursor::new(
            self.complete_snapshot()?,
        )?))
    }

    /// 按修订升序读取本作用域修订大于 `after_revision` 的归档证据，最多 `limit` 条。
    /// 分块缺失、校验失败或不属于本作用域时返回 `CorruptState`，不跳过。
    fn archived(&self, after_revision: u64, limit: usize) -> MemoryResult<ArchivedEvidencePage> {
        let _ = (after_revision, limit);
        Err(MemoryError::Unavailable)
    }
    /// 本作用域的完整记忆：热层与全部归档证据按修订升序合并，`archived_evidence` 为 0。
    /// 归档只是存储形式，回看历史的消费者（召回、摘要、提炼、观察）读这个视图，不会漏掉旧原文。
    /// 分块缺失或校验失败时返回 `CorruptState`，不跳过。
    ///
    /// 默认实现先读快照再逐页读归档，两次读取之间发生归档时可能报告不一致；
    /// 存储实现应在同一次读取中取得热层与分块索引。
    fn complete_snapshot(&self) -> MemoryResult<MemorySnapshot> {
        let mut snapshot = self.snapshot()?;
        if snapshot.archived_evidence == 0 {
            return Ok(snapshot);
        }
        let mut archived = Vec::new();
        let mut after = 0;
        loop {
            let page = self.archived(after, MAX_ARCHIVE_PAGE)?;
            // 不信任实现返回的游标与计数，避免坏页造成无限读取或无界累计。
            if page.evidence.len() > MAX_ARCHIVE_PAGE
                || (archived.len() as u64)
                    .checked_add(page.evidence.len() as u64)
                    .is_none_or(|count| count > snapshot.archived_evidence)
            {
                return Err(MemoryError::CorruptState);
            }
            let mut last = after;
            for item in &page.evidence {
                if item.revision <= last || item.revision > snapshot.revision {
                    return Err(MemoryError::CorruptState);
                }
                last = item.revision;
            }
            if page
                .next_after
                .is_some_and(|next| next != last || next <= after)
            {
                return Err(MemoryError::CorruptState);
            }
            archived.extend(page.evidence);
            match page.next_after {
                Some(next) => after = next,
                None => break,
            }
        }
        if archived.len() as u64 != snapshot.archived_evidence {
            return Err(MemoryError::CorruptState);
        }
        archived.append(&mut snapshot.evidence);
        archived.sort_by_key(|item| item.revision);
        let mut ids = std::collections::HashSet::new();
        if archived.iter().any(|item| !ids.insert(&item.id))
            || archived
                .windows(2)
                .any(|pair| pair[0].revision == pair[1].revision)
        {
            return Err(MemoryError::CorruptState);
        }
        snapshot.evidence = archived;
        snapshot.archived_evidence = 0;
        Ok(snapshot)
    }
}
#[derive(Clone)]
pub struct MemoryServiceHandle(pub Arc<dyn MemoryService>);

/// 宿主专用管理能力，不得发布到通用目录或传给模型/非可信通道。
pub trait MemoryAdmin: Send + Sync {
    /// 可信宿主枚举已持久保存的范围，用于低频扫描；默认实现明确不支持。
    /// 不向通用服务目录或模型暴露此管理能力。
    fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
        Err(MemoryError::Unavailable)
    }
    /// 给定通道的轻量范围/修订视图，只供后台调度，不证明归档正文已验证；默认兼容旧后端。
    fn revisions(&self, channel: &str) -> MemoryResult<Vec<(MemoryScope, u64)>> {
        validate_id(channel)?;
        let mut scopes = self.scopes()?;
        scopes.retain(|scope| scope.channel == channel);
        scopes.sort();
        scopes.dedup();
        scopes
            .into_iter()
            .map(|scope| {
                let revision = self.reader(scope.clone())?.revision()?;
                Ok((scope, revision))
            })
            .collect()
    }
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>>;
    /// 热层容量与归档状态；默认实现明确不支持。
    fn archive_status(&self) -> MemoryResult<MemoryArchiveStatus> {
        Err(MemoryError::Unavailable)
    }
    /// 相同来源重导入先于 CAS 判断，零写入、零修订；时间变化不算新来源。
    /// 不同正文占用同一 ID，或其他 ID 占用已有 message_id/完成轮次，均拒绝。
    /// 后续 Session 新增轮次不影响重放；首次来源快照修订保持原值。
    fn import_completed(
        &self,
        scope: &MemoryScope,
        expected_revision: u64,
        interaction: CompletedInteraction,
    ) -> MemoryResult<MemorySnapshot>;
    /// 确认必须由可信宿主授权（用户命令或显式开启的自主学习策略）；
    /// 自动确认引用真实 Existing 交互，不能伪造 UserStatement。证据与历史一次提交。
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
    PreferenceChange,
    VerbatimExcerpt,
    ConversationEpisode,
    ExpressionExample,
    ConversationDigest,
    SummarySource,
    SummaryQuote,
    SummaryPoint,
    SummaryBatch,
    SummaryJob,
    ArchivedEvidencePage
);

#[cfg(test)]
mod complete_snapshot_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Pages {
        snapshot: MemorySnapshot,
        pages: Vec<ArchivedEvidencePage>,
        calls: AtomicUsize,
    }
    impl MemoryService for Pages {
        fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
            Ok(self.snapshot.clone())
        }
        fn archived(&self, _: u64, _: usize) -> MemoryResult<ArchivedEvidencePage> {
            self.pages
                .get(self.calls.fetch_add(1, Ordering::SeqCst))
                .cloned()
                .ok_or(MemoryError::Unavailable)
        }
    }
    fn evidence(id: &str, revision: u64) -> InteractionEvidence {
        InteractionEvidence {
            id: id.into(),
            revision,
            at_ms: 1,
            source: EvidenceSource::CompletedInteraction {
                message_id: format!("m-{revision}"),
                session_revision: revision * 2,
                turn_id: revision,
                user_text: "原文".into(),
                assistant_text: "回复".into(),
            },
        }
    }
    fn source() -> Pages {
        Pages {
            snapshot: MemorySnapshot {
                scope: MemoryScope {
                    channel: "test".into(),
                    session_id: "s".into(),
                    user_id: "u".into(),
                },
                revision: 3,
                evidence: vec![evidence("c", 3)],
                preferences: vec![],
                archived_evidence: 2,
            },
            pages: vec![ArchivedEvidencePage {
                evidence: vec![evidence("a", 1), evidence("b", 2)],
                next_after: None,
            }],
            calls: AtomicUsize::new(0),
        }
    }
    #[test]
    fn complete_snapshot_rejects_nonadjacent_duplicate_ids() {
        let mut source = source();
        source.snapshot.evidence[0].id = "a".into();
        assert!(matches!(
            source.complete_snapshot(),
            Err(MemoryError::CorruptState)
        ));
    }
    #[test]
    fn complete_snapshot_rejects_nonprogressing_archive_cursor_before_next_read() {
        let mut source = source();
        source.pages = vec![
            ArchivedEvidencePage {
                evidence: vec![evidence("a", 1)],
                next_after: Some(0),
            },
            ArchivedEvidencePage {
                evidence: vec![evidence("b", 2)],
                next_after: None,
            },
        ];
        assert!(matches!(
            source.complete_snapshot(),
            Err(MemoryError::CorruptState)
        ));
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn complete_snapshot_accepts_monotonic_pages_and_preserves_raw_evidence() {
        let source = source();
        let snapshot = source.complete_snapshot().unwrap();
        assert_eq!(
            snapshot.evidence,
            vec![evidence("a", 1), evidence("b", 2), evidence("c", 3)]
        );
        assert_eq!(snapshot.archived_evidence, 0);
    }
}
