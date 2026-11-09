//! 每次受管执行独立创建的预算；先预留额度，再发起模型请求或工具批次。
use crate::LlmError;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    pub max_model_requests: u16,
    pub max_tool_calls: u16,
    pub timeout_ms: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetUsage {
    /// 已准入的请求；消费者关闭或请求失败也不退还额度。
    pub model_requests: u64,
    /// 已准入的工具调用，包含随后校验失败或没有实际启动的调用。
    pub admitted_tool_calls: u64,
}
#[derive(Debug)]
pub struct TurnBudget {
    limits: ExecutionLimits,
    deadline: Instant,
    usage: Mutex<BudgetUsage>,
}
impl TurnBudget {
    pub fn new(limits: ExecutionLimits) -> Result<Self, LlmError> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(limits.timeout_ms))
            .ok_or_else(|| LlmError::Configuration("执行预算无效".into()))?;
        Self::with_deadline(limits, deadline)
    }
    /// 沿用宿主先前确定的期限；绑定和排队不会重新授予执行时间。
    pub fn with_deadline(limits: ExecutionLimits, deadline: Instant) -> Result<Self, LlmError> {
        if !(1..=100).contains(&limits.max_model_requests)
            || limits.max_tool_calls > 1000
            || !(1..=600_000).contains(&limits.timeout_ms)
        {
            return Err(LlmError::Configuration("执行预算无效".into()));
        }
        Ok(Self {
            limits,
            deadline: deadline.min(Instant::now() + Duration::from_millis(limits.timeout_ms)),
            usage: Mutex::new(BudgetUsage::default()),
        })
    }
    pub fn usage(&self) -> Result<BudgetUsage, LlmError> {
        self.usage
            .lock()
            .map(|usage| *usage)
            .map_err(|_| LlmError::Backend("执行预算锁不可用".into()))
    }
    fn check_deadline(&self) -> Result<(), LlmError> {
        if Instant::now() >= self.deadline {
            Err(LlmError::Configuration("执行总期限已耗尽".into()))
        } else {
            Ok(())
        }
    }
    pub fn reserve_model_request(&self) -> Result<(), LlmError> {
        self.check_deadline()?;
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| LlmError::Backend("执行预算锁不可用".into()))?;
        if usage.model_requests >= u64::from(self.limits.max_model_requests) {
            return Err(LlmError::Configuration("模型请求预算已耗尽".into()));
        }
        usage.model_requests += 1;
        Ok(())
    }
    /// 整批准入；同时保留至少一个模型请求额度，用于回传工具结果。
    pub fn reserve_tool_batch(&self, calls: usize) -> Result<(), LlmError> {
        self.check_deadline()?;
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| LlmError::Backend("执行预算锁不可用".into()))?;
        let calls =
            u64::try_from(calls).map_err(|_| LlmError::Configuration("工具批次超过预算".into()))?;
        let next = usage
            .admitted_tool_calls
            .checked_add(calls)
            .ok_or_else(|| LlmError::Configuration("工具调用计数溢出".into()))?;
        if next > u64::from(self.limits.max_tool_calls)
            || usage.model_requests >= u64::from(self.limits.max_model_requests)
        {
            return Err(LlmError::Configuration("工具或后续模型请求预算不足".into()));
        }
        usage.admitted_tool_calls = next;
        Ok(())
    }
}

