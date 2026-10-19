//! 只重复尚未交给宿主的完整模型请求；请求体、工具回执及总期限保持不变。
use crate::OpenAiProvider;
use eve_llm_api::{LlmError, ModelResponse, ProviderFailureKind};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout_at};

fn transient(error: &LlmError) -> bool {
    matches!(error, LlmError::ProviderTimeout)
        || error.provider_failure().is_some_and(|failure| {
            failure.kind == ProviderFailureKind::Network
                || matches!(failure.http_status, Some(408 | 429 | 500 | 502 | 503 | 504))
        })
}

impl OpenAiProvider {
    pub(crate) async fn request_retried(
        &self,
        body: &serde_json::Value,
    ) -> Result<ModelResponse, LlmError> {
        let deadline = Instant::now() + self.config.request_timeout;
        timeout_at(deadline, async {
            for attempt in 0..=3 {
                let mut retry_after = None;
                let result = self.request_once(body, None, &mut retry_after).await;
                let Err(error) = &result else { return result };
                if attempt == 3 || !transient(error) {
                    return result;
                }
                let delay = Duration::from_secs(1 << attempt).max(retry_after.unwrap_or_default());
                if deadline.saturating_duration_since(Instant::now()) <= delay {
                    return result;
                }
                // 固定元数据，不记录请求体、端点、密钥、响应正文或工具输出。
                eprintln!(
                    "EVE_MODEL_RETRY {}",
                    serde_json::json!({"retry":attempt + 1,"max_retries":3,
                        "delay_ms":delay.as_millis(),"http_status":error.provider_http_status(),
                        "reason":if matches!(error,LlmError::ProviderTimeout) {"timeout"} else {"transient"}})
                );
                sleep(delay).await;
            }
            unreachable!("bounded attempts return their last result")
        })
        .await
        .map_err(|_| LlmError::ProviderTimeout)?
    }
}
