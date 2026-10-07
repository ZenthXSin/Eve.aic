mod support;

use eve_interest_api::*;
use eve_interest_plugin::{ModelInterestObserver, OBSERVER_VERSION};
use eve_llm_api::{
    ChatRole, LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse,
    ModelSelection, ToolCall,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::*;

struct Mock {
    reply: Result<ModelResponse, LlmError>,
    timeout: Duration,
    pending: bool,
    requests: Mutex<Vec<ModelRequest>>,
}
impl LlmProvider for Mock {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            if self.pending {
                std::future::pending::<()>().await;
            }
            self.reply.clone()
        })
    }
}
struct Resolver(Arc<Mock>);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: self.0.timeout,
        })
    }
}

fn setup(reply: Result<ModelResponse, LlmError>) -> (ModelInterestObserver, Arc<Mock>) {
    let mock = Arc::new(Mock {
        reply,
        timeout: Duration::from_secs(5),
        pending: false,
        requests: Mutex::new(vec![]),
    });
    (
        ModelInterestObserver::new(Arc::new(Resolver(mock.clone()))),
        mock,
    )
}

fn text(value: impl Into<String>) -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::Final { text: value.into() })
}

fn batch(texts: &[&str]) -> ObservationBatch {
    let source = memory(scope("session-a", "alice"), texts);
    ObservationBatch {
        id: "interest-batch-test".into(),
        scope: source.scope,
        observer_version: OBSERVER_VERSION.into(),
        started_at_ms: 100,
        evidence: source.evidence,
        known_interests: vec![],
    }
}

fn valid_output() -> serde_json::Value {
    json!({"updates": [{
        "target": {"new": {"topic": "Mindustry 模组创作"}},
        "statements": [
            {"kind": "interest", "quote": "我喜欢 Mindustry 这个游戏的模组", "evidence_id": "e-1"},
            {"kind": "difficulty", "quote": "不知道怎么创作", "evidence_id": "e-1"}
        ],
        "inferred_need": "可能希望学习如何制作模组"
    }]})
}

#[tokio::test]
async fn one_tool_free_request_carries_only_the_batch_and_generic_rules() {
    let (observer, mock) = setup(text(valid_output().to_string()));
    let input = batch(&[INTEREST]);
    let updates = observer.observe(input.clone()).await.unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].statements[1].kind, StatementKind::Difficulty);
    let request = {
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        requests[0].clone()
    };
    assert!(request.tools.is_empty());
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[0].role, ChatRole::System);
    let rules = request.messages[0].text.as_deref().unwrap();
    // 规则与领域无关：不能用固定游戏名或关键词分支冒充自主学习。
    for domain in ["Mindustry", "模组", "游戏"] {
        assert!(
            !rules.contains(domain),
            "system prompt must stay domain-agnostic: {domain}"
        );
    }
    assert_eq!(
        request.messages[1].text.as_deref().unwrap(),
        serde_json::to_string(&input).unwrap()
    );
    // 留出的其他领域使用同一观察器与契约。
    let held_out = "我最近迷上了水彩画，但调色总是很脏。";
    let (observer, mock) = setup(text(
        json!({"updates": [{
            "target": {"new": {"topic": "水彩画调色"}},
            "statements": [
                {"kind": "interest", "quote": "我最近迷上了水彩画", "evidence_id": "e-1"},
                {"kind": "difficulty", "quote": "调色总是很脏", "evidence_id": "e-1"}
            ]
        }]})
        .to_string(),
    ));
    let updates = observer.observe(batch(&[held_out])).await.unwrap();
    assert_eq!(updates[0].inferred_need, None);
    assert_eq!(
        mock.requests.lock().unwrap()[0].messages[0].text,
        request.messages[0].text
    );
}

#[tokio::test]
async fn malformed_unquoted_or_tool_outputs_are_invalid_without_retry() {
    let mut extra = valid_output();
    extra["updates"][0]["confidence"] = json!(90);
    let mut paraphrased = valid_output();
    paraphrased["updates"][0]["statements"][0]["quote"] = json!("用户很喜欢模组");
    let mut assistant = valid_output();
    assistant["updates"][0]["statements"][0]["quote"] = json!("Mindustry 模组很好玩");
    let mut unknown_kind = valid_output();
    unknown_kind["updates"][0]["statements"][0]["kind"] = json!("goal");
    let cases = vec![
        format!("```json\n{}\n```", valid_output()),
        extra.to_string(),
        paraphrased.to_string(),
        assistant.to_string(),
        unknown_kind.to_string(),
        r#"{"updates":[],"updates":[]}"#.into(),
        format!("{}{}", valid_output(), " ".repeat(MAX_OUTPUT_BYTES)),
    ];
    for output in cases {
        let (observer, mock) = setup(text(output));
        assert_eq!(
            observer.observe(batch(&[INTEREST])).await.err(),
            Some(InterestError::Observation(
                ObservationFailure::InvalidOutput
            ))
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
    let (observer, _) = setup(Ok(ModelResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "forbidden".into(),
            name: "must_not_execute".into(),
            arguments: json!({}),
        }],
    }));
    assert_eq!(
        observer.observe(batch(&[INTEREST])).await.err(),
        Some(InterestError::Observation(
            ObservationFailure::InvalidOutput
        ))
    );
    let (observer, _) = setup(Err(LlmError::ProviderTimeout));
    assert_eq!(
        observer.observe(batch(&[INTEREST])).await.err(),
        Some(InterestError::Observation(ObservationFailure::Timeout))
    );
    let (observer, _) = setup(Err(LlmError::Provider("private".into())));
    assert_eq!(
        observer.observe(batch(&[INTEREST])).await.err(),
        Some(InterestError::Observation(ObservationFailure::Provider))
    );
    let (empty, _) = setup(text(r#"{"updates":[]}"#));
    assert!(empty.observe(batch(&[INTEREST])).await.unwrap().is_empty());
}

#[tokio::test]
async fn slow_provider_times_out_and_foreign_versions_never_call_the_model() {
    let mock = Arc::new(Mock {
        reply: text(valid_output().to_string()),
        timeout: Duration::from_millis(20),
        pending: true,
        requests: Mutex::new(vec![]),
    });
    let observer = ModelInterestObserver::new(Arc::new(Resolver(mock.clone())));
    assert_eq!(
        observer.observe(batch(&[INTEREST])).await.err(),
        Some(InterestError::Observation(ObservationFailure::Timeout))
    );
    let mut foreign = batch(&[INTEREST]);
    foreign.observer_version = "another-observer".into();
    assert_eq!(
        observer.observe(foreign).await.err(),
        Some(InterestError::InvalidInput)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}
