#[path = "support/session.rs"]
mod fixture;
use eve_control_api::{ControlPhase, GenerationKey};
use eve_llm_api::{
    ChatRole, LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse,
    ModelSelection,
};
use eve_message_api::*;
use eve_message_diagnostics::{BoundedRelationDiagnostics, DiagnosticCoverage};
use eve_runtime::LlmRelationJudge;
use fixture::{Provider, Step, final_response, key};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

fn input(text: &str) -> RelationInput {
    RelationInput {
        message: IncomingMessage {
            message_id: "private-message-id".into(),
            target: GenerationKey {
                session: key("private-session-id"),
                task_id: "private-task-id".into(),
                controller_epoch: [7; 16],
                generation: 1,
            },
            text: text.into(),
            reply_to: None,
        },
        phase: ControlPhase::Generating,
        task_text: "生成三页报告".into(),
        cancel_requested: false,
        started_tools: Some(0),
        clarification: None,
    }
}

fn wire(intent: &str, confidence: u8, text: Option<&str>) -> String {
    json!({
        "parts": [{"intent": intent, "confidence": confidence, "text": text}],
        "explanation": "简短依据"
    })
    .to_string()
}

#[tokio::test]
async fn binds_locally_and_maps_unique_original_utf8_text() {
    let text = "补上预算 📚；改成两页";
    let body = json!({
        "parts": [
            {"intent": "supplement", "confidence": 95, "text": "补上预算 📚"},
            {"intent": "correction", "confidence": 92, "text": "改成两页"}
        ],
        "explanation": "补充预算并纠正页数"
    });
    let provider = Provider::new(vec![Step::new(final_response(&body.to_string()))]);
    let judge = LlmRelationJudge::new(provider.clone());
    let input = input(text);
    let decision = judge.judge(input.clone()).await.unwrap();
    decision.validate(&input).unwrap();
    for (part, expected) in decision.parts.iter().zip(["补上预算 📚", "改成两页"]) {
        let span = part.span.unwrap();
        assert_eq!(&text[span.start..span.end], expected);
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    requests[0].validate().unwrap();
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[0].messages[0].role, ChatRole::System);
    let state = requests[0].messages[1].text.as_ref().unwrap();
    for id in [
        "private-message-id",
        "private-session-id",
        "private-task-id",
        "用户一",
    ] {
        assert!(!state.contains(id));
    }
    let state: serde_json::Value = serde_json::from_str(state).unwrap();
    assert_eq!(state["message"], text);
    assert_eq!(state["task_text"], "生成三页报告");
    assert_eq!(state["started_tools"], 0);
}

#[tokio::test]
async fn sends_clarification_content_and_match_boolean_without_question_id() {
    let provider = Provider::new(vec![Step::new(final_response(&wire(
        "answer",
        90,
        Some("两页"),
    )))]);
    let judge = LlmRelationJudge::new(provider.clone());
    let mut input = input("两页");
    input.message.reply_to = Some("private-question-id".into());
    input.clarification = Some(ClarificationContext {
        question_id: "private-question-id".into(),
        source_text: "调整报告".into(),
        prompt: "需要几页？".into(),
    });
    judge.judge(input).await.unwrap();
    let requests = provider.requests.lock().unwrap();
    let text = requests[0].messages[1].text.as_ref().unwrap();
    assert!(!text.contains("private-question-id"));
    let state: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(state["clarification"]["reply_matches"], true);
    assert_eq!(state["clarification"]["prompt"], "需要几页？");
}

