//! 原始证据归档契约：热层之外的不可变分块与按来源读取。
//!
//! 热层（`MemorySnapshot::evidence`）只保留近期或仍被偏好引用的证据；更早的完成交互
//! 移入不可变分块，修订照常计入作用域修订，可按修订顺序逐页读取原文。归档不删除、
//! 不改写、不摘要替换任何原始证据。
use crate::{InteractionEvidence, MemoryError, MemoryResult};
use serde::{Deserialize, Serialize};

/// 单页最多返回的归档证据数。
pub const MAX_ARCHIVE_PAGE: usize = 64;

/// 可替换的不可变分块存储；只由可信宿主装配，不发布给模型或通道。
///
/// `put` 成功即表示内容已持久；同名分块已存在时实现必须核对内容完全一致，
/// 不同则返回 `Conflict`，不得覆盖。实现不提供删除。
pub trait EvidenceArchiveStore: Send + Sync {
    fn put(&self, name: &str, bytes: &[u8]) -> MemoryResult<()>;
    fn get(&self, name: &str) -> MemoryResult<Option<Vec<u8>>>;
    /// 已存在的分块名，用于启动核对与报告未被索引引用的分块。
    fn list(&self) -> MemoryResult<Vec<String>>;
}

/// 本作用域已归档证据的一页，按修订升序；`next_after` 为空表示已读完。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedEvidencePage {
    pub evidence: Vec<InteractionEvidence>,
    pub next_after: Option<u64>,
}

/// 宿主面板与诊断使用的容量和归档状态；不含正文。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryArchiveStatus {
    /// 是否装配了归档存储；未装配时热层满即拒绝新增。
    pub enabled: bool,
    pub hot_evidence: usize,
    pub hot_bytes: usize,
    pub chunks: u64,
    pub archived_evidence: u64,
    pub archived_bytes: u64,
    /// 存在于存储但未被索引引用的分块（例如提交前中断）；保留不删。
    pub unreferenced_chunks: usize,
    /// 最近一次归档失败的原因；成功归档后清空。不影响已提交的状态。
    pub last_error: Option<String>,
}

/// 校验单页请求上限。
pub fn validate_archive_page_limit(limit: usize) -> MemoryResult<()> {
    if limit == 0 || limit > MAX_ARCHIVE_PAGE {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}
