use super::*;
use eve_control_api::{ControlPhase, GenerationKey};
use eve_message_api::*;
use eve_session_api::SessionKey;

fn input() -> RelationInput {
    RelationInput {
        message: IncomingMessage {
            message_id: "secret-message".into(),
            text: "正文不进入事件".into(),
            target: GenerationKey {
                session: SessionKey::new("secret-session", "secret-user").unwrap(),
                task_id: "secret-task".into(),
                controller_epoch: [1; 16],
                generation: 1,
            },
            reply_to: None,
        },
        phase: ControlPhase::Generating,
        task_text: "任务正文".into(),
        cancel_requested: false,
        started_tools: Some(0),
        clarification: None,
    }
}
struct Legacy;
impl RelationJudge for Legacy {
    fn judge(&self, _: RelationInput) -> RelationFuture<'_> {
        Box::pin(async { Err(RelationError::Unavailable) })
    }
}

#[tokio::test]
async fn old_custom_judge_remains_compatible_and_explicitly_unknown() {
    let observer = Arc::new(BoundedRelationDiagnostics::default());
    assert_eq!(
        Legacy.judge_observed(input(), observer.clone()).await,
        Err(RelationError::Unavailable)
    );
    let report = observer.snapshot();
    assert_eq!(report.coverage, DiagnosticCoverage::Unsupported);
    assert_eq!(report.counts, None);
    assert_eq!(report.events, [RelationObservation::Unsupported]);
}

#[tokio::test]
async fn supported_wrapper_cannot_turn_a_legacy_delegate_into_known_zero_counts() {
    let observer = Arc::new(BoundedRelationDiagnostics::default());
    observe_relation(observer.as_ref(), RelationObservation::Supported);
    let guard = RelationObservationGuard::stage(observer.clone(), RelationStage::Primary);
    let result = Legacy.judge_observed(input(), observer.clone()).await;
    guard.finish(RelationOutcome::from(result.unwrap_err()));
    let report = observer.snapshot();
    assert_eq!(report.coverage, DiagnosticCoverage::Unsupported);
    assert_eq!(report.counts, None);
}

#[tokio::test]
async fn cancellation_and_concurrent_observers_keep_independent_terminal_events() {
    let left = Arc::new(BoundedRelationDiagnostics::default());
    let right = Arc::new(BoundedRelationDiagnostics::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn({
        let observer = left.clone();
        let entered = entered.clone();
        async move {
            observe_relation(observer.as_ref(), RelationObservation::Supported);
            let _guard =
                RelationObservationGuard::attempt(observer, RelationAttempt::ClassifierCall);
            entered.notify_one();
            std::future::pending::<()>().await;
        }
    });
    entered.notified().await;
    observe_relation(right.as_ref(), RelationObservation::Supported);
    RelationObservationGuard::stage(right.clone(), RelationStage::Rules)
        .finish(RelationOutcome::Completed);
    task.abort();
    let _ = task.await;
    let left = left.snapshot();
    let right = right.snapshot();
    assert_eq!(left.coverage, DiagnosticCoverage::Complete);
    assert_eq!(left.counts.unwrap().classifier_calls, 1);
    assert!(matches!(
        left.events.last(),
        Some(RelationObservation::Finished {
            outcome: RelationOutcome::Dropped,
            ..
        })
    ));
    assert_eq!(right.counts.unwrap().rules_started, 1);
    assert!(!right.events.iter().any(|event| matches!(
        event,
        RelationObservation::Finished {
            outcome: RelationOutcome::Dropped,
            ..
        }
    )));
}

#[test]
fn overflow_or_unbalanced_events_never_claim_zero_or_complete_counts() {
    let observer = BoundedRelationDiagnostics::default();
    for _ in 0..MAX_OBSERVATIONS + 3 {
        observer.observe(RelationObservation::Supported);
    }
    let snapshot = observer.snapshot();
    assert_eq!(snapshot.events.len(), MAX_OBSERVATIONS);
    assert_eq!(snapshot.coverage, DiagnosticCoverage::Overflow);
    assert_eq!(snapshot.counts, None);
    let observer = BoundedRelationDiagnostics::default();
    observer.observe(RelationObservation::Supported);
    observer.observe(RelationObservation::Started {
        operation: RelationOperation::Stage(RelationStage::Primary),
    });
    assert_eq!(observer.snapshot().coverage, DiagnosticCoverage::Invalid);
    assert_eq!(observer.snapshot().counts, None);
    assert_eq!(
        BoundedRelationDiagnostics::default().snapshot().coverage,
        DiagnosticCoverage::Unreported
    );
}

struct Panics;
impl RelationObserver for Panics {
    fn observe(&self, _: RelationObservation) {
        panic!("observer fixture");
    }
}
#[test]
fn observer_panic_does_not_escape_start_finish_or_drop() {
    let observer = Arc::new(Panics);
    observe_relation(observer.as_ref(), RelationObservation::Supported);
    RelationObservationGuard::stage(observer.clone(), RelationStage::Rules)
        .finish(RelationOutcome::Completed);
    drop(RelationObservationGuard::attempt(
        observer,
        RelationAttempt::ModelProviderCall,
    ));
}
