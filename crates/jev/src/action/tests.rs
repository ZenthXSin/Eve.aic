use super::*;
use crate::tests::{judge, server, server_with_session};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot};

fn action() -> ActionReviewRequest {
    ActionReviewRequest {
        model_session: Some(
            eve_llm_api::ModelSession::anchored("review", None, "persistent-action-test")
                .unwrap()
                .with_turn("turn-3")
                .unwrap(),
        ),
        action: "具体系统变更".into(),
        program: "eve.apply_self_upgrade".into(),
        arguments: vec!["完整源码差异、候选清单和租约".into()],
        workspace: "/workspace".into(),
        isolated: false,
        cwd: ".".into(),
        policy_reason: "只核具体系统影响".into(),
        claimed_confirmation: false,
        delegated: true,
        user_messages: vec![],
        recent_actions: vec![],
    }
}

fn selected(label: &str, labels: &[&str]) -> Value {
    let probabilities: BTreeMap<_, _> = labels
        .iter()
        .map(|item| (*item, if *item == label { 1.0 } else { 0.0 }))
        .collect();
    json!({"type":"choice", "choice":label, "confidence":0.99, "probabilities":probabilities})
}
fn output(harm: &str, boundary: Option<&str>) -> Value {
    let mut body = json!({"model":"jev-latest", "usage":{"input_tokens":1}, "answers": {
        "direct_system_harm":selected(harm, &HARM_LABELS), "grounded":{"type":"noul", "noul":0.98}
    }});
    if let Some(boundary) = boundary {
        body["answers"]["boundary_intent"] = selected(boundary, &INTENT_LABELS);
    }
    body
}

#[tokio::test]
async fn actual_http_sends_complete_action_in_separate_stable_session_and_returns_both_modes() {
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        for harm in ["none", "bounded", "high", "critical"] {
            let input = action();
            let boundary = (mode == ActionReviewMode::IdentityBoundary).then_some("explicit");
            let session = input
                .model_session
                .as_ref()
                .unwrap()
                .fork("jev-action-review", None)
                .unwrap();
            assert_ne!(
                session.session_id(),
                input.model_session.as_ref().unwrap().session_id()
            );
            let (url, sent, task) = server_with_session(
                200,
                serde_json::to_vec(&output(harm, boundary)).unwrap(),
                "",
                Some(session),
            )
            .await;
            let result = ActionReviewClassifier::classify(&judge(url), mode, input.clone()).await;
            let ActionClassification::Decided {
                verdict,
                confidence,
            } = result
            else {
                panic!("应有确定判断");
            };
            assert_eq!(confidence, 98);
            assert_eq!(verdict.allow, matches!(harm, "none" | "bounded"));
            assert_eq!(verdict.permits_system_safety(), verdict.allow);
            if mode == ActionReviewMode::IdentityBoundary && verdict.allow {
                assert_eq!(verdict.authorization, ReviewAuthorization::High);
                assert!(verdict.permits());
            } else {
                assert_eq!(verdict.authorization, ReviewAuthorization::Unknown);
            }
            let body = sent.await.unwrap();
            assert_eq!(
                body["state"]["request"],
                serde_json::to_value(input).unwrap()
            );
            assert_eq!(body["state"]["mode"], serde_json::to_value(mode).unwrap());
            assert_eq!(
                body["questions"].as_object().unwrap().len(),
                if boundary.is_some() { 3 } else { 2 }
            );
            let encoded = body.to_string();
            assert!(!encoded.contains("test-jev-secret"));
            assert!(!encoded.contains("persistent-action-test"));
            assert!(!encoded.contains("model_session"));
            task.await.unwrap();
        }
    }
}

#[tokio::test]
async fn identity_intent_is_independent_from_claimed_confirmation_and_denial_needs_no_llm() {
    for boundary in ["absent", "revoked"] {
        let (url, sent, task) = server(
            200,
            serde_json::to_vec(&output("none", Some(boundary))).unwrap(),
            "",
        )
        .await;
        let mut input = action();
        input.claimed_confirmation = true;
        input.user_messages = vec!["这里是被引用的别人的请求，不代表本人要求".into()];
        let decision = ActionReviewClassifier::classify(
            &judge(url),
            ActionReviewMode::IdentityBoundary,
            input.clone(),
        )
        .await;
        let ActionClassification::Decided { verdict, .. } = decision else {
            panic!("应有明确拒绝");
        };
        assert!(!verdict.allow);
        assert_eq!(verdict.authorization, ReviewAuthorization::Unknown);
        assert_eq!(
            sent.await.unwrap()["state"]["request"],
            serde_json::to_value(input).unwrap()
        );
        task.await.unwrap();
    }
}

