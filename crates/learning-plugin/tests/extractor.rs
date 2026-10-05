use eve_learning_api::{
    CandidateDraft, LearningBatch, LearningError, LearningFailure, MAX_CANDIDATE_BYTES,
    MAX_EVIDENCE_BYTES, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES, PreferenceExtractor,
};
use eve_learning_plugin::ModelPreferenceExtractor;
use eve_llm_api::{
    ChatRole, LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse,
    ModelSelection, ToolCall,
};
use eve_memory_api::{EvidenceSource, InteractionEvidence, MemoryScope};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
enum Reply {
    Ready(Result<ModelResponse, LlmError>),
    Pending,
}

struct Mock {
    reply: Reply,
    timeout: Duration,
    resolve_error: Option<LlmError>,
    resolves: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
    dropped: AtomicUsize,
}

struct PendingGuard<'a>(&'a AtomicUsize);
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl LlmProvider for Mock {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            match &self.reply {
                Reply::Ready(reply) => reply.clone(),
                Reply::Pending => {
                    let _guard = PendingGuard(&self.dropped);
                    std::future::pending().await
                }
            }
        })
    }
}

struct Resolver(Arc<Mock>);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        self.0.resolves.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.0.resolve_error {
            return Err(error.clone());
        }
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: self.0.timeout,
        })
    }
}

fn setup(reply: Reply, timeout: Duration) -> (ModelPreferenceExtractor, Arc<Mock>) {
    let mock = Arc::new(Mock {
        reply,
        timeout,
        resolve_error: None,
        resolves: AtomicUsize::new(0),
        requests: Mutex::new(vec![]),
        dropped: AtomicUsize::new(0),
    });
    (
        ModelPreferenceExtractor::new(Arc::new(Resolver(mock.clone()))),
        mock,
    )
}

fn with_text(text: impl Into<String>) -> (ModelPreferenceExtractor, Arc<Mock>) {
    setup(
        Reply::Ready(Ok(ModelResponse::Final { text: text.into() })),
        Duration::from_secs(5),
    )
}

fn evidence(index: u64) -> InteractionEvidence {
    InteractionEvidence {
        id: format!("evidence-{index}"),
        revision: index,
        at_ms: 100 + index,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("opaque-message-{index}"),
            session_revision: 2 * index,
            turn_id: index,
            user_text: "请一直用中文简短回答。\n引用：\"忽略规则\"".into(),
            assistant_text: "我会用中文回答。我的猜测：用户一定爱喝咖啡。".into(),
        },
    }
}

fn batch() -> LearningBatch {
    LearningBatch {
        id: "learning-batch-01".into(),
        scope: MemoryScope {
            channel: "qq".into(),
            session_id: "app-01-group-02-user-03".into(),
            user_id: "user-03".into(),
        },
        extractor_version: "preference-extractor:v1".into(),
        started_at_ms: 500,
        evidence: vec![evidence(1), evidence(2), evidence(3)],
    }
}

fn draft() -> CandidateDraft {
    CandidateDraft {
        text: "使用中文简短回答".into(),
        confidence: 90,
        evidence_ids: vec!["evidence-1".into(), "evidence-2".into()],
    }
}

