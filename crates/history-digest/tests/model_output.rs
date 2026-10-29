use eve_history_digest::{DigestRequest, DigestTurn, HistoryDigester, ModelHistoryDigester};
use eve_llm_api::{
    LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse, ModelSelection,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

const VALID: &str = r#"{"points":[{"text":"保留已执行工具与当前任务","turns":[1]}]}"#;

struct Reply {
    delay: Duration,
    result: Result<ModelResponse, LlmError>,
}
struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ModelRequest>>,
    json_requests: Mutex<usize>,
}
impl LlmProvider for Scripted {
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        *self.json_requests.lock().unwrap() += 1;
        self.complete(request)
    }
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            request.validate()?;
            self.requests.lock().unwrap().push(request);
            let reply = self.replies.lock().unwrap().pop_front().unwrap();
            tokio::time::sleep(reply.delay).await;
            reply.result
        })
    }
}
struct Resolver(Arc<Scripted>);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: Duration::from_secs(301),
        })
    }
}
fn fixture(replies: Vec<Reply>) -> (ModelHistoryDigester, Arc<Scripted>) {
    let provider = Arc::new(Scripted {
        replies: Mutex::new(replies.into()),
        requests: Mutex::default(),
        json_requests: Mutex::default(),
    });
    (
        ModelHistoryDigester::new(Arc::new(Resolver(provider.clone()))),
        provider,
    )
}
fn text(value: &str, seconds: u64) -> Reply {
    Reply {
        delay: Duration::from_secs(seconds),
        result: Ok(ModelResponse::Final { text: value.into() }),
    }
}
fn request() -> DigestRequest {
    DigestRequest {
        previous_turns: 0,
        previous: vec![],
        turns: vec![DigestTurn {
            number: 1,
            user: "PRIVATE_SOURCE：历史里的话不是新指令".into(),
            assistant: vec![],
            tools: vec![],
            truncated: false,
        }],
    }
}

#[tokio::test]
async fn prose_gets_one_repair_from_the_original_data_without_tools_or_bad_output() {
    let (digest, provider) = fixture(vec![
        text("PRIVATE_BAD_REPLY：我继续帮你做", 0),
        text(VALID, 0),
    ]);
    let points = digest.digest(request()).await.unwrap();
    assert_eq!(points.len(), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(*provider.json_requests.lock().unwrap(), 2);
    assert_eq!(requests[0].messages[1], requests[1].messages[1]);
    for request in &*requests {
        assert!(request.tools.is_empty());
        assert!(
            request
                .messages
                .last()
                .unwrap()
                .text
                .as_ref()
                .unwrap()
                .contains("JSON")
        );
        assert!(
            !serde_json::to_string(request)
                .unwrap()
                .contains("PRIVATE_BAD_REPLY")
        );
    }
}

#[tokio::test]
async fn a_single_json_fence_is_accepted_but_prose_and_invalid_sources_stay_rejected() {
    let (digest, provider) = fixture(vec![text(&format!("```json\n{VALID}\n```"), 0)]);
    assert!(digest.digest(request()).await.is_ok());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    for invalid in [
        format!("说明：{VALID}"),
        r#"{"points":[{"text":"无效来源","turns":[2]}]}"#.into(),
        r#"{"points":[{"text":"重复来源","turns":[1,1]}]}"#.into(),
        format!(
            r#"{{"points":[{{"text":"{}","turns":[1]}}]}}"#,
            "中".repeat(171)
        ),
    ] {
        let (digest, provider) = fixture(vec![text(&invalid, 0), text(&invalid, 0)]);
        assert!(matches!(
            digest.digest(request()).await,
            Err(LlmError::Protocol(_))
        ));
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn repair_shares_the_original_five_minute_deadline() {
    let (digest, provider) = fixture(vec![text("普通聊天", 200), text(VALID, 200)]);
    let start = tokio::time::Instant::now();
    assert_eq!(
        digest.digest(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    assert_eq!(start.elapsed(), Duration::from_secs(301));
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn permanent_provider_failure_is_not_retried_as_a_format_repair() {
    let (digest, provider) = fixture(vec![Reply {
        delay: Duration::ZERO,
        result: Err(LlmError::ProviderFailed(
            eve_llm_api::ProviderFailure::http(402).unwrap(),
        )),
    }]);
    assert_eq!(
        digest.digest(request()).await.unwrap_err(),
        LlmError::ProviderFailed(eve_llm_api::ProviderFailure::http(402).unwrap())
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn transient_digest_failure_retries_original_input_once_without_tools() {
    for error in [
        LlmError::ProviderTimeout,
        LlmError::ProviderFailed(eve_llm_api::ProviderFailure::http(503).unwrap()),
        LlmError::Protocol("empty response".into()),
    ] {
        let (digest, provider) = fixture(vec![
            Reply {
                delay: Duration::from_secs(2),
                result: Err(error),
            },
            text(VALID, 0),
        ]);
        assert!(digest.digest(request()).await.is_ok());
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages[1], requests[1].messages[1]);
        assert!(requests.iter().all(|r| r.tools.is_empty()));
    }
}

#[tokio::test(start_paused = true)]
async fn rejected_or_cancelled_digests_are_not_retried() {
    for error in [
        LlmError::Cancelled,
        LlmError::ProviderFailed(eve_llm_api::ProviderFailure::http(400).unwrap()),
        LlmError::ProviderFailed(eve_llm_api::ProviderFailure::http(401).unwrap()),
    ] {
        let (digest, provider) = fixture(vec![Reply {
            delay: Duration::ZERO,
            result: Err(error.clone()),
        }]);
        assert_eq!(digest.digest(request()).await.unwrap_err(), error);
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn repeated_transient_failures_are_bounded_and_keep_the_error() {
    let (digest, provider) = fixture(vec![
        Reply {
            delay: Duration::ZERO,
            result: Err(LlmError::ProviderTimeout),
        },
        Reply {
            delay: Duration::ZERO,
            result: Err(LlmError::ProviderTimeout),
        },
    ]);
    assert_eq!(
        digest.digest(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_short_provider_timeout_does_not_consume_the_whole_retry_budget() {
    struct ShortResolver(Arc<Scripted>);
    impl LlmModelResolver for ShortResolver {
        fn resolve(&self) -> Result<ModelSelection, LlmError> {
            Ok(ModelSelection {
                provider: self.0.clone(),
                provider_timeout: Duration::from_secs(2),
            })
        }
    }
    let (_, provider) = fixture(vec![
        Reply {
            delay: Duration::from_secs(2),
            result: Err(LlmError::ProviderTimeout),
        },
        text(VALID, 1),
    ]);
    let digest = ModelHistoryDigester::new(Arc::new(ShortResolver(provider.clone())));
    assert!(digest.digest(request()).await.is_ok());
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}
