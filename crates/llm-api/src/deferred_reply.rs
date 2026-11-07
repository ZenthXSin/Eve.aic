//! 完整模型响应失败后的同轮延后接续策略；不决定发言内容，也不重放工具。
use crate::LlmError;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeferredReplyPolicy {
    /// 整轮共享的额外模型请求次数；0 表示只受 recovery_window 限制。
    /// 显式设置 1..=8 时，次数和总期限同时生效，成功返回工具后也不重置。
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    /// 从首次可恢复失败开始计算的等待与再次请求总期限。
    pub recovery_window: Duration,
}

impl DeferredReplyPolicy {
    pub fn validate(self) -> Result<(), LlmError> {
        if self.max_retries > 8
            || self.initial_delay.is_zero()
            || self.initial_delay > self.max_delay
            || self.max_delay > self.recovery_window
            || self.recovery_window > Duration::from_secs(900)
        {
            return Err(LlmError::Configuration(
                "延后回复的重试次数、退避和总期限无效".into(),
            ));
        }
        Ok(())
    }
}