#[test]
fn strict_scores_reject_malformed_evidence_and_keep_uncertainty_separate_from_deny() {
    let mut null_boundary = output("none", None);
    null_boundary["answers"]["boundary_intent"] = Value::Null;
    assert_eq!(
        parse(
            ActionReviewMode::SystemSafety,
            &serde_json::to_vec(&null_boundary).unwrap()
        ),
        Err(ActionReviewUncertainty::InvalidResponse),
    );
    for boundary in [None, Some("explicit")] {
        let mode = if boundary.is_some() {
            ActionReviewMode::IdentityBoundary
        } else {
            ActionReviewMode::SystemSafety
        };
        let bytes = |body: &Value| serde_json::to_vec(body).unwrap();
        assert_eq!(
            parse(mode, &bytes(&output("uncertain", boundary))),
            Err(ActionReviewUncertainty::Ambiguous)
        );
        let mut low = output("none", boundary);
        low["answers"]["grounded"]["noul"] = json!(0.79);
        assert!(matches!(
            parse(mode, &bytes(&low)).unwrap(),
            ActionClassification::Decided { confidence: 79, .. }
        ));
        for mutation in 0..7 {
            let mut body = output("none", boundary);
            match mutation {
                0 => body["answers"]["direct_system_harm"]["type"] = json!("text"),
                1 => body["answers"]["direct_system_harm"]["confidence"] = json!(1.1),
                2 => body["answers"]["direct_system_harm"]["probabilities"]["unknown"] = json!(0.0),
                3 => body["answers"]["direct_system_harm"]["probabilities"]["none"] = json!(0.4),
                4 => {
                    body["answers"]["direct_system_harm"]["probabilities"]["none"] = json!(0.4);
                    body["answers"]["direct_system_harm"]["probabilities"]["high"] = json!(0.6);
                }
                5 => body["answers"]["grounded"]["noul"] = json!(-0.1),
                6 => body["answers"]["extra"] = json!(true),
                _ => unreachable!(),
            }
            assert_eq!(
                parse(mode, &bytes(&body)),
                Err(ActionReviewUncertainty::InvalidResponse)
            );
        }
        let duplicate = output("none", boundary)
            .to_string()
            .replace("\"none\":1.0", "\"none\":1.0,\"none\":1.0");
        assert_eq!(
            parse(mode, duplicate.as_bytes()),
            Err(ActionReviewUncertainty::InvalidResponse)
        );
    }
    assert_eq!(
        parse(
            ActionReviewMode::IdentityBoundary,
            &serde_json::to_vec(&output("none", Some("uncertain"))).unwrap()
        ),
        Err(ActionReviewUncertainty::Ambiguous)
    );
    assert_eq!(
        parse(
            ActionReviewMode::IdentityBoundary,
            &serde_json::to_vec(&output("none", None)).unwrap()
        ),
        Err(ActionReviewUncertainty::InvalidResponse)
    );
    assert_eq!(
        parse(
            ActionReviewMode::SystemSafety,
            &serde_json::to_vec(&output("none", Some("explicit"))).unwrap()
        ),
        Err(ActionReviewUncertainty::InvalidResponse)
    );
}

#[tokio::test]
async fn full_input_over_jev_limit_never_sends_an_http_request_or_truncates() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let judge = judge(format!("http://{}", listener.local_addr().unwrap()));
    let mut input = action();
    input.arguments.push("x".repeat(MAX_INPUT_BYTES));
    input.validate_capacity().unwrap();
    assert_eq!(
        ActionReviewClassifier::classify(&judge, ActionReviewMode::SystemSafety, input).await,
        ActionClassification::Uncertain(ActionReviewUncertainty::InputTooLarge)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(judge.slots.status().unwrap().active, 0);
}

#[tokio::test]
async fn http_errors_redirects_and_invalid_responses_are_uncertain_without_retries() {
    for (status, body, reason) in [
        (
            503,
            b"synthetic-provider-body-must-not-escape".to_vec(),
            ActionReviewUncertainty::Unavailable,
        ),
        (302, vec![], ActionReviewUncertainty::Unavailable),
        (
            200,
            b"{}".to_vec(),
            ActionReviewUncertainty::InvalidResponse,
        ),
        (
            200,
            vec![b'x'; crate::MAX_OUTPUT_BYTES + 1],
            ActionReviewUncertainty::InvalidResponse,
        ),
    ] {
        let (url, sent, task) =
            server(status, body, "Location: http://127.0.0.1:1/unused\r\n").await;
        let result =
            ActionReviewClassifier::classify(&judge(url), ActionReviewMode::SystemSafety, action())
                .await;
        assert_eq!(result, ActionClassification::Uncertain(reason));
        sent.await.unwrap();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn one_http_slot_is_shared_across_both_modes_and_cancel_releases_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let judge = Arc::new(judge(format!("http://{}", listener.local_addr().unwrap())));
    let (started_tx, started_rx) = oneshot::channel();
    let holding = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut byte = [0; 1];
        socket.read_exact(&mut byte).await.unwrap();
        started_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let first = {
        let judge = judge.clone();
        tokio::spawn(async move {
            ActionReviewClassifier::classify(
                judge.as_ref(),
                ActionReviewMode::SystemSafety,
                action(),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(judge.slots.status().unwrap().active, 1);
    assert_eq!(
        ActionReviewClassifier::classify(
            judge.as_ref(),
            ActionReviewMode::IdentityBoundary,
            action()
        )
        .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(judge.slots.status().unwrap().active, 0);
    holding.abort();
    let _ = holding.await;
}
