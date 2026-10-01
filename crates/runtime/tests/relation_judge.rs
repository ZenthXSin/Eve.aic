#[path = "support/session.rs"]
mod fixture;
use eve_control_api::{ControlPhase, GenerationKey};
use eve_llm_api::{ChatRole, LlmError, ModelResponse};
use eve_message_api::*;
use eve_runtime::LlmRelationJudge;
use fixture::{Provider, Step, final_response, key};
use serde_json::json;

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
