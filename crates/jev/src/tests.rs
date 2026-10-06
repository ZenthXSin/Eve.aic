use super::*;
use eve_control_api::{ControlPhase, GenerationKey};
use eve_message_api::{ClarificationContext, IncomingMessage, RelationOperation};
use eve_message_diagnostics::{BoundedRelationDiagnostics, DiagnosticCoverage};
use eve_session_api::SessionKey;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

fn input() -> RelationInput {
    RelationInput {
        message: IncomingMessage {
            message_id: "PRIVATE-MESSAGE-ID".into(),
            text: "把解释改成中文，不要省略结论。".into(),
            target: GenerationKey {
                session: SessionKey {
                    session_id: "PRIVATE-SESSION-ID".into(),
                    user_id: "PRIVATE-USER-ID".into(),
                },
                task_id: "PRIVATE-TASK-ID".into(),
                controller_epoch: [17; 16],
                generation: 3,
            },
            reply_to: None,
        },
        task_text: "原任务原文".into(),
        phase: ControlPhase::Generating,
        cancel_requested: false,
        started_tools: Some(0),
        clarification: None,
    }
}
fn output(label: &str) -> Value {
    let probabilities: BTreeMap<_, _> = LABELS
        .iter()
        .map(|name| (*name, if *name == label { 1.0 } else { 0.0 }))
        .collect();
    json!({"model":"jev-latest", "usage":{"input_tokens":1,"output_tokens":1}, "answers":{
        "intent":{"type":"choice","choice":label,"confidence":0.99,"probabilities":probabilities},
        "whole_message":{"type":"noul","noul":0.98}}})
}
async fn server(
    status: u16,
    body: Vec<u8>,
    extra: &str,
) -> (
    String,
    oneshot::Receiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, receiver) = oneshot::channel();
    let extra = extra.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = vec![];
        let header_end = loop {
            let mut block = [0; 4096];
            let n = socket.read(&mut block).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&block[..n]);
            if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
        assert!(headers.starts_with("POST /v1/systemone HTTP/1.1\r\n"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-jev-secret\r\n")
        );
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|v| v.parse().unwrap())
            })
            .unwrap();
        while bytes.len() - header_end < length {
            let mut block = [0; 4096];
            let n = socket.read(&mut block).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&block[..n]);
        }
        sender
            .send(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap())
            .unwrap();
        let head = format!(
            "HTTP/1.1 {status} status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        let _ = socket.write_all(&body).await;
    });
    (url, receiver, task)
}
fn judge(base_url: String) -> JevRelationJudge {
    JevRelationJudge::new(
        JevConfig {
            base_url,
            ..JevConfig::default()
        },
        "test-jev-secret",
    )
    .unwrap()
}

#[tokio::test]
async fn actual_http_sends_only_scoped_text_and_binds_whole_utf8_input_locally() {
    let (url, request, task) =
        server(200, serde_json::to_vec(&output("correction")).unwrap(), "").await;
    let input = input();
    let result = judge(url).judge(input.clone()).await.unwrap();
    result.validate(&input).unwrap();
    assert_eq!(result.parts[0].intent, MessageIntent::Correction);
    assert_eq!(
        result.parts[0].span,
        Some(TextSpan {
            start: 0,
            end: input.message.text.len()
        })
    );
    let sent = request.await.unwrap();
    assert_eq!(sent["state"]["message"], input.message.text);
    assert_eq!(sent["state"]["task_text"], input.task_text);
    assert!(sent["state"].get("target").is_none());
    assert!(sent["state"].get("controller_epoch").is_none());
    assert_eq!(sent["questions"].as_object().unwrap().len(), 2);
    for marker in [
        "PRIVATE-MESSAGE-ID",
        "PRIVATE-SESSION-ID",
        "PRIVATE-USER-ID",
        "PRIVATE-TASK-ID",
        "PRIVATE-EPOCH",
        "test-jev-secret",
    ] {
        assert!(!sent.to_string().contains(marker));
    }
    assert!(sent.get("tools").is_none());
    task.await.unwrap();
}