#[tokio::test]
async fn rejects_invented_repeated_overlapping_and_missing_source_fragments() {
    for (source, body) in [
        ("中文要求", wire("correction", 90, Some("英文要求"))),
        ("两页，两页", wire("correction", 90, Some("两页"))),
        ("aaa", wire("correction", 90, Some("aa"))),
        ("报告", wire("correction", 90, None)),
        ("报告", wire("cancel", 90, Some("报告"))),
        ("报告", wire("correction", 101, Some("报告"))),
        ("报告", wire("correction", 90, Some(""))),
        (
            "补充中文",
            json!({
                "parts": [
                    {"intent":"supplement","confidence":90,"text":"补充中文"},
                    {"intent":"correction","confidence":90,"text":"中文"}
                ],
                "explanation":"依据"
            })
            .to_string(),
        ),
    ] {
        let provider = Provider::new(vec![Step::new(final_response(&body))]);
        assert_eq!(
            LlmRelationJudge::new(provider).judge(input(source)).await,
            Err(RelationError::Protocol)
        );
    }
}

#[tokio::test]
async fn rejects_non_closed_json_duplicate_fields_and_tool_requests() {
    let valid = wire("cancel", 95, None);
    for body in [
        "not json".to_string(),
        format!("```json\n{valid}\n```"),
        format!("{valid} trailing"),
        valid.replace("\"explanation\":", "\"extra\":true,\"explanation\":"),
        valid.replace(
            "\"explanation\":",
            "\"explanation\":\"first\",\"explanation\":",
        ),
        valid.replace("\"confidence\":95", "\"confidence\":95,\"confidence\":99"),
        valid.replace("\"cancel\"", "\"unknown\""),
        json!({"parts": [], "explanation":"依据"}).to_string(),
        json!({"parts": vec![json!({"intent":"cancel","confidence":90});17], "explanation":"依据"})
            .to_string(),
        "x".repeat(16385),
    ] {
        let provider = Provider::new(vec![Step::new(final_response(&body))]);
        assert_eq!(
            LlmRelationJudge::new(provider)
                .judge(input("取消任务"))
                .await,
            Err(RelationError::Protocol)
        );
    }
    let provider = Provider::new(vec![Step::new(Ok(ModelResponse::ToolCalls {
        calls: vec![],
    }))]);
    assert_eq!(
        LlmRelationJudge::new(provider)
            .judge(input("取消任务"))
            .await,
        Err(RelationError::Protocol)
    );
}

#[tokio::test]
async fn bounds_input_and_returns_errors_without_provider_diagnostics() {
    let provider = Provider::new(vec![]);
    let judge = LlmRelationJudge::new(provider.clone());
    assert_eq!(
        judge.judge(input(&"x".repeat(65537))).await,
        Err(RelationError::Unavailable)
    );
    let mut invalid = input("取消任务");
    invalid.message.message_id.clear();
    assert_eq!(judge.judge(invalid).await, Err(RelationError::Protocol));
    assert!(provider.requests.lock().unwrap().is_empty());
    for (error, expected) in [
        (
            LlmError::Provider("private credential".into()),
            RelationError::Unavailable,
        ),
        (LlmError::ProviderTimeout, RelationError::Timeout),
        (
            LlmError::Protocol("private body".into()),
            RelationError::Protocol,
        ),
        (LlmError::Cancelled, RelationError::Unavailable),
    ] {
        let provider = Provider::new(vec![Step::new(Err(error))]);
        assert_eq!(
            LlmRelationJudge::new(provider)
                .judge(input("取消任务"))
                .await,
            Err(expected)
        );
    }
}

#[derive(Clone)]
enum Resolution {
    Selected(ModelSelection),
    Error(LlmError),
    Panic,
}

struct Resolver {
    current: Mutex<Resolution>,
    calls: AtomicUsize,
}

impl Resolver {
    fn new(provider: Arc<dyn LlmProvider>, provider_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(Resolution::Selected(ModelSelection {
                provider,
                provider_timeout,
            })),
            calls: AtomicUsize::new(0),
        })
    }

    fn select(&self, provider: Arc<dyn LlmProvider>, provider_timeout: Duration) {
        *self.current.lock().unwrap() = Resolution::Selected(ModelSelection {
            provider,
            provider_timeout,
        });
    }
}

impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let current = self.current.lock().unwrap().clone();
        match current {
            Resolution::Selected(selected) => Ok(selected),
            Resolution::Error(error) => Err(error),
            Resolution::Panic => panic!("模型解析异常夹具"),
        }
    }
}

#[tokio::test]
async fn resolves_each_judgement_once_and_keeps_in_flight_model_selection() {
    let gate = Arc::new(Notify::new());
    let first = Provider::new(vec![Step {
        gate: Some(gate.clone()),
        ..Step::new(final_response(&wire("correction", 90, Some("两页"))))
    }]);
    let second = Provider::new(vec![Step::new(final_response(&wire(
        "correction",
        99,
        Some("两页"),
    )))]);
    let resolver = Resolver::new(first.clone(), Duration::from_secs(3));
    let judge = Arc::new(LlmRelationJudge::with_resolver(resolver.clone()));
    let pending = tokio::spawn({
        let judge = judge.clone();
        async move { judge.judge(input("两页")).await }
    });
    first.wait_requests(1).await;
    resolver.select(second.clone(), Duration::from_secs(1));
    let latest = judge.judge(input("两页")).await.unwrap();
    assert_eq!(latest.parts[0].confidence, 99);
    assert!(!pending.is_finished());
    gate.notify_one();
    let previous = pending.await.unwrap().unwrap();
    assert_eq!(previous.parts[0].confidence, 90);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    for provider in [first, second] {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty());
    }
}

#[tokio::test]
async fn rejects_bad_input_and_failed_model_selection_before_any_provider_request() {
    let provider = Provider::new(vec![]);
    let resolver = Resolver::new(provider.clone(), Duration::from_secs(1));
    let judge = LlmRelationJudge::with_resolver(resolver.clone());
    let mut invalid = input("修改报告");
    invalid.message.message_id.clear();
    assert_eq!(judge.judge(invalid).await, Err(RelationError::Protocol));
    assert_eq!(
        judge.judge(input(&"x".repeat(65537))).await,
        Err(RelationError::Unavailable)
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    for (error, expected) in [
        (
            LlmError::Configuration("private credential and endpoint".into()),
            RelationError::Unavailable,
        ),
        (
            LlmError::Protocol("private response body".into()),
            RelationError::Protocol,
        ),
        (LlmError::ProviderTimeout, RelationError::Timeout),
    ] {
        *resolver.current.lock().unwrap() = Resolution::Error(error);
        assert_eq!(judge.judge(input("修改报告")).await, Err(expected));
    }
    *resolver.current.lock().unwrap() = Resolution::Panic;
    assert_eq!(
        judge.judge(input("修改报告")).await,
        Err(RelationError::Unavailable)
    );
    for timeout in [Duration::ZERO, Duration::MAX] {
        resolver.select(provider.clone(), timeout);
        assert_eq!(
            judge.judge(input("修改报告")).await,
            Err(RelationError::Unavailable)
        );
    }
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 6);
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[derive(Default)]
struct PendingProvider {
    started: AtomicUsize,
    active: AtomicUsize,
    cancelled: AtomicUsize,
    entered: Notify,
}

struct PendingRequest<'a>(&'a PendingProvider);

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}

