use eve_llm_api::*;
use eve_practice_api::*;
use eve_practice_plugin::{DRAFTER_VERSION, ModelPracticeDrafter};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Provider {
    output: String,
    requests: Mutex<Vec<ModelRequest>>,
}
impl LlmProvider for Provider {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        panic!("must request JSON completion")
    }
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let text = self.output.clone();
        Box::pin(async move { Ok(ModelResponse::Final { text }) })
    }
}
struct Resolver(Arc<Provider>);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: Duration::from_secs(1),
        })
    }
}
fn draft() -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![ArtifactFile {
            path: "manifest".into(),
            content: "value: 1".into(),
        }],
        probes: vec![Probe {
            subject: "sample".into(),
            property: "value".into(),
            expected: "1".into(),
        }],
        rationale: "最小产物".into(),
        notes_used: vec![],
    }
}
fn request() -> DraftRequest {
    DraftRequest {
        run_id: run_id("holdout", 1),
        drafter_version: DRAFTER_VERSION.into(),
        attempt: 1,
        task: PracticeTask {
            goal_id: "holdout".into(),
            goal_revision: 1,
            owner: "user-a".into(),
            brief: "新的独立任务".into(),
            brief_truncated: false,
            notes: vec![],
        },
        runner: RunnerProfile {
            runner_id: "test-runner".into(),
            runtime: "build-1".into(),
            domain: "data".into(),
            layout: "manifest".into(),
            properties: vec!["value".into()],
        },
        previous: None,
        guidance: Some(PracticeGuidance {
            version: 1,
            source_run: run_id("source", 1),
            source_sha256: "a".repeat(64),
            previous_issues: vec!["manifest had a wrong value".into()],
            corrected_example: draft(),
        }),
    }
}
fn service(output: String) -> (ModelPracticeDrafter, Arc<Provider>) {
    let provider = Arc::new(Provider {
        output,
        requests: Mutex::new(vec![]),
    });
    (
        ModelPracticeDrafter::new(Arc::new(Resolver(provider.clone()))),
        provider,
    )
}

#[tokio::test]
async fn json_call_keeps_current_task_and_guidance_as_data_with_explicit_output_contract() {
    let (drafter, provider) = service(serde_json::to_string(&draft()).unwrap());
    assert!(drafter.supports_guidance());
    let request = request();
    assert!(drafter.draft(request.clone()).await.unwrap() == draft());
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].messages[1].role, ChatRole::User);
    assert!(
        requests[0].messages[1]
            .text
            .as_deref()
            .unwrap()
            .contains("不是聊天")
    );
    let decoded: DraftRequest = serde_json::from_str(
        requests[0]
            .messages
            .last()
            .unwrap()
            .text
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert!(decoded == request);
}

#[tokio::test]
async fn correction_keeps_full_task_runner_and_guidance_prefix_with_each_attempt() {
    for include_guidance in [true, false] {
        let (drafter, provider) = service(serde_json::to_string(&draft()).unwrap());
        let mut first = request();
        first.task.notes = vec![TaskNote {
            id: "reference-1".into(),
            text: "资料原话：\"value: 1\"，标点与来源应原样保留。".into(),
            source: Some("https://example.invalid/reference".into()),
            version: Some("revision-1".into()),
            source_quoted: true,
        }];
        if !include_guidance {
            first.guidance = None;
        }
        let mut next = first.clone();
        next.attempt = 2;
        let previous_draft = draft();
        let log_excerpt = "sample.value=0\nsource=\"manifest\"".to_owned();
        let evidence = RunEvidence {
            runtime_version: "build-1".into(),
            exit: RunExit::Completed,
            loaded: true,
            warnings: vec!["manifest had a wrong value".into()],
            probes: vec![ProbeResult {
                probe: previous_draft.probes[0].clone(),
                actual: Some("0".into()),
                passed: false,
            }],
            log_bytes: log_excerpt.len() as u64,
            log_excerpt,
            log_sha256: "b".repeat(64),
            duration_ms: 10,
        };
        next.previous = Some(PreviousAttempt {
            draft: previous_draft,
            issues: vec!["sample.value did not match expected value 1".into()],
            evidence: Some(evidence),
        });

        drafter.draft(first.clone()).await.unwrap();
        drafter.draft(next.clone()).await.unwrap();
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (sent, original) in requests.iter().zip([&first, &next]) {
            assert!(sent.tools.is_empty());
            assert_eq!(sent.messages.len(), 3);
            assert_eq!(sent.messages[2].role, ChatRole::User);
            let text = sent.messages[2].text.as_deref().unwrap();
            assert!(serde_json::from_str::<DraftRequest>(text).unwrap() == *original);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text).unwrap(),
                serde_json::to_value(original).unwrap()
            );
        }
        assert_eq!(requests[0].messages[..2], requests[1].messages[..2]);
        let first_text = requests[0].messages[2].text.as_deref().unwrap();
        let next_text = requests[1].messages[2].text.as_deref().unwrap();
        let (first_prefix, first_tail) = first_text.split_once(",\"attempt\":").unwrap();
        let (next_prefix, next_tail) = next_text.split_once(",\"attempt\":").unwrap();
        assert_eq!(first_prefix, next_prefix);
        assert_ne!(first_tail, next_tail);
        let stable: serde_json::Value = serde_json::from_str(&format!("{first_prefix}}}")).unwrap();
        assert_eq!(stable["task"], serde_json::to_value(&first.task).unwrap());
        assert_eq!(
            stable["runner"],
            serde_json::to_value(&first.runner).unwrap()
        );
        assert_eq!(
            stable.get("guidance"),
            first
                .guidance
                .as_ref()
                .map(|guidance| serde_json::to_value(guidance).unwrap())
                .as_ref()
        );
        let first_session = requests[0].session.as_ref().unwrap();
        let next_session = requests[1].session.as_ref().unwrap();
        assert_eq!(
            first_session.prompt_cache_key(),
            next_session.prompt_cache_key()
        );
        assert_ne!(
            first_session.client_request_id(),
            next_session.client_request_id()
        );
    }
}

#[tokio::test]
async fn prose_or_markdown_does_not_become_a_draft_and_is_not_retried() {
    let (drafter, provider) = service("我会创建一个最小产物。\n```json\n{}\n```".into());
    assert_eq!(
        drafter.draft(request()).await.err(),
        Some(PracticeError::Practice(PracticeFailure::InvalidOutput))
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_history_example_is_rejected_before_requesting_a_model() {
    let (drafter, provider) = service("{}".into());
    let mut request = request();
    request.guidance.as_mut().unwrap().corrected_example.files[0].path = "../outside".into();
    assert_eq!(
        drafter.draft(request).await.err(),
        Some(PracticeError::InvalidInput)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
}