#[test]
fn validates_every_intent_without_generating_or_trimming_text() {
    for label in LABELS {
        let mut input = input();
        input.clarification = Some(ClarificationContext {
            question_id: "q".into(),
            source_text: "澄清原文".into(),
            prompt: "哪一种？".into(),
        });
        input.message.reply_to = Some("q".into());
        let d = decision(&serde_json::to_vec(&output(label)).unwrap(), &input).unwrap();
        d.validate(&input).unwrap();
        assert_eq!(d.message_id, input.message.message_id);
        assert_eq!(d.target, input.message.target);
        if let Some(span) = d.parts[0].span {
            assert_eq!(
                &input.message.text[span.start..span.end],
                input.message.text
            );
        }
    }
}

#[test]
fn invalid_protocol_probabilities_and_duplicate_keys_are_rejected() {
    let mut cases = vec![];
    for (path, value) in [
        ("/answers/intent/type", json!("noul")),
        ("/answers/intent/choice", json!("not-an-intent")),
        ("/answers/intent/confidence", json!(1.1)),
        ("/answers/intent/confidence", json!(-0.1)),
        ("/answers/whole_message/noul", json!("0.98")),
        ("/answers/whole_message/noul", json!(1.1)),
        ("/answers/intent/probabilities/correction", json!(0.5)),
        ("/answers/intent/probabilities/cancel", json!(2.0)),
    ] {
        let mut v = output("correction");
        *v.pointer_mut(path).unwrap() = value;
        cases.push(v.to_string());
    }
    let mut missing = output("correction");
    missing["answers"]["intent"]["probabilities"]
        .as_object_mut()
        .unwrap()
        .remove("cancel");
    cases.push(missing.to_string());
    let mut extra = output("correction");
    extra["answers"]["intent"]["probabilities"]["tool"] = json!(0.0);
    cases.push(extra.to_string());
    let mut wrong_max = output("correction");
    wrong_max["answers"]["intent"]["choice"] = json!("cancel");
    cases.push(wrong_max.to_string());
    cases.push(
        output("correction")
            .to_string()
            .replace("\"noul\":0.98", "\"noul\":0.98,\"noul\":0.99"),
    );
    cases.push(
        output("correction")
            .to_string()
            .replace("\"cancel\":0.0", "\"cancel\":0.0,\"cancel\":0.0"),
    );
    for bytes in cases {
        assert_eq!(
            decision(bytes.as_bytes(), &input()),
            Err(RelationError::Protocol)
        );
    }
}

#[test]
fn uncertainty_complex_intents_and_unmatched_answers_cannot_bypass_fallback() {
    let mut v = output("correction");
    v["answers"]["whole_message"]["noul"] = json!(0.79);
    assert_eq!(
        decision(&serde_json::to_vec(&v).unwrap(), &input())
            .unwrap()
            .parts[0]
            .intent,
        MessageIntent::Ambiguous
    );
    let mut v = output("cancel");
    v["answers"]["intent"]["confidence"] = json!(0.79);
    assert_eq!(
        decision(&serde_json::to_vec(&v).unwrap(), &input())
            .unwrap()
            .parts[0]
            .confidence,
        79
    );
    assert_eq!(
        decision(&serde_json::to_vec(&output("answer")).unwrap(), &input()),
        Err(RelationError::Protocol)
    );
}

#[tokio::test]
async fn http_failures_redirects_and_oversized_payloads_do_not_become_decisions() {
    for (status, body, extra, expected) in [
        (
            500,
            b"PRIVATE_PROVIDER_ERROR".to_vec(),
            "",
            RelationError::Unavailable,
        ),
        (
            302,
            vec![],
            "Location: http://127.0.0.1:1/leak\r\n",
            RelationError::Unavailable,
        ),
        (
            200,
            vec![b' '; MAX_OUTPUT_BYTES + 1],
            "",
            RelationError::Protocol,
        ),
        (200, b"not json".to_vec(), "", RelationError::Protocol),
    ] {
        let (url, received, task) = server(status, body, extra).await;
        assert_eq!(judge(url).judge(input()).await, Err(expected));
        received.await.unwrap();
        task.await.unwrap();
    }
}

