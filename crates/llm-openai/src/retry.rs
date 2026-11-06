//! 只重复尚未交给宿主的完整模型请求；请求体、工具回执及总期限保持不变。
use crate::OpenAiProvider;
use eve_llm_api::{LlmError, ModelTextSink, ProtocolFailureKind, ProviderFailureKind};
use std::time::Duration;
use tokio::time::{Instant, sleep};

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
        headers: &reqwest::header::HeaderMap,
        sink: Option<&dyn ModelTextSink>,
        deadline: Instant,
        diagnostics: &mut crate::timing::CallDiagnostics,
    ) -> Result<crate::completion::CompletedRequest, LlmError> {
        let mut retried_arguments = false;
        for attempt in 0..=3 {
            let mut retry_after = None;
            let identity = diagnostics.next_attempt();
            let result = self
                .request_once(body, headers, sink, &mut retry_after, identity)
                .await;
            let Err(error) = &result else { return result };
            let invalid_arguments =
                error.protocol_failure() == Some(ProtocolFailureKind::ToolArguments);
            if let Some(kind) = error.protocol_failure() {
                let mut fields = identity.fields();
                fields["kind"] = serde_json::json!(kind);
                eprintln!("EVE_MODEL_PROTOCOL_FAILURE {fields}");
            }
            // 只重取尚未交给宿主的参数批次，绝不修补或执行无效 JSON。
            // 前面已经执行的工具回执仍在同一请求体中；一次恢复也计入原四次总上限。
            let recover_arguments =
                self.recover_tool_arguments && invalid_arguments && !retried_arguments;
            if attempt == 3 || (!transient(error) && !recover_arguments) {
                return result;
            }
            let delay = Duration::from_secs(1 << attempt).max(retry_after.unwrap_or_default());
            if deadline.saturating_duration_since(Instant::now()) <= delay {
                return result;
            }
            retried_arguments |= recover_arguments;
            // 固定元数据，不记录请求体、端点、密钥、响应正文或工具输出。
            let mut fields = identity.fields();
            fields["retry"] = serde_json::json!(attempt + 1);
            fields["max_retries"] = serde_json::json!(3);
            fields["delay_ms"] = serde_json::json!(delay.as_millis());
            fields["http_status"] = serde_json::json!(error.provider_http_status());
            fields["reason"] = serde_json::json!(if recover_arguments {
                "invalid_tool_arguments"
            } else if matches!(error, LlmError::ProviderTimeout) {
                "timeout"
            } else {
                "transient"
            });
            eprintln!("EVE_MODEL_RETRY {fields}");
            sleep(delay).await;
        }
        unreachable!("bounded attempts return their last result")
    }
}
