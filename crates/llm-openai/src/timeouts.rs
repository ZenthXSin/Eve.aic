//! 流式传输的分阶段期限；只记录固定阶段与计时，不记录请求或响应内容。
use eve_llm_api::LlmError;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamTimeoutPolicy {
    /// 从发出请求到收到首个非空响应块，包含等待响应头。
    pub first_response: Duration,
    /// 两个非空响应块之间允许的最长停顿。
    pub idle: Duration,
    /// 完整请求的总期限；启用重试时也包含所有尝试与退避。
    pub total: Duration,
}
impl StreamTimeoutPolicy {
    pub fn validate(self) -> Result<(), LlmError> {
        if self.first_response.is_zero()
            || self.idle.is_zero()
            || self.total.is_zero()
            || self.first_response > self.total
            || self.idle > self.total
        {
            return Err(LlmError::Configuration(
                "流式首包、停顿期限须大于零且不超过总期限".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn expired(
    phase: &str,
    limit: Duration,
    started: Instant,
    received: Option<usize>,
) -> LlmError {
    eprintln!(
        "EVE_MODEL_TIMEOUT {}",
        serde_json::json!({
            "phase":phase,"limit_ms":limit.as_millis(),
            "at_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()),
            "elapsed_ms":started.elapsed().as_millis(),"received_bytes":received,
        })
    );
    LlmError::ProviderTimeout
}
