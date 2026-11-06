#[allow(dead_code)]
#[path = "../../llm-openai/tests/support/mod.rs"]
mod support;

use eve_action_review::{FallbackActionReviewer, ReviewChainLimits};
use eve_developer_api::{
    ActionReviewFuture, ActionReviewMode, ActionReviewObserver, ActionReviewRequest,
    ActionReviewRoute, ActionReviewUncertainty, ActionReviewVerdict, ActionReviewer,
    ReviewAuthorization, ReviewRisk,
};
use eve_jev::{JevConfig, JevRelationJudge};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{Reply, Server};

fn request() -> ActionReviewRequest {
    ActionReviewRequest {
        model_session: Some(
            eve_llm_api::ModelSession::anchored("review", None, "persistent-test-task")
                .unwrap()
                .with_turn("turn-1")
                .unwrap(),
        ),
        action: "具体系统变更".into(),
        program: "eve.apply_self_upgrade".into(),
        arguments: vec!["完整差异".into()],
        workspace: "/workspace".into(),
        isolated: false,
        cwd: ".".into(),
        policy_reason: "直接系统影响".into(),
        claimed_confirmation: false,
        delegated: true,
        user_messages: vec![],
        recent_actions: vec![],
    }
}
fn choice(selected: &str, labels: &[&str], confidence: f64) -> Value {
    let probabilities: BTreeMap<_, _> = labels
        .iter()
        .map(|label| (*label, if *label == selected { 1.0 } else { 0.0 }))
        .collect();
    json!({"type":"choice", "choice":selected, "confidence":confidence, "probabilities":probabilities})
}
fn response(mode: ActionReviewMode, harm: &str, confidence: f64) -> Reply {
    let mut body = json!({"answers":{
        "direct_system_harm": choice(harm, &["none", "bounded", "high", "critical", "uncertain"], confidence),
        "grounded":{"type":"noul", "noul":1.0}
    }});
    if mode == ActionReviewMode::IdentityBoundary {
        body["answers"]["boundary_intent"] = choice(
            "explicit",
            &["explicit", "absent", "revoked", "uncertain"],
            1.0,
        );
    }
    Reply::json(body)
}
struct Fallback(Mutex<Vec<(ActionReviewMode, ActionReviewRequest)>>);
impl Fallback {
    fn run(&self, mode: ActionReviewMode, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.0.lock().unwrap().push((mode, request));
        Box::pin(async {
            Ok(ActionReviewVerdict {
                risk: ReviewRisk::Low,
                authorization: ReviewAuthorization::High,
                allow: true,
                rationale: "原始完整请求的后备结论".into(),
            })
        })
    }
}
impl ActionReviewer for Fallback {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(ActionReviewMode::IdentityBoundary, request)
    }
    fn review_system_safety(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(ActionReviewMode::SystemSafety, request)
    }
}
#[derive(Default)]
struct Observations(Mutex<Vec<(ActionReviewMode, ActionReviewRoute)>>);
impl ActionReviewObserver for Observations {
    fn route(&self, mode: ActionReviewMode, route: ActionReviewRoute) {
        self.0.lock().unwrap().push((mode, route));
    }
}
fn judge(server: &Server) -> Arc<JevRelationJudge> {
    Arc::new(
        JevRelationJudge::new(
            JevConfig {
                base_url: server.url.trim_end_matches("/v1/responses").into(),
                ..JevConfig::default()
            },
            "synthetic-jev-only",
        )
        .unwrap(),
    )
}
async fn run(
    chain: &dyn ActionReviewer,
    mode: ActionReviewMode,
    request: ActionReviewRequest,
) -> Result<ActionReviewVerdict, String> {
    match mode {
        ActionReviewMode::IdentityBoundary => chain.review(request).await,
        ActionReviewMode::SystemSafety => chain.review_system_safety(request).await,
    }
}

