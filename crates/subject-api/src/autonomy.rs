//! 持续自主运行的周期预算；总目标序号独立于本周期用量。
use crate::{SubjectError, SubjectResult, SubjectScope, validate_id, validate_scope};
use serde::{Deserialize, Serialize};

pub const AUTONOMY_DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutonomyWindow {
    pub started_at_ms: u64,
    pub period_ms: u64,
    pub used: u32,
}
impl AutonomyWindow {
    pub fn validate(&self, limit: u32) -> SubjectResult<()> {
        if self.started_at_ms == 0
            || self.period_ms != AUTONOMY_DAY_MS
            || !(1..=20).contains(&limit)
            || self.used > limit
            || self.started_at_ms.checked_add(self.period_ms).is_none()
        {
            return Err(SubjectError::InvalidInput);
        }
        Ok(())
    }
    pub fn next_at_ms(&self) -> SubjectResult<u64> {
        self.started_at_ms
            .checked_add(self.period_ms)
            .ok_or(SubjectError::InvalidInput)
    }
}

/// 宿主操作者的显式启动授权；不是模型参数，也不伪装为一条 QQ 消息。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAutonomySeed {
    pub version: u32,
    pub authorization_id: String,
    pub scope: SubjectScope,
    pub max_goals: u32,
    pub steps_per_goal: u32,
    pub interests: Vec<String>,
    pub review_messages: Vec<String>,
}
impl HostAutonomySeed {
    pub fn validate(&self) -> SubjectResult<()> {
        validate_scope(&self.scope)?;
        validate_id(&self.authorization_id)?;
        if self.version != 1
            || !(1..=20).contains(&self.max_goals)
            || !(1..=550).contains(&self.steps_per_goal)
            || self.interests.is_empty()
            || self.interests.len() > 8
            || self
                .interests
                .iter()
                .any(|v| v.trim().is_empty() || v.len() > 120)
            || self.review_messages.is_empty()
            || self.review_messages.len() > 6
            || self.review_messages.iter().any(|v| v.trim().is_empty())
            || serde_json::to_vec(self)
                .map_err(|_| SubjectError::InvalidInput)?
                .len()
                > 6000
        {
            return Err(SubjectError::InvalidInput);
        }
        Ok(())
    }
}

/// 宿主保留的自主执行历史；独立于有限关注视图和当前兴趣。
/// 保留调用凭据只表示曾预留派发，不能据此宣称任务已成功。
pub trait AutonomyMemoryStore: Send + Sync {
    fn host_authorization_seen(&self, scope: &SubjectScope) -> SubjectResult<bool>;
    fn record_host_authorization(
        &self,
        scope: &SubjectScope,
        id: &str,
        digest: &str,
        now: u64,
    ) -> SubjectResult<()>;
    /// 按稳定问题查询首次预留的自主关注 ID；换授权、窗口或进程不能重做同一问题。
    fn problem_focus(&self, scope: &SubjectScope, problem: &str) -> SubjectResult<Option<String>>;
    /// 同问题同关注可幂等核对，另一个关注必须拒绝；提交不明时保留记录并停止。
    fn reserve_problem(
        &self,
        scope: &SubjectScope,
        problem: &str,
        focus: &str,
        now: u64,
    ) -> SubjectResult<()>;
    /// 已结算任务无需因热关注回收而反复重新登记。
    fn task_reconciled(
        &self,
        scope: &SubjectScope,
        task: &str,
        outcome: &str,
    ) -> SubjectResult<bool>;
    fn record_task_reconciled(
        &self,
        scope: &SubjectScope,
        task: &str,
        outcome: &str,
        now: u64,
    ) -> SubjectResult<()>;
}
