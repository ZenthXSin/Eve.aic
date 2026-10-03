//! 受信宿主装配的内生反思准入；只派生候选，不代表父任务已完成。
use crate::{ExecutionScope, LoopError, LoopResult};
use eve_cognition_api::CognitionAdmin;
use std::sync::Arc;

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

/// 受信宿主可替换的差距发现与目标派生能力，不负责模型、工具或对外发送。
///
/// 实现必须遵守宿主提供的来源、可见范围和启动派生上限；派生目标、驱动及
/// 因果证据须原子保存，保留父目标修订去重、容量、失败与恢复语义。
/// 无变化时不得改写状态。修订冲突返回 StaleRevision，由宿主下一轮重新读取，
/// 不能重放旧快照；其它错误由宿主停止规划并等待已有执行收尾。
pub trait EndogenousPlanning: Send + Sync {
    fn reconcile(&self, now_ms: u64) -> LoopResult<EndogenousReport>;
}

/// 在认知状态恢复后创建本次启动的规划器；仅交给受信宿主，不发布为通用服务。
///
/// 只在 run 命令中调用一次。实现接收公开管理契约与宿主校验后的范围和预算，
/// 不持有 Kernel 私有能力；替换规划器不会增加模型、工具或对外发送权限。
pub trait EndogenousPlannerFactory: Send + Sync {
    fn create(
        &self,
        admin: Arc<dyn CognitionAdmin>,
        options: EndogenousOptions,
    ) -> LoopResult<Arc<dyn EndogenousPlanning>>;
}
