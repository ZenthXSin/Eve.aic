//! 受信宿主装配的内生反思准入；只派生候选，不代表父任务已完成。
use crate::{ExecutionScope, LoopError, LoopResult};

/// 单次宿主启动的反思派生上限；每个父目标修订至多派生一次。
#[derive(Clone, Debug)]
pub struct EndogenousOptions {
    /// 仅此范围内、来源获准的 Waiting 目标可成为反思输入。
    pub scope: ExecutionScope,
    pub max_derivations: u16,
    /// 子目标还会继承父目标更短的预算和有效期。
    pub timeout_ms: u64,
}

impl EndogenousOptions {
    pub fn validate(&self) -> LoopResult<()> {
        self.scope.validate()?;
        if !(1..=32).contains(&self.max_derivations) || !(1..=30_000).contains(&self.timeout_ms) {
            return Err(LoopError::InvalidInput);
        }
        Ok(())
    }
}

/// reconcile 每次至多原子派生一个目标；两个列表都为空时没有写入状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndogenousReport {
    pub created_goal_ids: Vec<String>,
    /// 父待办已修订、撤销或过期的旧 Ready 候选；历史记录仍保留。
    pub invalidated_goal_ids: Vec<String>,
    pub revision: u64,
}
