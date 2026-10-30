//! 绑定原始作用域和修订的只读证据扫描；分页不改变原文，也不授予跨作用域权限。
use crate::*;
use std::collections::VecDeque;

#[derive(Clone)]
pub struct MemoryScanSummary {
    pub scope: MemoryScope,
    pub revision: u64,
    pub preferences: Vec<Preference>,
    pub evidence_count: u64,
}

/// 创建后固定本次读取的热层和不可变归档索引。每页至多 MAX_ARCHIVE_PAGE 条，
/// 全部证据按修订严格递增；空页表示结束。损坏或缺块必须返回错误。
/// 内置游标在服务关闭后拒绝后续读取；旧实现默认游标是已经交付的不可变快照副本。
/// 读取错误后调用方须丢弃游标并重新创建，不能跳过失败页继续使用部分视图。
/// 消费者仍须核查作用域、计数、来源唯一性及偏好引用，不能只信摘要。
pub trait MemoryEvidenceCursor: Send {
    fn summary(&self) -> &MemoryScanSummary;
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>>;
}

pub(crate) struct SnapshotCursor {
    summary: MemoryScanSummary,
    evidence: VecDeque<InteractionEvidence>,
}
impl SnapshotCursor {
    pub(crate) fn new(mut snapshot: MemorySnapshot) -> MemoryResult<Self> {
        if snapshot.archived_evidence != 0 {
            return Err(MemoryError::CorruptState);
        }
        snapshot.evidence.sort_by_key(|e| e.revision);
        Ok(Self {
            summary: MemoryScanSummary {
                scope: snapshot.scope,
                revision: snapshot.revision,
                preferences: snapshot.preferences,
                evidence_count: snapshot.evidence.len() as u64,
            },
            evidence: snapshot.evidence.into(),
        })
    }
}
impl MemoryEvidenceCursor for SnapshotCursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        let count = self.evidence.len().min(MAX_ARCHIVE_PAGE);
        Ok(self.evidence.drain(..count).collect())
    }
}