/// 同一主任务与全部子代理共享的工具步数；与模型请求预算独立，不跨任务共用。
#[derive(Clone, Debug)]
pub struct SharedToolSteps {
    inner: std::sync::Arc<SharedStepsInner>,
    parent_reserve: usize,
}
#[derive(Debug)]
struct SharedStepsInner {
    limit: usize,
    usage: Mutex<SharedStepUsage>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SharedStepUsage {
    pub tool_calls: usize,
    pub model_requests: usize,
}
impl SharedToolSteps {
    pub fn new(limit: usize) -> Result<Self, LlmError> {
        if limit == 0 {
            return Err(LlmError::Configuration("共享步数上限不能为零".into()));
        }
        Ok(Self {
            inner: std::sync::Arc::new(SharedStepsInner {
                limit,
                usage: Mutex::new(SharedStepUsage::default()),
            }),
            parent_reserve: 0,
        })
    }
    pub fn limit(&self) -> usize {
        self.inner.limit
    }
    /// 创建共享同一计数的子代理视图，给父执行者保留收尾额度。
    /// 嵌套或再次克隆只能保持/收窄视图，不能恢复父视图的余额。
    pub fn with_parent_reserve(&self, reserve: usize) -> Self {
        Self {
            inner: self.inner.clone(),
            parent_reserve: self.parent_reserve.max(reserve).min(self.limit()),
        }
    }
    pub fn parent_reserve(&self) -> usize {
        self.parent_reserve
    }
    pub fn usage(&self) -> Result<SharedStepUsage, LlmError> {
        self.inner
            .usage
            .lock()
            .map(|u| *u)
            .map_err(|_| LlmError::Backend("共享步数不可用".into()))
    }
    pub fn remaining(&self) -> Result<usize, LlmError> {
        Ok(self
            .limit()
            .saturating_sub(self.parent_reserve)
            .saturating_sub(self.usage()?.tool_calls))
    }
    /// 整批准入，失败不扣减；调用失败、拒绝或取消不返还已准入步数。
    pub fn reserve(&self, count: usize) -> Result<bool, LlmError> {
        let mut usage = self
            .inner
            .usage
            .lock()
            .map_err(|_| LlmError::Backend("共享步数不可用".into()))?;
        let Some(next) = usage
            .tool_calls
            .checked_add(count)
            .filter(|n| *n <= self.limit().saturating_sub(self.parent_reserve))
        else {
            return Ok(false);
        };
        usage.tool_calls = next;
        Ok(true)
    }
    pub fn record_model_request(&self) -> Result<(), LlmError> {
        let mut usage = self
            .inner
            .usage
            .lock()
            .map_err(|_| LlmError::Backend("共享步数不可用".into()))?;
        usage.model_requests = usage
            .model_requests
            .checked_add(1)
            .ok_or_else(|| LlmError::Backend("共享模型计数溢出".into()))?;
        Ok(())
    }
    /// 仅在独立后台任务恢复、尚未准入新调用时恢复已持久化计数。
    pub fn restore(&self, prior: SharedStepUsage) -> Result<(), LlmError> {
        let mut usage = self
            .inner
            .usage
            .lock()
            .map_err(|_| LlmError::Backend("共享步数不可用".into()))?;
        if *usage != SharedStepUsage::default() || prior.tool_calls > self.limit() {
            return Err(LlmError::Context("共享步数恢复无效；未重置预算".into()));
        }
        *usage = prior;
        Ok(())
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;
    #[test]
    fn parallel_children_and_parent_cannot_overspend_the_same_pool() {
        let pool = SharedToolSteps::new(50).unwrap();
        assert!(pool.reserve(10).unwrap());
        let threads: Vec<_> = (0..20)
            .map(|_| {
                let child = pool.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        let _ = child.reserve(1).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(pool.usage().unwrap().tool_calls, 50);
        assert_eq!(pool.remaining().unwrap(), 0);
        assert!(!pool.reserve(1).unwrap());
        assert_eq!(SharedToolSteps::new(50).unwrap().remaining().unwrap(), 50);
    }
    #[test]
    fn restored_children_are_charged_and_failed_batch_is_not_partially_admitted() {
        let pool = SharedToolSteps::new(10).unwrap();
        pool.restore(SharedStepUsage {
            tool_calls: 7,
            model_requests: 4,
        })
        .unwrap();
        assert!(!pool.reserve(4).unwrap());
        assert_eq!(pool.remaining().unwrap(), 3);
        assert!(pool.reserve(2).unwrap());
        assert!(pool.restore(SharedStepUsage::default()).is_err());
        assert_eq!(pool.remaining().unwrap(), 1);
        pool.record_model_request().unwrap();
        assert_eq!(pool.usage().unwrap().model_requests, 5);
    }

    #[test]
    fn parallel_delegates_cannot_spend_the_parents_completion_reserve() {
        let parent = SharedToolSteps::new(100).unwrap();
        assert!(parent.reserve(7).unwrap());
        let child = parent.with_parent_reserve(20);
        let threads: Vec<_> = (0..12)
            .map(|_| {
                let child = child.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let _ = child.reserve(1).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(parent.usage().unwrap().tool_calls, 80);
        assert_eq!(child.remaining().unwrap(), 0);
        assert_eq!(parent.remaining().unwrap(), 20);
        assert!(!child.reserve(1).unwrap());
        // 父线程完成核验、记忆写入、发言后同样不能超支。
        assert!(parent.reserve(20).unwrap());
        assert!(!parent.reserve(1).unwrap());
        assert_eq!(parent.usage().unwrap().tool_calls, 100);
    }

    #[test]
    fn delegated_reserve_tracks_parent_spending_and_cannot_be_reclaimed() {
        let parent = SharedToolSteps::new(12).unwrap();
        let child = parent.with_parent_reserve(3);
        assert_eq!(child.remaining().unwrap(), 9);
        assert!(parent.reserve(2).unwrap());
        assert_eq!(child.remaining().unwrap(), 7);
        // 整批超额不部分扣费，已准入后失败则不退还。
        assert!(!child.reserve(8).unwrap());
        assert_eq!(parent.usage().unwrap().tool_calls, 2);
        assert!(child.reserve(7).unwrap());
        let cloned = child.with_parent_reserve(0);
        assert_eq!(cloned.parent_reserve(), 3);
        assert!(!cloned.reserve(1).unwrap());
        assert_eq!(parent.remaining().unwrap(), 3);
        assert_eq!(
            parent.with_parent_reserve(usize::MAX).remaining().unwrap(),
            0
        );
    }

    #[test]
    fn restored_parent_and_child_keep_spent_steps_without_regranting_the_reserve() {
        let parent = SharedToolSteps::new(100).unwrap();
        parent
            .restore(SharedStepUsage {
                tool_calls: 76,
                model_requests: 20,
            })
            .unwrap();
        let child = parent.with_parent_reserve(20);
        assert_eq!(parent.remaining().unwrap(), 24);
        assert_eq!(child.remaining().unwrap(), 4);
        assert!(child.reserve(4).unwrap());
        assert!(child.restore(SharedStepUsage::default()).is_err());
        assert!(!child.reserve(1).unwrap());
        assert!(parent.reserve(20).unwrap());
        assert_eq!(parent.usage().unwrap().tool_calls, 100);
    }
}