impl LlmProvider for PendingProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        assert!(request.tools.is_empty());
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            let _pending = PendingRequest(self);
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn selected_provider_timeout_and_outer_deadline_drop_the_only_request() {
    let provider = Arc::new(PendingProvider::default());
    let resolver = Resolver::new(provider.clone(), Duration::from_millis(25));
    let judge = LlmRelationJudge::with_resolver(resolver.clone());
    let result = tokio::time::timeout(Duration::from_secs(2), judge.judge(input("修改报告")))
        .await
        .expect("Provider 期限应先结束");
    assert_eq!(result, Err(RelationError::Timeout));
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    assert_eq!(provider.cancelled.load(Ordering::SeqCst), 1);
    resolver.select(provider.clone(), Duration::from_secs(30));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), judge.judge(input("修改报告")))
            .await
            .is_err()
    );
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    assert_eq!(provider.cancelled.load(Ordering::SeqCst), 2);
    assert_eq!(provider.started.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn explicit_cancellation_drops_request_and_next_judgement_resolves_again() {
    let blocked = Arc::new(PendingProvider::default());
    let resolver = Resolver::new(blocked.clone(), Duration::from_secs(30));
    let judge = Arc::new(LlmRelationJudge::with_resolver(resolver.clone()));
    let pending = tokio::spawn({
        let judge = judge.clone();
        async move { judge.judge(input("修改报告")).await }
    });
    tokio::time::timeout(Duration::from_secs(2), blocked.entered.notified())
        .await
        .unwrap();
    assert_eq!(blocked.active.load(Ordering::SeqCst), 1);
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(blocked.active.load(Ordering::SeqCst), 0);
    assert_eq!(blocked.cancelled.load(Ordering::SeqCst), 1);

    let next = Provider::new(vec![Step::new(final_response(&wire(
        "correction",
        95,
        Some("修改报告"),
    )))]);
    resolver.select(next.clone(), Duration::from_secs(1));
    let input = input("修改报告");
    judge
        .judge(input.clone())
        .await
        .unwrap()
        .validate(&input)
        .unwrap();
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert_eq!(next.requests.lock().unwrap().len(), 1);
    assert_eq!(blocked.started.load(Ordering::SeqCst), 1);
}

struct PanickingObserver;
impl RelationObserver for PanickingObserver {
    fn observe(&self, _: RelationObservation) {
        panic!("synthetic observer panic");
    }
}

#[tokio::test]
async fn observer_panics_leave_actual_provider_result_and_call_count_unchanged() {
    let provider = Provider::new(vec![Step::new(final_response(&wire(
        "correction",
        95,
        Some("修改报告"),
    )))]);
    let judge = LlmRelationJudge::new(provider.clone());
    let result = judge
        .judge_observed(input("修改报告"), Arc::new(PanickingObserver))
        .await
        .unwrap();
    assert_eq!(result.parts[0].intent, MessageIntent::Correction);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn shared_judge_diagnostics_are_request_scoped_during_concurrent_cancellation() {
    let provider = Arc::new(PendingProvider::default());
    let judge = Arc::new(LlmRelationJudge::new(provider.clone()));
    let left = Arc::new(BoundedRelationDiagnostics::default());
    let right = Arc::new(BoundedRelationDiagnostics::default());
    let first = tokio::spawn({
        let judge = judge.clone();
        let observer = left.clone();
        async move { judge.judge_observed(input("修改左边"), observer).await }
    });
    provider.entered.notified().await;
    let second = tokio::spawn({
        let judge = judge.clone();
        let observer = right.clone();
        async move { judge.judge_observed(input("修改右边"), observer).await }
    });
    provider.entered.notified().await;
    first.abort();
    let _ = first.await;
    assert_eq!(left.snapshot().coverage, DiagnosticCoverage::Complete);
    assert_eq!(left.snapshot().counts.unwrap().model_provider_calls, 1);
    assert_eq!(right.snapshot().coverage, DiagnosticCoverage::Invalid);
    assert_eq!(provider.active.load(Ordering::SeqCst), 1);
    second.abort();
    let _ = second.await;
    assert_eq!(right.snapshot().coverage, DiagnosticCoverage::Complete);
    assert_eq!(right.snapshot().counts.unwrap().model_provider_calls, 1);
    assert_eq!(provider.started.load(Ordering::SeqCst), 2);
    for observer in [left, right] {
        assert!(matches!(
            observer.snapshot().events.last(),
            Some(RelationObservation::Finished {
                operation: RelationOperation::Attempt(RelationAttempt::ModelProviderCall),
                outcome: RelationOutcome::Dropped,
                ..
            })
        ));
    }
}
