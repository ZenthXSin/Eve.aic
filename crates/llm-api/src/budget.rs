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