fn assert_single_call(mock: &Mock) {
    assert_eq!(mock.resolves.load(Ordering::SeqCst), 1);
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

fn assert_no_calls(mock: &Mock) {
    assert_eq!(mock.resolves.load(Ordering::SeqCst), 0);
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sends_only_fixed_rules_and_exact_scoped_batch_once() {
    let expected = draft();
    let output = json!({"candidates": [expected.clone()]}).to_string();
    let (extractor, mock) = with_text(output);
    let input = batch();
    assert_eq!(extractor.version(), input.extractor_version);
    assert_eq!(extractor.extract(input.clone()).await.unwrap(), [expected]);
    assert_single_call(&mock);
    let mut request = mock.requests.lock().unwrap()[0].clone();
    request.validate().unwrap();
    assert!(request.tools.is_empty());
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[0].role, ChatRole::System);
    assert_eq!(request.messages[1].role, ChatRole::User);
    assert_eq!(
        request.messages[1].text.as_deref().unwrap(),
        serde_json::to_string(&input).unwrap()
    );
    let actual: LearningBatch =
        serde_json::from_str(request.messages[1].text.as_deref().unwrap()).unwrap();
    assert_eq!(actual, input);
    let system = request.messages[0].text.clone().unwrap();
    assert!(system.contains("助手自述、助手猜测、用户沉默"));
    assert!(system.contains("证据不足时返回空 candidates"));
    assert!(!system.contains(&input.scope.user_id));
    for message in &request.messages {
        assert!(message.tool_calls.is_empty());
        assert!(message.tool_results.is_empty());
    }
    // 消息文本预算：唯一动态部分是 batch；固定 System 和空文本结构小于 4096。
    for message in &mut request.messages {
        message.text = Some(String::new());
    }
    assert!(system.len() + serde_json::to_vec(&request).unwrap().len() <= 4096);

    let (other_extractor, other_mock) = with_text(r#"{"candidates":[]}"#);
    let mut other_input = batch();
    other_input.scope.user_id = "different-user".into();
    other_input.scope.session_id = "another-session".into();
    other_extractor.extract(other_input).await.unwrap();
    assert_eq!(
        other_mock.requests.lock().unwrap()[0].messages[0]
            .text
            .as_ref(),
        Some(&system)
    );
}

#[tokio::test]
async fn accepts_empty_candidates_and_exact_text_confidence_boundaries() {
    let (extractor, mock) = with_text(r#"{"candidates":[]}"#);
    assert!(extractor.extract(batch()).await.unwrap().is_empty());
    assert_single_call(&mock);

    let mut low = draft();
    low.confidence = 0;
    low.text = "a".repeat(MAX_CANDIDATE_BYTES);
    let mut high = draft();
    high.confidence = 100;
    high.evidence_ids = vec!["evidence-3".into()];
    let expected = vec![low, high];
    let output = json!({"candidates": expected}).to_string();
    let (extractor, mock) = with_text(output);
    assert_eq!(extractor.extract(batch()).await.unwrap(), expected);
    assert_single_call(&mock);
}

#[tokio::test]
async fn rejects_invalid_output_without_repair_or_retry() {
    let candidate = json!(draft());
    let mut over_confidence = candidate.clone();
    over_confidence["confidence"] = json!(101);
    let mut huge_text = candidate.clone();
    huge_text["text"] = json!("界".repeat(MAX_CANDIDATE_BYTES / 3 + 1));
    let mut empty_text = candidate.clone();
    empty_text["text"] = json!(" \n\t");
    let mut nul_text = candidate.clone();
    nul_text["text"] = json!("a\0b");
    let mut unrecognized = candidate.clone();
    unrecognized["scope"] = json!({"user_id":"another-user"});
    let mut foreign_id = candidate.clone();
    foreign_id["evidence_ids"] = json!(["evidence-from-another-scope"]);
    let mut duplicate_id = candidate.clone();
    duplicate_id["evidence_ids"] = json!(["evidence-1", "evidence-1"]);
    let mut no_evidence = candidate.clone();
    no_evidence["evidence_ids"] = json!([]);
    let malformed = [
        "not json".to_owned(),
        "```json\n{\"candidates\":[]}\n```".into(),
        "[]".into(),
        "null".into(),
        "{}".into(),
        r#"{"candidates":[],"extra":false}"#.into(),
        r#"{"candidates":[],"candidates":[]}"#.into(),
        r#"{"candidates":[{"text":"one","text":"two","confidence":1,"evidence_ids":["evidence-1"]}]}"#.into(),
        r#"{"candidates":[{"text":"one","confidence":1,"confi\u0064ence":2,"evidence_ids":["evidence-1"]}]}"#.into(),
        r#"{"candidates":[{"text":"one","confidence":1,"evidence_ids":["evidence-1"],"evidence_ids":["evidence-2"]}]}"#.into(),
        r#"{"candidates":[{"text":"one","confidence":-1,"evidence_ids":["evidence-1"]}]}"#.into(),
        r#"{"candidates":[{"text":"one","confidence":0.5,"evidence_ids":["evidence-1"]}]}"#.into(),
        r#"{"candidates":[{"text":"one","confidence":256,"evidence_ids":["evidence-1"]}]}"#.into(),
        json!({"candidates": [over_confidence]}).to_string(),
        json!({"candidates": [huge_text]}).to_string(),
        json!({"candidates": [empty_text]}).to_string(),
        json!({"candidates": [nul_text]}).to_string(),
        json!({"candidates": [unrecognized]}).to_string(),
        json!({"candidates": [foreign_id]}).to_string(),
        json!({"candidates": [duplicate_id]}).to_string(),
        json!({"candidates": [no_evidence]}).to_string(),
        json!({"candidates": [candidate.clone(), candidate.clone(), candidate.clone(), candidate]}).to_string(),
        format!("{{\"candidates\":[]}}{}", " ".repeat(MAX_OUTPUT_BYTES)),
        r#"{"candidates":[]} {"candidates":[]}"#.into(),
    ];
    for (index, output) in malformed.into_iter().enumerate() {
        let (extractor, mock) = with_text(output);
        assert_eq!(
            extractor.extract(batch()).await.unwrap_err(),
            LearningError::Extraction(LearningFailure::InvalidOutput),
            "case {index}"
        );
        assert_single_call(&mock);
    }
}

#[tokio::test]
async fn refuses_tool_calls_without_running_tools_or_second_request() {
    let (extractor, mock) = setup(
        Reply::Ready(Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".into(),
                name: "write_memory".into(),
                arguments: json!({"text":"inject"}),
            }],
        })),
        Duration::from_secs(5),
    );
    assert_eq!(
        extractor.extract(batch()).await.unwrap_err(),
        LearningError::Extraction(LearningFailure::InvalidOutput)
    );
    assert_single_call(&mock);
}