#[tokio::test]
async fn real_jev_http_allow_and_deny_do_not_call_either_llm_entry() {
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        for (harm, allow) in [("none", true), ("critical", false)] {
            let mut server = Server::start(vec![response(mode, harm, 0.99)]).await;
            let fallback = Arc::new(Fallback(Mutex::new(vec![])));
            let chain = FallbackActionReviewer::new(Some(judge(&server)), Some(fallback.clone()));
            assert_eq!(run(&chain, mode, request()).await.unwrap().allow, allow);
            assert!(fallback.0.lock().unwrap().is_empty());
            let sent = server.next().await;
            assert!(sent.headers.starts_with("POST /v1/systemone HTTP/1.1\r\n"));
            assert!(sent.headers.contains("Bearer synthetic-jev-only"));
            assert_eq!(
                sent.body["state"]["request"],
                serde_json::to_value(request()).unwrap()
            );
            assert!(server.requests.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn real_jev_uncertainty_invalid_response_and_http_failure_call_only_the_correct_fallback_once()
 {
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        let mut unavailable = Reply::json(json!({"error":"synthetic provider text"}));
        unavailable.status = 503;
        for (reply, reason) in [
            (
                response(mode, "none", 0.79),
                ActionReviewUncertainty::LowConfidence,
            ),
            (
                response(mode, "uncertain", 0.99),
                ActionReviewUncertainty::Ambiguous,
            ),
            (
                Reply::json(json!({"answers":{}})),
                ActionReviewUncertainty::InvalidResponse,
            ),
            (unavailable, ActionReviewUncertainty::Unavailable),
        ] {
            let mut server = Server::start(vec![reply]).await;
            let fallback = Arc::new(Fallback(Mutex::new(vec![])));
            let observations = Arc::new(Observations::default());
            let chain = FallbackActionReviewer::new(Some(judge(&server)), Some(fallback.clone()))
                .with_observer(observations.clone());
            assert!(run(&chain, mode, request()).await.unwrap().allow);
            assert_eq!(*fallback.0.lock().unwrap(), vec![(mode, request())]);
            assert_eq!(
                *observations.0.lock().unwrap(),
                vec![(mode, ActionReviewRoute::Fallback { reason })]
            );
            server.next().await;
            assert!(server.requests.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn jev_input_capacity_falls_back_with_full_action_and_session_without_http() {
    let mut server = Server::start(vec![]).await;
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        let fallback = Arc::new(Fallback(Mutex::new(vec![])));
        let observations = Arc::new(Observations::default());
        let chain = FallbackActionReviewer::new(Some(judge(&server)), Some(fallback.clone()))
            .with_observer(observations.clone());
        let mut original = request();
        original
            .arguments
            .push("x".repeat(eve_jev::MAX_INPUT_BYTES));
        original.validate_capacity().unwrap();
        assert!(run(&chain, mode, original.clone()).await.unwrap().allow);
        assert_eq!(*fallback.0.lock().unwrap(), vec![(mode, original)]);
        assert_eq!(
            *observations.0.lock().unwrap(),
            vec![(
                mode,
                ActionReviewRoute::Fallback {
                    reason: ActionReviewUncertainty::InputTooLarge
                }
            )]
        );
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn real_busy_slot_falls_back_and_cancellation_frees_the_shared_http_client() {
    let mut slow = response(ActionReviewMode::SystemSafety, "none", 0.99);
    slow.header_delay = Duration::from_secs(3);
    let mut server = Server::start(vec![
        slow,
        response(ActionReviewMode::SystemSafety, "none", 0.99),
    ])
    .await;
    let fallback = Arc::new(Fallback(Mutex::new(vec![])));
    let chain = Arc::new(FallbackActionReviewer::new(
        Some(judge(&server)),
        Some(fallback.clone()),
    ));
    let first = {
        let chain = chain.clone();
        tokio::spawn(async move { chain.review_system_safety(request()).await })
    };
    server.next().await;
    assert!(chain.review(request()).await.unwrap().allow);
    assert_eq!(
        *fallback.0.lock().unwrap(),
        vec![(ActionReviewMode::IdentityBoundary, request())]
    );
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(chain.review_system_safety(request()).await.unwrap().allow);
    server.next().await;
    assert_eq!(fallback.0.lock().unwrap().len(), 1);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn real_jev_timeout_enters_fallback_once_with_remaining_total_deadline() {
    let mut slow = response(ActionReviewMode::SystemSafety, "none", 0.99);
    slow.header_delay = Duration::from_secs(3);
    let mut server = Server::start(vec![slow]).await;
    let fallback = Arc::new(Fallback(Mutex::new(vec![])));
    let observations = Arc::new(Observations::default());
    let chain = FallbackActionReviewer::new(Some(judge(&server)), Some(fallback.clone()))
        .with_limits(ReviewChainLimits {
            classifier_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_secs(5),
            confidence_threshold: 80,
        })
        .unwrap()
        .with_observer(observations.clone());
    assert!(chain.review_system_safety(request()).await.unwrap().allow);
    assert_eq!(fallback.0.lock().unwrap().len(), 1);
    assert_eq!(
        *observations.0.lock().unwrap(),
        vec![(
            ActionReviewMode::SystemSafety,
            ActionReviewRoute::Fallback {
                reason: ActionReviewUncertainty::Timeout
            }
        )]
    );
    server.next().await;
    assert!(server.requests.try_recv().is_err());
}
