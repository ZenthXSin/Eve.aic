use crate::{LearningError, LearningJob, LearningResult};
use eve_memory_api::MemoryScope;
use std::collections::VecDeque;

pub const MAX_LEARNING_PAGE: usize = 16;

/// 空页表示结束；错误后不能跳过失败页继续使用。内置游标固定创建时的版本，
/// 服务停止后拒绝读取；旧实现默认返回已经交付的快照副本。
pub trait LearningHistoryCursor<T>: Send {
    fn scope(&self) -> &MemoryScope;
    fn next_page(&mut self) -> LearningResult<Vec<T>>;
}
pub struct SnapshotHistoryCursor<T> {
    scope: MemoryScope,
    records: VecDeque<T>,
}
impl<T> SnapshotHistoryCursor<T> {
    pub fn new(scope: MemoryScope, records: Vec<T>) -> LearningResult<Self> {
        scope.validate().map_err(|_| LearningError::InvalidInput)?;
        Ok(Self {
            scope,
            records: records.into(),
        })
    }
}
impl<T: Send> LearningHistoryCursor<T> for SnapshotHistoryCursor<T> {
    fn scope(&self) -> &MemoryScope {
        &self.scope
    }
    fn next_page(&mut self) -> LearningResult<Vec<T>> {
        let count = self.records.len().min(MAX_LEARNING_PAGE);
        Ok(self.records.drain(..count).collect())
    }
}
pub(crate) fn jobs_scope(scope: &MemoryScope, jobs: &[LearningJob]) -> LearningResult<()> {
    if jobs.iter().any(|j| &j.batch.scope != scope) {
        return Err(LearningError::Conflict);
    }
    Ok(())
}
