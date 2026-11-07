//! 保留当前轮次和已经确认的工具回执，只延后下一次完整模型请求。
use eve_llm_api::{DeferredReplyPolicy, LlmError, ProviderFailureKind};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Default)]
pub(crate) struct DeferredReply {
    used: u32,
    deadline: Option<Instant>,
    last_delay: Option<Duration>,
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
        if !transient || (policy.max_retries != 0 && self.used >= policy.max_retries) {
            return None;
        }
        let now = Instant::now();
        let deadline = *self.deadline.get_or_insert(now + policy.recovery_window);
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return None;
        }
        let delay = self
            .last_delay
            .map_or(policy.initial_delay, |delay| delay.saturating_mul(2))
            .min(policy.max_delay);
        self.last_delay = Some(delay);
        self.used = self.used.saturating_add(1);
        // 最后一段等待仍保留原轮次到截止，宿主到点结束，不能再发出模型请求。
        Some((self.used, delay.min(remaining), deadline))
    }
}