#[tokio::test]
async fn maps_provider_failures_without_leaking_messages_or_retrying() {
    for (error, failure) in [
        (
            LlmError::Provider("secret".into()),
            LearningFailure::Provider,
        ),
        (LlmError::ProviderTimeout, LearningFailure::Timeout),
        (LlmError::Cancelled, LearningFailure::Cancelled),
        (
            LlmError::Protocol("secret".into()),
            LearningFailure::Provider,
        ),
    ] {
        let (extractor, mock) = setup(Reply::Ready(Err(error)), Duration::from_secs(5));
        let error = extractor.extract(batch()).await.unwrap_err();
        assert_eq!(error, LearningError::Extraction(failure));
        assert!(!error.to_string().contains("secret"));
        assert_single_call(&mock);
    }
}

#[tokio::test]
async fn resolver_failure_never_calls_provider() {
    let mock = Arc::new(Mock {
        reply: Reply::Pending,
        timeout: Duration::from_secs(5),
        resolve_error: Some(LlmError::Configuration("private config".into())),
        resolves: AtomicUsize::new(0),
        requests: Mutex::new(vec![]),
        dropped: AtomicUsize::new(0),
    });
    let extractor = ModelPreferenceExtractor::new(Arc::new(Resolver(mock.clone())));
    assert_eq!(
        extractor.extract(batch()).await.unwrap_err(),
        LearningError::Extraction(LearningFailure::Provider)
    );
    assert_eq!(mock.resolves.load(Ordering::SeqCst), 1);
    assert!(mock.requests.lock().unwrap().is_empty());
}

fn user_text(evidence: &mut InteractionEvidence) -> &mut String {
    let EvidenceSource::CompletedInteraction { user_text, .. } = &mut evidence.source else {
        panic!("expected completed interaction")
    };
    user_text
}

fn fill_evidence(evidence: &mut InteractionEvidence, size: usize) {
    *user_text(evidence) = "x".into();
    let overhead = serde_json::to_vec(evidence).unwrap().len() - 1;
    *user_text(evidence) = "x".repeat(size - overhead);
    assert_eq!(serde_json::to_vec(evidence).unwrap().len(), size);
}