#[test]
fn invalid_configuration_and_oversized_inputs_fail_before_request() {
    for base_url in [
        "http://example.com",
        "https://user:pass@example.com",
        "https://example.com/?secret=value",
        "ftp://example.com",
    ] {
        assert!(
            JevRelationJudge::new(
                JevConfig {
                    base_url: base_url.into(),
                    ..JevConfig::default()
                },
                "test-key"
            )
            .is_err()
        );
    }
    for key in ["", "contains space", "line\nkey", "中文"] {
        assert!(JevRelationJudge::new(JevConfig::default(), key).is_err());
    }
    let mut input = input();
    input.task_text = "界".repeat(MAX_INPUT_BYTES);
    assert_eq!(
        request(&input, "jev-latest"),
        Err(RelationError::Unavailable)
    );
    assert!(!JevConfigurationError.to_string().contains("secret"));
}

#[tokio::test]
async fn timeout_cancellation_and_full_capacity_release_request_slots() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let j = std::sync::Arc::new(
        JevRelationJudge::new(
            JevConfig {
                base_url: format!("http://{}", listener.local_addr().unwrap()),
                timeout: Duration::from_millis(100),
                ..JevConfig::default()
            },
            "test-key",
        )
        .unwrap(),
    );
    let first = tokio::spawn({
        let j = j.clone();
        async move { j.judge(input()).await }
    });
    let (_socket, _) = listener.accept().await.unwrap();
    assert_eq!(j.judge(input()).await, Err(RelationError::Unavailable));
    assert_eq!(first.await.unwrap(), Err(RelationError::Timeout));
    let second = tokio::spawn({
        let j = j.clone();
        async move { j.judge(input()).await }
    });
    let (_socket, _) = listener.accept().await.unwrap();
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    let third = tokio::spawn({
        let j = j.clone();
        async move { j.judge(input()).await }
    });
    let (_socket, _) = listener.accept().await.unwrap();
    third.abort();
    assert!(third.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn diagnostics_distinguish_preflight_rejection_capacity_timeout_and_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let judge = Arc::new(
        JevRelationJudge::new(
            JevConfig {
                base_url: format!("http://{}", listener.local_addr().unwrap()),
                timeout: Duration::from_millis(150),
                ..JevConfig::default()
            },
            "test-key",
        )
        .unwrap(),
    );
    let preflight = Arc::new(BoundedRelationDiagnostics::default());
    let mut oversized = input();
    oversized.task_text = "界".repeat(MAX_INPUT_BYTES);
    assert_eq!(
        judge.judge_observed(oversized, preflight.clone()).await,
        Err(RelationError::Unavailable)
    );
    assert_eq!(preflight.snapshot().coverage, DiagnosticCoverage::Complete);
    assert_eq!(preflight.snapshot().counts.unwrap().classifier_calls, 0);

    let timed = Arc::new(BoundedRelationDiagnostics::default());
    let first = tokio::spawn({
        let judge = judge.clone();
        let observer = timed.clone();
        async move { judge.judge_observed(input(), observer).await }
    });
    let (_first_socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let busy = Arc::new(BoundedRelationDiagnostics::default());
    assert_eq!(
        judge.judge_observed(input(), busy.clone()).await,
        Err(RelationError::Unavailable)
    );
    assert_eq!(busy.snapshot().coverage, DiagnosticCoverage::Complete);
    assert_eq!(busy.snapshot().counts.unwrap().classifier_calls, 0);
    assert_eq!(first.await.unwrap(), Err(RelationError::Timeout));
    assert_eq!(timed.snapshot().counts.unwrap().classifier_calls, 1);
    assert!(matches!(
        timed.snapshot().events.last(),
        Some(RelationObservation::Finished {
            operation: RelationOperation::Attempt(RelationAttempt::ClassifierCall),
            outcome: RelationOutcome::Timeout,
            ..
        })
    ));

    let dropped = Arc::new(BoundedRelationDiagnostics::default());
    let second = tokio::spawn({
        let judge = judge.clone();
        let observer = dropped.clone();
        async move { judge.judge_observed(input(), observer).await }
    });
    let (_second_socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    assert_eq!(dropped.snapshot().coverage, DiagnosticCoverage::Complete);
    assert_eq!(dropped.snapshot().counts.unwrap().classifier_calls, 1);
    assert!(matches!(
        dropped.snapshot().events.last(),
        Some(RelationObservation::Finished {
            operation: RelationOperation::Attempt(RelationAttempt::ClassifierCall),
            outcome: RelationOutcome::Dropped,
            ..
        })
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "本地两次实际准入以外不应重试或发送额外请求"
    );
}
