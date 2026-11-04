//! 保留当前轮次和已经确认的工具回执，只延后下一次完整模型请求。
use eve_llm_api::{DeferredReplyPolicy, LlmError, ProviderFailureKind};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Default)]
pub(crate) struct DeferredReply {
    used: u32,
    deadline: Option<Instant>,
}

impl DeferredReply {
    pub(crate) fn next(
        &mut self,
        policy: DeferredReplyPolicy,
        error: &LlmError,
    ) -> Option<(u32, Duration, Instant)> {
        let transient = matches!(error, LlmError::ProviderTimeout)
            || error.provider_failure().is_some_and(|failure| {
                failure.kind == ProviderFailureKind::Network
                    || matches!(failure.http_status, Some(408 | 429 | 500 | 502 | 503 | 504))
            });
        if !transient || self.used >= policy.max_retries {
            return None;
        }
        let now = Instant::now();
        let deadline = *self.deadline.get_or_insert(now + policy.recovery_window);
        let delay = policy
            .initial_delay
            .saturating_mul(1 << self.used)
            .min(policy.max_delay);
        if deadline.saturating_duration_since(now) <= delay {
            return None;
        }
        self.used += 1;
        Some((self.used, delay, deadline))
    }
}
