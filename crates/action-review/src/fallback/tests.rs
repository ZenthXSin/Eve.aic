use super::*;
use eve_developer_api::{ActionClassificationFuture, ReviewAuthorization, ReviewRisk};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn request() -> ActionReviewRequest {
    ActionReviewRequest {
        model_session: Some(
            eve_llm_api::ModelSession::anchored("review", None, "persisted-task-7").unwrap(),
        ),
        action: "具体系统变更".into(),
        program: "eve.apply_self_upgrade".into(),
        arguments: vec!["完整参数和差异".into()],
        workspace: "/workspace".into(),
        isolated: false,
        cwd: ".".into(),
        policy_reason: "检查直接系统影响".into(),
        claimed_confirmation: false,
        delegated: true,
        user_messages: vec![],
        recent_actions: vec![],
    }
}
fn verdict(allow: bool) -> ActionReviewVerdict {
    ActionReviewVerdict {
        risk: if allow {
            ReviewRisk::Low
        } else {
            ReviewRisk::High
        },
        authorization: ReviewAuthorization::Unknown,
        allow,
        rationale: "具体系统影响".into(),
    }
}
fn limits() -> ReviewChainLimits {
    ReviewChainLimits {
        classifier_timeout: Duration::from_secs(4),
        total_timeout: Duration::from_secs(10),
        confidence_threshold: 80,
    }
}

