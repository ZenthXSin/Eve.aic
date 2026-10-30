//! 绑定原始作用域和修订的只读证据扫描；分页不改变原文，也不授予跨作用域权限。
use crate::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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

/// 校验可替换游标的范围、全局来源唯一性、修订和偏好引用。
/// 只有读到空页，全部视图才验证完成；中途错误后不可继续读取。
pub struct ValidatedMemoryCursor {
    inner: Box<dyn MemoryEvidenceCursor>,
    summary: MemoryScanSummary,
    index: ScanIndex,
    completed: bool,
    failed: bool,
}
impl ValidatedMemoryCursor {
    pub fn new(scope: MemoryScope, inner: Box<dyn MemoryEvidenceCursor>) -> MemoryResult<Self> {
        let summary = inner.summary().clone();
        summary
            .scope
            .validate()
            .map_err(|_| MemoryError::CorruptState)?;
        if summary.scope != scope || summary.preferences.len() > MAX_PREFERENCES {
            return Err(MemoryError::CorruptState);
        }
        Ok(Self {
            inner,
            summary,
            index: ScanIndex::default(),
            completed: false,
            failed: false,
        })
    }
    /// 仅在完整扫描校验后提供来源修订，不返回不完整视图的验证结果。
    pub fn evidence_revision(&self, id: &str) -> Option<u64> {
        if !self.completed || self.failed {
            return None;
        }
        self.index.ids.get(id).copied()
    }
    fn read_checked(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        let page = self.inner.next_page()?;
        if page.len() > MAX_ARCHIVE_PAGE {
            return Err(MemoryError::CorruptState);
        }
        for evidence in &page {
            self.index.observe(evidence, &self.summary)?;
        }
        if page.is_empty() {
            self.index.finish(&self.summary)?;
            self.completed = true;
        }
        Ok(page)
    }
}
impl MemoryEvidenceCursor for ValidatedMemoryCursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        if self.failed {
            return Err(MemoryError::Unavailable);
        }
        // 结束后仍询问底层，让服务关闭检测和非法追加页检查继续生效。
        let result = self.read_checked();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
/// 仅保存唯一性与来源引用核验所需元数据，不保留扫描过的原文。
#[derive(Default)]
struct ScanIndex {
    ids: BTreeMap<String, u64>,
    messages: BTreeSet<String>,
    turns: BTreeSet<u64>,
    statements: BTreeSet<String>,
    last_revision: u64,
}
impl ScanIndex {
    fn observe(
        &mut self,
        evidence: &InteractionEvidence,
        summary: &MemoryScanSummary,
    ) -> MemoryResult<()> {
        let invalid = || MemoryError::CorruptState;
        validate_id(&evidence.id).map_err(|_| invalid())?;
        if self.ids.len() as u64 >= summary.evidence_count
            || evidence.revision <= self.last_revision
            || evidence.revision > summary.revision
            || self
                .ids
                .insert(evidence.id.clone(), evidence.revision)
                .is_some()
        {
            return Err(invalid());
        }
        self.last_revision = evidence.revision;
        let message = match &evidence.source {
            EvidenceSource::UserStatement { message_id, text } => {
                validate_text(text, MAX_TEXT_BYTES).map_err(|_| invalid())?;
                self.statements.insert(evidence.id.clone());
                message_id
            }
            EvidenceSource::CompletedInteraction {
                message_id,
                session_revision,
                turn_id,
                user_text,
                assistant_text,
            } => {
                validate_text(user_text, MAX_TEXT_BYTES).map_err(|_| invalid())?;
                validate_text(assistant_text, MAX_TEXT_BYTES).map_err(|_| invalid())?;
                if *turn_id == 0
                    || turn_id.checked_mul(2).is_none_or(|n| *session_revision < n)
                    || !self.turns.insert(*turn_id)
                {
                    return Err(invalid());
                }
                message_id
            }
        };
        validate_id(message).map_err(|_| invalid())?;
        if !self.messages.insert(message.clone()) {
            return Err(invalid());
        }
        Ok(())
    }
    fn finish(&self, summary: &MemoryScanSummary) -> MemoryResult<()> {
        if self.ids.len() as u64 != summary.evidence_count {
            return Err(MemoryError::CorruptState);
        }
        let mut preferences = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        let mut count = 0usize;
        for preference in &summary.preferences {
            validate_id(&preference.id).map_err(|_| MemoryError::CorruptState)?;
            validate_text(&preference.text, MAX_PREFERENCE_BYTES)
                .map_err(|_| MemoryError::CorruptState)?;
            count = count
                .checked_add(preference.history.len())
                .ok_or(MemoryError::CorruptState)?;
            if count > MAX_HISTORY
                || !preferences.insert(&preference.id)
                || preference.revision == 0
                || preference.revision > summary.revision
                || preference.revision != preference.history.len() as u64
            {
                return Err(MemoryError::CorruptState);
            }
            for (index, version) in preference.history.iter().enumerate() {
                validate_id(&version.evidence_id).map_err(|_| MemoryError::CorruptState)?;
                validate_text(&version.text, MAX_PREFERENCE_BYTES)
                    .map_err(|_| MemoryError::CorruptState)?;
                if version.revision != index as u64 + 1
                    || !self.ids.contains_key(&version.evidence_id)
                {
                    return Err(MemoryError::CorruptState);
                }
                referenced.insert(&version.evidence_id);
                if version.status == PreferenceStatus::Revoked
                    && (index == 0
                        || index + 1 != preference.history.len()
                        || version.text != preference.history[index - 1].text)
                {
                    return Err(MemoryError::CorruptState);
                }
            }
            let last = preference.history.last().ok_or(MemoryError::CorruptState)?;
            if last.revision != preference.revision
                || last.text != preference.text
                || last.status != preference.status
            {
                return Err(MemoryError::CorruptState);
            }
        }
        if summary.revision != (self.turns.len() + count) as u64
            || self.statements.iter().any(|id| !referenced.contains(id))
        {
            return Err(MemoryError::CorruptState);
        }
        Ok(())
    }
}
