use eve_history_digest::{DigestRequest, DigestTurn, HistoryDigester, ModelHistoryDigester};
use eve_llm_api::{
    LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse, ModelSelection,
};
use std::{sync::Arc, time::Duration};

struct Delayed(Duration);
impl LlmProvider for Delayed {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            request.validate()?;
            assert!(request.tools.is_empty());
            tokio::time::sleep(self.0).await;
            Ok(ModelResponse::Final {
                text: r#"{"points":[{"text":"保留当前任务与已验证进度","turns":[1]}]}"#.into(),
            })
        })
    }
}
struct Resolver(Duration);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: Arc::new(Delayed(self.0)),
            provider_timeout: Duration::from_secs(301),
        })
    }
}
fn request() -> DigestRequest {
    DigestRequest {
        previous_turns: 0,
        previous: vec![],
        turns: vec![DigestTurn {
            number: 1,
            user: "继续核验现有产物".into(),
            assistant: vec![],
            tools: vec![],
            truncated: false,
        }],
    }
}

#[tokio::test(start_paused = true)]
async fn a_digest_can_outlive_the_old_180_second_wrapper() {
    let digest = ModelHistoryDigester::new(Arc::new(Resolver(Duration::from_secs(200))));
    let points = digest.digest(request()).await.unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].turns, [1]);
}

#[tokio::test(start_paused = true)]
async fn a_stuck_digest_is_bounded_by_five_minutes_plus_adapter_grace() {
    let digest = ModelHistoryDigester::new(Arc::new(Resolver(Duration::from_secs(310))));
    let start = tokio::time::Instant::now();
    assert_eq!(
        digest.digest(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    assert_eq!(start.elapsed(), Duration::from_secs(301));
    assert_eq!(
        eve_llm_api::CONTEXT_COMPACTION_TIMEOUT,
        Duration::from_secs(300)
    );
}