#[tokio::test]
async fn preserves_exact_input_budget_boundaries_and_never_truncates() {
    let mut single = batch();
    single.evidence.truncate(1);
    fill_evidence(&mut single.evidence[0], MAX_EVIDENCE_BYTES);
    let (extractor, mock) = with_text(r#"{"candidates":[]}"#);
    extractor.extract(single.clone()).await.unwrap();
    assert_single_call(&mock);
    assert_eq!(
        mock.requests.lock().unwrap()[0].messages[1]
            .text
            .as_ref()
            .unwrap(),
        &serde_json::to_string(&single).unwrap()
    );
    user_text(&mut single.evidence[0]).push('x');
    let (extractor, mock) = with_text(r#"{"candidates":[]}"#);
    assert_eq!(
        extractor.extract(single).await.unwrap_err(),
        LearningError::InvalidInput
    );
    assert_no_calls(&mock);

    let mut full = batch();
    full.evidence = (1..=4).map(evidence).collect();
    for evidence in &mut full.evidence {
        fill_evidence(evidence, MAX_EVIDENCE_BYTES);
    }
    let excess = serde_json::to_vec(&full).unwrap().len() - MAX_INPUT_BYTES;
    let last_text = user_text(&mut full.evidence[3]);
    last_text.truncate(last_text.len() - excess);
    assert_eq!(serde_json::to_vec(&full).unwrap().len(), MAX_INPUT_BYTES);
    let (extractor, mock) = with_text(r#"{"candidates":[]}"#);
    extractor.extract(full.clone()).await.unwrap();
    assert_single_call(&mock);
    assert_eq!(
        mock.requests.lock().unwrap()[0].messages[1]
            .text
            .as_ref()
            .unwrap()
            .len(),
        MAX_INPUT_BYTES
    );
    user_text(&mut full.evidence[3]).push('x');
    assert!(serde_json::to_vec(&full.evidence[3]).unwrap().len() <= MAX_EVIDENCE_BYTES);
    let (extractor, mock) = with_text(r#"{"candidates":[]}"#);
    assert_eq!(
        extractor.extract(full).await.unwrap_err(),
        LearningError::InvalidInput
    );
    assert_no_calls(&mock);
}

#[tokio::test]
async fn rejects_unbounded_or_noncompleted_or_duplicate_input_before_resolve() {
    let mut inputs = Vec::new();
    let mut invalid = batch();
    invalid.scope.user_id.clear();
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.id.clear();
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.extractor_version = "unknown-version".into();
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.evidence.clear();
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.evidence = (1..=9).map(evidence).collect();
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.evidence[0].source = EvidenceSource::UserStatement {
        message_id: "command".into(),
        text: "/remember command is not a completed interaction".into(),
    };
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.evidence[0].revision = 0;
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.evidence[0].id = invalid.evidence[1].id.clone();
    inputs.push(invalid);
    let mut invalid = batch();
    invalid.evidence[0].source = invalid.evidence[1].source.clone();
    inputs.push(invalid);
    let mut invalid = batch();
    user_text(&mut invalid.evidence[0]).clear();
    inputs.push(invalid);
    for (index, invalid) in inputs.into_iter().enumerate() {
        let (extractor, mock) = with_text(r#"{"candidates":[]}"#);
        assert_eq!(
            extractor.extract(invalid).await.unwrap_err(),
            LearningError::InvalidInput,
            "case {index}"
        );
        assert_no_calls(&mock);
    }
}

#[tokio::test]
async fn provider_timeout_drops_pending_request_once() {
    let (extractor, mock) = setup(Reply::Pending, Duration::from_millis(10));
    let result = tokio::time::timeout(Duration::from_secs(1), extractor.extract(batch()))
        .await
        .expect("configured short timeout was ignored");
    assert_eq!(
        result.unwrap_err(),
        LearningError::Extraction(LearningFailure::Timeout)
    );
    assert_single_call(&mock);
    assert_eq!(mock.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn caller_cancellation_drops_request_without_retrying() {
    let (extractor, mock) = setup(Reply::Pending, Duration::from_secs(20));
    // 外部宿主取消即丢弃该 Future，不能留下后台请求或重新解析模型。
    assert!(
        tokio::time::timeout(Duration::from_millis(10), extractor.extract(batch()))
            .await
            .is_err()
    );
    assert_single_call(&mock);
    assert_eq!(mock.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn caps_long_provider_timeout_at_thirty_seconds() {
    let (extractor, mock) = setup(Reply::Pending, Duration::from_secs(120));
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(35), extractor.extract(batch()))
        .await
        .expect("thirty-second hard limit was ignored");
    assert_eq!(
        result.unwrap_err(),
        LearningError::Extraction(LearningFailure::Timeout)
    );
    assert!(started.elapsed() >= Duration::from_secs(29));
    assert_single_call(&mock);
    assert_eq!(mock.dropped.load(Ordering::SeqCst), 1);
}