struct Classifier {
    decision: ActionClassification,
    delay: Duration,
    panic: bool,
    seen: Mutex<Vec<(ActionReviewMode, ActionReviewRequest)>>,
    active: Arc<AtomicUsize>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ActionReviewClassifier for Classifier {
    fn classify(
        &self,
        mode: ActionReviewMode,
        request: ActionReviewRequest,
    ) -> ActionClassificationFuture<'_> {
        Box::pin(async move {
            self.seen.lock().unwrap().push((mode, request));
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(self.active.clone());
            assert!(!self.panic, "synthetic classifier panic");
            tokio::time::sleep(self.delay).await;
            self.decision.clone()
        })
    }
}
struct Fallback {
    seen: Mutex<Vec<(ActionReviewMode, ActionReviewRequest)>>,
    delay: Duration,
}
impl Fallback {
    fn run(&self, mode: ActionReviewMode, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        Box::pin(async move {
            self.seen.lock().unwrap().push((mode, request));
            tokio::time::sleep(self.delay).await;
            Ok(verdict(true))
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
fn classifier(decision: ActionClassification, delay: Duration) -> Arc<Classifier> {
    Arc::new(Classifier {
        decision,
        delay,
        panic: false,
        seen: Mutex::new(vec![]),
        active: Arc::new(AtomicUsize::new(0)),
    })
}
fn fallback(delay: Duration) -> Arc<Fallback> {
    Arc::new(Fallback {
        seen: Mutex::new(vec![]),
        delay,
    })
}

#[tokio::test(start_paused = true)]
async fn certain_allow_and_deny_end_both_paths_without_fallback() {
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        for allow in [true, false] {
            let expected = verdict(allow);
            let classifier = classifier(
                ActionClassification::Decided {
                    verdict: expected.clone(),
                    confidence: 80,
                },
                Duration::ZERO,
            );
            let fallback = fallback(Duration::ZERO);
            let observations = Arc::new(Observations::default());
            let chain =
                FallbackActionReviewer::new(Some(classifier.clone()), Some(fallback.clone()))
                    .with_observer(observations.clone());
            assert_eq!(run(&chain, mode, request()).await.unwrap(), expected);
            assert_eq!(*classifier.seen.lock().unwrap(), vec![(mode, request())]);
            assert!(fallback.seen.lock().unwrap().is_empty());
            assert_eq!(
                *observations.0.lock().unwrap(),
                vec![(mode, ActionReviewRoute::Classifier { confidence: 80 })]
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn uncertainty_low_score_invalid_verdict_and_panic_fallback_once_with_original_request() {
    let cases = [
        (
            ActionClassification::Uncertain(ActionReviewUncertainty::Busy),
            false,
            ActionReviewUncertainty::Busy,
        ),
        (
            ActionClassification::Uncertain(ActionReviewUncertainty::InputTooLarge),
            false,
            ActionReviewUncertainty::InputTooLarge,
        ),
        (
            ActionClassification::Decided {
                verdict: verdict(true),
                confidence: 79,
            },
            false,
            ActionReviewUncertainty::LowConfidence,
        ),
        (
            ActionClassification::Decided {
                verdict: verdict(true),
                confidence: 101,
            },
            false,
            ActionReviewUncertainty::InvalidResponse,
        ),
        (
            ActionClassification::Decided {
                verdict: ActionReviewVerdict {
                    allow: true,
                    ..verdict(false)
                },
                confidence: 99,
            },
            false,
            ActionReviewUncertainty::InvalidResponse,
        ),
        (
            ActionClassification::Uncertain(ActionReviewUncertainty::Unavailable),
            true,
            ActionReviewUncertainty::Panicked,
        ),
    ];
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        for (decision, panic, reason) in &cases {
            let mut classifier = classifier(decision.clone(), Duration::ZERO);
            Arc::get_mut(&mut classifier).unwrap().panic = *panic;
            let fallback = fallback(Duration::ZERO);
            let observations = Arc::new(Observations::default());
            let chain =
                FallbackActionReviewer::new(Some(classifier.clone()), Some(fallback.clone()))
                    .with_observer(observations.clone());
            let mut original = request();
            original.arguments.push("完整原文".repeat(10_000));
            assert!(run(&chain, mode, original.clone()).await.is_ok());
            assert_eq!(*fallback.seen.lock().unwrap(), vec![(mode, original)]);
            assert_eq!(
                *observations.0.lock().unwrap(),
                vec![(mode, ActionReviewRoute::Fallback { reason: *reason })]
            );
            assert_eq!(classifier.active.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_preserves_remaining_deadline_and_drops_classifier_before_fallback() {
    let classifier = classifier(
        ActionClassification::Uncertain(ActionReviewUncertainty::Ambiguous),
        Duration::from_secs(20),
    );
    let fallback = fallback(Duration::from_secs(20));
    let observations = Arc::new(Observations::default());
    let chain = FallbackActionReviewer::new(Some(classifier.clone()), Some(fallback.clone()))
        .with_limits(limits())
        .unwrap()
        .with_observer(observations.clone());
    let start = Instant::now();
    assert!(
        chain
            .review_system_safety(request())
            .await
            .unwrap_err()
            .contains("超时")
    );
    let elapsed = Instant::now() - start;
    assert!(elapsed >= Duration::from_secs(10));
    assert!(elapsed <= Duration::from_millis(10_010));
    assert_eq!(classifier.active.load(Ordering::SeqCst), 0);
    assert_eq!(fallback.seen.lock().unwrap().len(), 1);
    assert_eq!(
        *observations.0.lock().unwrap(),
        vec![(
            ActionReviewMode::SystemSafety,
            ActionReviewRoute::Fallback {
                reason: ActionReviewUncertainty::Timeout
            }
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_outer_review_releases_classifier_without_starting_fallback() {
    let classifier = classifier(
        ActionClassification::Uncertain(ActionReviewUncertainty::Ambiguous),
        Duration::from_secs(20),
    );
    let fallback = fallback(Duration::ZERO);
    let chain = Arc::new(
        FallbackActionReviewer::new(Some(classifier.clone()), Some(fallback.clone()))
            .with_limits(limits())
            .unwrap(),
    );
    let running = tokio::spawn(async move { chain.review_system_safety(request()).await });
    tokio::task::yield_now().await;
    assert_eq!(classifier.active.load(Ordering::SeqCst), 1);
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    assert_eq!(classifier.active.load(Ordering::SeqCst), 0);
    assert!(fallback.seen.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn no_fallback_is_needed_for_certain_result_but_uncertain_and_oversized_never_execute() {
    let classifier = classifier(
        ActionClassification::Decided {
            verdict: verdict(true),
            confidence: 99,
        },
        Duration::ZERO,
    );
    let chain = FallbackActionReviewer::new(Some(classifier.clone()), None);
    assert!(chain.review_system_safety(request()).await.is_ok());
    let mut oversized = request();
    oversized.arguments.push("x".repeat(256 * 1024));
    assert!(chain.review_system_safety(oversized).await.is_err());
    assert_eq!(classifier.seen.lock().unwrap().len(), 1);
    let chain = FallbackActionReviewer::new(None, None);
    assert!(
        chain
            .review(request())
            .await
            .unwrap_err()
            .contains("后备审查模型不可用")
    );
}
