use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{
    CognitionLoopPlugin, EchoReceiptVerifier, EndogenousPlanner, EvidenceDrivePolicy,
    ReflectionDrivePolicy, ReflectionVerifier, current_reflection, evaluate_agenda,
    evaluate_reflection_agenda,
};
use eve_cognition_plugin::{
    CognitionController, CognitionPlugin, UserGoalFeedback, UserGoalFileObservation,
};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::PluginId;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const NOW: u64 = 1_000_000;
const SECRET: &str = "private-document-body-must-not-appear-in-agenda";
type EventMutation = Box<dyn Fn(&mut CognitiveEvent)>;

async fn open() -> (Kernel, CognitionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(MemoryStateStore::default()),
        ..KernelServices::default()
    });
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(COGNITION_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    (kernel, admin)
}

fn parent(id: &str, priority: u8) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: "cognition.cli".into(),
            reference: format!("request-{id}"),
        },
        visibility: Visibility::User("alice".into()),
        description: SECRET.into(),
        verification: "user-goal:v1".into(),
        priority,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 0,
            max_attempts: 1,
            timeout_ms: 1000,
        },
        stop_condition: "user-confirmation".into(),
        expires_at_ms: None,
        status: GoalStatus::Waiting,
        wait_reason: Some("需要新的可核对信息".into()),
        block_reason: None,
        execution: None,
        feedback: None,
    }
}

fn ready(id: &str, priority: u8) -> Goal {
    let mut goal = parent(id, priority);
    goal.status = GoalStatus::Ready;
    goal.wait_reason = None;
    goal.verification = "echo:ok".into();
    goal.stop_condition = "single-attempt".into();
    goal.budget.max_tool_calls = 1;
    goal
}

fn scope() -> ExecutionScope {
    ExecutionScope {
        subject_id: "eve".into(),
        access: ReadAccess::User("alice".into()),
        sources: vec![AllowedSource {
            kind: SourceKind::User,
            channel: "cognition.cli".into(),
        }],
    }
}

fn options() -> EndogenousOptions {
    EndogenousOptions {
        scope: scope(),
        max_derivations: 8,
        timeout_ms: 1000,
    }
}

fn loop_options() -> LoopOptions {
    LoopOptions {
        scope: scope(),
        poll_interval_ms: 250,
        max_executions: 8,
    }
}

fn seed(admin: &CognitionController, goals: Vec<Goal>) {
    let snapshot = admin.snapshot().unwrap();
    let mut state = snapshot.state;
    for goal in goals {
        state.goals.insert(goal.id.clone(), goal);
    }
    admin.replace(snapshot.revision, state).unwrap();
}

fn initial_event(goal: &Goal, at_ms: u64) -> CognitiveEvent {
    CognitiveEvent {
        id: goal.source.reference.clone(),
        kind: CognitiveEventKind::ExternalInput,
        source: goal.source.clone(),
        visibility: goal.visibility.clone(),
        goal_id: Some(goal.id.clone()),
        caused_by: None,
        at_ms,
        summary: SECRET.into(),
    }
}

fn save_feedback(admin: &CognitionController, goal_id: &str, at_ms: u64) {
    let snapshot = admin.snapshot().unwrap();
    UserGoalFeedback::new(
        Arc::new(admin.clone()),
        "eve".into(),
        "alice".into(),
        "cognition.cli".into(),
        "cognition.feedback".into(),
    )
    .unwrap()
    .submit(GoalFeedbackInput {
        goal_id: goal_id.into(),
        expected_goal_revision: snapshot.state.goals[goal_id].revision,
        feedback_id: format!("feedback-{goal_id}-{at_ms}"),
        text: SECRET.into(),
        at_ms,
    })
    .unwrap();
}

fn save_observation(admin: &CognitionController, goal_id: &str, at_ms: u64) {
    let snapshot = admin.snapshot().unwrap();
    UserGoalFileObservation::new(
        Arc::new(admin.clone()),
        "eve".into(),
        "alice".into(),
        "cognition.cli".into(),
    )
    .unwrap()
    .submit(FileObservationInput {
        goal_id: goal_id.into(),
        expected_goal_revision: snapshot.state.goals[goal_id].revision,
        observation_source_id: format!("file-source:{}", "a".repeat(64)),
        sha256: "b".repeat(64),
        byte_count: SECRET.len() as u64,
        text_excerpt: SECRET.into(),
        text_truncated: false,
        observed_at_ms: at_ms,
    })
    .unwrap();
}

/// Return an admission error after recording selection: the test observes the real worker
/// without pretending to execute tools or producing a synthetic successful receipt.
#[derive(Default)]
struct RecordingExecutor {
    selected: Mutex<Vec<String>>,
    submitted: tokio::sync::Notify,
}

impl GoalExecutor for RecordingExecutor {
    fn submit(
        &self,
        _goal: &Goal,
        _attempt: &ExecutionAttempt,
    ) -> LoopResult<eve_control_api::GenerationKey> {
        panic!("worker must pass the monotonic deadline to submit_before")
    }

    fn submit_before(
        &self,
        goal: &Goal,
        _attempt: &ExecutionAttempt,
        deadline: Instant,
    ) -> LoopResult<eve_control_api::GenerationKey> {
        assert!(deadline > Instant::now());
        self.selected.lock().unwrap().push(goal.id.clone());
        self.submitted.notify_one();
        Err(LoopError::Execution)
    }

    fn cancel(&self, _key: &eve_control_api::GenerationKey) -> LoopResult<()> {
        panic!("a rejected admission has no generation to cancel")
    }

    fn wait(
        &self,
        _key: &eve_control_api::GenerationKey,
    ) -> LoopFuture<'static, GoalExecutionReport> {
        panic!("a rejected admission has no generation to await")
    }
}

fn rank(snapshot: &CognitiveSnapshot, ids: &[&str], now_ms: u64) -> Vec<RankedGoal> {
    let goals = ids
        .iter()
        .map(|id| snapshot.state.goals[*id].clone())
        .collect::<Vec<_>>();
    EvidenceDrivePolicy
        .rank_with_state(snapshot, &goals, now_ms)
        .unwrap()
}

#[tokio::test]
async fn priority_deadline_age_and_total_have_explicit_bounded_effects() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![parent("goal", 20)]);
    let original = admin.snapshot().unwrap();
    assert_eq!(rank(&original, &["goal"], NOW)[0].strength, 20);
    for (remaining, expected) in [
        (1, 35),
        (60_000, 35),
        (60_001, 28),
        (3_600_000, 28),
        (3_600_001, 20),
    ] {
        let mut snapshot = original.clone();
        snapshot.state.goals.get_mut("goal").unwrap().expires_at_ms = Some(NOW + remaining);
        assert_eq!(rank(&snapshot, &["goal"], NOW)[0].strength, expected);
    }
    for expires in [NOW - 1, NOW] {
        let mut snapshot = original.clone();
        snapshot.state.goals.get_mut("goal").unwrap().expires_at_ms = Some(expires);
        assert_eq!(rank(&snapshot, &["goal"], NOW)[0].strength, 20);
    }
    let started_at = 1000;
    let mut aged = original.clone();
    aged.state
        .events
        .push(initial_event(&aged.state.goals["goal"], started_at));
    for (elapsed, expected) in [
        (21_600_000 - 1, 20),
        (21_600_000, 21),
        (21_600_000 * 10, 30),
        (21_600_000 * 100, 30),
    ] {
        assert_eq!(
            rank(&aged, &["goal"], started_at + elapsed)[0].strength,
            expected
        );
    }
    aged.state.goals.get_mut("goal").unwrap().priority = 99;
    assert_eq!(rank(&aged, &["goal"], u64::MAX)[0].strength, 100);
    assert_eq!(admin.snapshot().unwrap(), original);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn future_unrelated_inferred_or_mismatched_events_do_not_make_goals_older() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![parent("goal", 20)]);
    let original = admin.snapshot().unwrap();
    let now = 21_600_000 * 20;
    let event = initial_event(&original.state.goals["goal"], 1);
    let mutations: Vec<EventMutation> = vec![
        Box::new(move |e| e.at_ms = now + 1),
        Box::new(|e| e.at_ms = 0),
        Box::new(|e| e.kind = CognitiveEventKind::DriveEvaluated),
        Box::new(|e| e.source.kind = SourceKind::Inference),
        Box::new(|e| e.source.kind = SourceKind::Internal),
        Box::new(|e| e.source.channel = "other-channel".into()),
        Box::new(|e| e.source.reference = "another-request".into()),
        Box::new(|e| e.goal_id = Some("another-goal".into())),
        Box::new(|e| e.visibility = Visibility::User("bob".into())),
    ];
    for mutate in mutations {
        let mut snapshot = original.clone();
        let mut bad = event.clone();
        mutate(&mut bad);
        snapshot.state.events.push(bad);
        assert_eq!(rank(&snapshot, &["goal"], now)[0].strength, 20);
    }
    for kind in [SourceKind::Inference, SourceKind::Internal] {
        let mut snapshot = original.clone();
        snapshot.state.goals.get_mut("goal").unwrap().source.kind = kind;
        snapshot
            .state
            .events
            .push(initial_event(&snapshot.state.goals["goal"], 1));
        assert_eq!(rank(&snapshot, &["goal"], now)[0].strength, 20);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn only_current_persisted_feedback_supplies_freshness_and_never_leaks_text() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![parent("goal", 20)]);
    save_feedback(&admin, "goal", NOW);
    let original = admin.snapshot().unwrap();
    for (elapsed, expected) in [
        (0, 35),
        (300_000, 35),
        (300_001, 28),
        (3_600_000, 28),
        (3_600_001, 20),
    ] {
        assert_eq!(
            rank(&original, &["goal"], NOW + elapsed)[0].strength,
            expected
        );
    }
    assert_eq!(rank(&original, &["goal"], NOW - 1)[0].strength, 20);
    let scored = rank(&original, &["goal"], NOW).remove(0);
    assert!(!scored.reason.contains(SECRET));
    assert!(!scored.reason.contains(&original.state.events[0].id));
    assert!(scored.reason.contains("fresh_evidence_id_sha256="));
    let mutations: Vec<EventMutation> = vec![
        Box::new(|e| e.kind = CognitiveEventKind::StateChanged),
        Box::new(|e| e.source.kind = SourceKind::Inference),
        Box::new(|e| e.source.channel = "cognition.cli".into()),
        Box::new(|e| e.source.reference = "unrelated-feedback".into()),
        Box::new(|e| e.id = "another-id".into()),
        Box::new(|e| e.goal_id = Some("another-goal".into())),
        Box::new(|e| e.visibility = Visibility::User("bob".into())),
        Box::new(|e| e.summary = "unverified plain text".into()),
    ];
    for mutate in mutations {
        let mut snapshot = original.clone();
        mutate(&mut snapshot.state.events[0]);
        assert_eq!(rank(&snapshot, &["goal"], NOW)[0].strength, 20);
    }
    let mut older_revision = original.clone();
    older_revision.state.goals.get_mut("goal").unwrap().revision += 1;
    assert_eq!(rank(&older_revision, &["goal"], NOW)[0].strength, 20);
    let mut overwritten_wait = original.clone();
    overwritten_wait
        .state
        .goals
        .get_mut("goal")
        .unwrap()
        .wait_reason = Some("different latest input".into());
    assert_eq!(rank(&overwritten_wait, &["goal"], NOW)[0].strength, 20);
    assert_eq!(admin.snapshot().unwrap(), original);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn file_freshness_requires_the_current_complete_environment_receipt() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![parent("goal", 20)]);
    save_observation(&admin, "goal", NOW);
    let original = admin.snapshot().unwrap();
    assert_eq!(rank(&original, &["goal"], NOW)[0].strength, 35);
    let event_id = &original.state.events[0].id;
    let reason = &rank(&original, &["goal"], NOW)[0].reason;
    assert!(!reason.contains(SECRET));
    assert!(!reason.contains(event_id));
    assert!(!reason.contains("file-source:"));
    let mutations: Vec<EventMutation> = vec![
        Box::new(|e| e.kind = CognitiveEventKind::Feedback),
        Box::new(|e| e.source.kind = SourceKind::User),
        Box::new(|e| e.source.kind = SourceKind::Inference),
        Box::new(|e| e.source.channel = "untrusted-observer".into()),
        Box::new(|e| e.source.reference = format!("file-source:{}", "c".repeat(64))),
        Box::new(|e| e.id = "forged-observation-id".into()),
        Box::new(|e| e.at_ms += 1),
        Box::new(|e| e.goal_id = Some("another-goal".into())),
        Box::new(|e| e.visibility = Visibility::Public),
    ];
    for mutate in mutations {
        let mut snapshot = original.clone();
        mutate(&mut snapshot.state.events[0]);
        assert_eq!(rank(&snapshot, &["goal"], NOW + 2)[0].strength, 20);
    }
    let mut older_revision = original.clone();
    older_revision.state.goals.get_mut("goal").unwrap().revision += 1;
    assert_eq!(rank(&older_revision, &["goal"], NOW)[0].strength, 20);
    assert_eq!(rank(&original, &["goal"], NOW - 1)[0].strength, 20);
    // A later user fact replaces the current source; the old observation cannot stack bonuses.
    save_feedback(&admin, "goal", NOW + 1);
    assert_eq!(
        rank(&admin.snapshot().unwrap(), &["goal"], NOW + 1)[0].strength,
        35
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn ranking_is_deterministic_and_rejects_detached_or_duplicate_candidates() {
    let (kernel, admin) = open().await;
    seed(
        &admin,
        vec![parent("b", 50), parent("a", 50), parent("z", 70)],
    );
    let snapshot = admin.snapshot().unwrap();
    let first = rank(&snapshot, &["b", "z", "a"], NOW);
    let second = rank(&snapshot, &["a", "b", "z"], NOW);
    assert_eq!(
        first.iter().map(|v| v.goal_id.as_str()).collect::<Vec<_>>(),
        ["z", "a", "b"]
    );
    assert_eq!(
        first
            .iter()
            .map(|v| (&v.goal_id, v.strength, &v.reason))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|v| (&v.goal_id, v.strength, &v.reason))
            .collect::<Vec<_>>()
    );
    let candidate = snapshot.state.goals["a"].clone();
    assert!(
        EvidenceDrivePolicy
            .rank_with_state(&snapshot, &[candidate.clone(), candidate.clone()], NOW)
            .is_err()
    );
    let mut detached = candidate;
    detached.priority = 100;
    assert!(
        EvidenceDrivePolicy
            .rank_with_state(&snapshot, &[detached], NOW)
            .is_err()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn preview_filters_match_admission_constraints_and_never_expose_hidden_ids() {
    let (kernel, admin) = open().await;
    let mut expired = ready("expired", 100);
    expired.expires_at_ms = Some(NOW);
    let mut denied = ready("denied", 100);
    denied.source.channel = "other-host".into();
    let mut unsupported = ready("unsupported", 100);
    unsupported.verification = "unknown:v1".into();
    let mut hidden = ready("bob-secret-goal", 100);
    hidden.visibility = Visibility::User("bob".into());
    seed(
        &admin,
        vec![
            ready("ready", 10),
            parent("waiting", 100),
            expired,
            denied,
            unsupported,
            hidden,
        ],
    );
    let original = admin.snapshot().unwrap();
    let mut snapshot = original.clone();
    let mut invalid = ready("invalid-budget", 100);
    invalid.budget.timeout_ms = 0;
    snapshot.state.goals.insert(invalid.id.clone(), invalid);
    let preview = evaluate_agenda(
        &snapshot,
        &loop_options(),
        &EvidenceDrivePolicy,
        &EchoReceiptVerifier,
        0,
        NOW,
    )
    .unwrap();
    assert_eq!(preview.revision, original.revision);
    assert_eq!(preview.evaluated_at_ms, NOW);
    assert_eq!(preview.blocker, None);
    assert_eq!(
        preview
            .ranked
            .iter()
            .map(|g| g.goal_id.as_str())
            .collect::<Vec<_>>(),
        ["ready"]
    );
    assert_eq!(
        preview.excluded,
        vec![
            AgendaExclusion {
                goal_id: "denied".into(),
                reason: AgendaExclusionReason::ScopeDenied
            },
            AgendaExclusion {
                goal_id: "expired".into(),
                reason: AgendaExclusionReason::Expired
            },
            AgendaExclusion {
                goal_id: "invalid-budget".into(),
                reason: AgendaExclusionReason::InvalidBudget
            },
            AgendaExclusion {
                goal_id: "unsupported".into(),
                reason: AgendaExclusionReason::UnsupportedVerification
            },
            AgendaExclusion {
                goal_id: "waiting".into(),
                reason: AgendaExclusionReason::NotReady
            },
        ]
    );
    assert!(!format!("{preview:?}").contains("bob-secret-goal"));
    assert!(!format!("{preview:?}").contains(SECRET));
    let limited = evaluate_agenda(
        &snapshot,
        &loop_options(),
        &EvidenceDrivePolicy,
        &EchoReceiptVerifier,
        8,
        NOW,
    )
    .unwrap();
    assert_eq!(limited.blocker, Some(AgendaBlocker::ExecutionLimitReached));
    assert_eq!(limited.ranked, preview.ranked);
    snapshot
        .state
        .goals
        .get_mut("bob-secret-goal")
        .unwrap()
        .status = GoalStatus::Executing;
    let busy = evaluate_agenda(
        &snapshot,
        &loop_options(),
        &EvidenceDrivePolicy,
        &EchoReceiptVerifier,
        0,
        NOW,
    )
    .unwrap();
    assert_eq!(busy.blocker, Some(AgendaBlocker::AlreadyExecuting));
    assert_eq!(busy.ranked, preview.ranked);
    assert!(!format!("{busy:?}").contains("bob-secret-goal"));
    let mut wrong_subject = snapshot.clone();
    wrong_subject.subject_id = "another-subject".into();
    assert_eq!(
        evaluate_agenda(
            &wrong_subject,
            &loop_options(),
            &EvidenceDrivePolicy,
            &EchoReceiptVerifier,
            0,
            NOW
        ),
        Err(LoopError::Cognition(CognitionError::SubjectMismatch))
    );
    assert_eq!(
        evaluate_agenda(
            &snapshot,
            &loop_options(),
            &EvidenceDrivePolicy,
            &EchoReceiptVerifier,
            0,
            0
        ),
        Err(LoopError::InvalidInput)
    );
    assert_eq!(admin.snapshot().unwrap(), original);
    kernel.stop_all().await.unwrap();
}

struct FixedPolicy(Vec<RankedGoal>);
impl DrivePolicy for FixedPolicy {
    fn rank(&self, _goals: &[Goal], _now_ms: u64) -> LoopResult<Vec<RankedGoal>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn evaluation_supports_legacy_policies_but_rejects_expansion_duplicate_or_invalid_rankings() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![ready("a", 10), ready("b", 20)]);
    let snapshot = admin.snapshot().unwrap();
    let item = RankedGoal {
        goal_id: "a".into(),
        strength: 1,
        reason: "legacy".into(),
    };
    let preview = evaluate_agenda(
        &snapshot,
        &loop_options(),
        &FixedPolicy(vec![item.clone()]),
        &EchoReceiptVerifier,
        0,
        NOW,
    )
    .unwrap();
    assert_eq!(preview.ranked, std::slice::from_ref(&item));
    assert_eq!(
        preview.excluded,
        [AgendaExclusion {
            goal_id: "b".into(),
            reason: AgendaExclusionReason::PolicyOmitted
        }]
    );
    for bad in [
        vec![item.clone(), item.clone()],
        vec![RankedGoal {
            goal_id: "outside".into(),
            ..item.clone()
        }],
        vec![RankedGoal {
            strength: 101,
            ..item.clone()
        }],
        vec![RankedGoal {
            reason: String::new(),
            ..item
        }],
    ] {
        assert_eq!(
            evaluate_agenda(
                &snapshot,
                &loop_options(),
                &FixedPolicy(bad),
                &EchoReceiptVerifier,
                0,
                NOW
            ),
            Err(LoopError::InvalidInput)
        );
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn readonly_preview_and_live_worker_select_the_same_goal() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![ready("a-low", 10), ready("z-high", 90)]);
    let snapshot = admin.snapshot().unwrap();
    let options = LoopOptions {
        max_executions: 1,
        ..loop_options()
    };
    let preview = evaluate_agenda(
        &snapshot,
        &options,
        &EvidenceDrivePolicy,
        &EchoReceiptVerifier,
        0,
        NOW,
    )
    .unwrap();
    assert_eq!(preview.ranked[0].goal_id, "z-high");
    assert_eq!(admin.snapshot().unwrap(), snapshot);
    let executor = Arc::new(RecordingExecutor::default());
    let plugin = CognitionLoopPlugin::new(
        Arc::new(admin.clone()),
        Arc::new(EvidenceDrivePolicy),
        Arc::new(EchoReceiptVerifier),
        executor.clone(),
        options,
        vec![],
    )
    .unwrap();
    let control = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(LOOP_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), executor.submitted.notified())
        .await
        .expect("worker should admit the preview's first candidate");
    control.shutdown().await.unwrap();
    assert_eq!(
        *executor.selected.lock().unwrap(),
        [preview.ranked[0].goal_id.clone()]
    );
    assert_eq!(
        admin.snapshot().unwrap().state.goals["a-low"].status,
        GoalStatus::Ready
    );
    // The executor recorded selection but rejected admission, so no submitted
    // generation or fabricated successful tool execution is counted.
    assert_eq!(control.stats().unwrap().submitted, 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn reflection_preview_and_real_derivation_follow_evidence_score_before_id() {
    let (kernel, admin) = open().await;
    seed(
        &admin,
        vec![parent("a-baseline", 50), parent("z-new-fact", 50)],
    );
    save_feedback(&admin, "z-new-fact", NOW);
    let before = admin.snapshot().unwrap();
    let options = EndogenousOptions {
        max_derivations: 1,
        ..options()
    };
    let preview = evaluate_reflection_agenda(&before, &options, 0, NOW).unwrap();
    assert_eq!(
        preview
            .ranked
            .iter()
            .map(|item| item.goal_id.as_str())
            .collect::<Vec<_>>(),
        ["z-new-fact", "a-baseline"]
    );
    assert_eq!(preview.ranked[0].strength, 65);
    assert_eq!(admin.snapshot().unwrap(), before);
    let planner = EndogenousPlanner::new(Arc::new(admin.clone()), options.clone()).unwrap();
    let report = planner.reconcile(NOW).unwrap();
    let after = admin.snapshot().unwrap();
    let child = &after.state.goals[&report.created_goal_ids[0]];
    assert_eq!(child.source.reference, preview.ranked[0].goal_id);
    assert_eq!(child.priority, before.state.goals["z-new-fact"].priority);
    let drive = after
        .state
        .drives
        .values()
        .find(|drive| drive.goal_ids == [child.id.clone()])
        .unwrap();
    assert_eq!(drive.strength, preview.ranked[0].strength);
    assert_eq!(drive.reason, preview.ranked[0].reason);
    let limited = evaluate_reflection_agenda(&after, &options, 1, NOW + 1).unwrap();
    assert_eq!(limited.blocker, Some(AgendaBlocker::DerivationLimitReached));
    assert_eq!(
        limited
            .ranked
            .iter()
            .map(|item| item.goal_id.as_str())
            .collect::<Vec<_>>(),
        ["a-baseline"]
    );
    assert!(limited.excluded.contains(&AgendaExclusion {
        goal_id: "z-new-fact".into(),
        reason: AgendaExclusionReason::AlreadyDerived
    }));
    assert!(
        planner
            .reconcile(NOW + 1)
            .unwrap()
            .created_goal_ids
            .is_empty()
    );
    assert_eq!(admin.snapshot().unwrap(), after);
    // Reaching this startup's limit must not retain a stale, executable child.
    save_feedback(&admin, "z-new-fact", NOW + 2);
    let invalidated = planner.reconcile(NOW + 3).unwrap();
    assert_eq!(
        invalidated.invalidated_goal_ids,
        std::slice::from_ref(&child.id)
    );
    assert!(invalidated.created_goal_ids.is_empty());
    assert_eq!(
        admin.snapshot().unwrap().state.goals[&child.id].status,
        GoalStatus::Cancelled
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn reflection_policy_requires_current_parent_scope_status_and_causal_proof() {
    let (kernel, admin) = open().await;
    seed(&admin, vec![parent("parent", 50)]);
    let planner = EndogenousPlanner::new(Arc::new(admin.clone()), options()).unwrap();
    let child_id = planner.reconcile(NOW).unwrap().created_goal_ids.remove(0);
    let original = admin.snapshot().unwrap();
    let child = original.state.goals[&child_id].clone();
    let policy = ReflectionDrivePolicy::new(scope()).unwrap();
    assert_eq!(
        policy
            .rank_with_state(&original, std::slice::from_ref(&child), NOW)
            .unwrap()[0]
            .goal_id,
        child.id
    );
    assert!(policy.rank(std::slice::from_ref(&child), NOW).is_err());
    for mutate in [
        |goal: &mut Goal| goal.revision += 1,
        |goal: &mut Goal| goal.status = GoalStatus::Cancelled,
        |goal: &mut Goal| goal.expires_at_ms = Some(NOW),
        |goal: &mut Goal| goal.source.channel = "untrusted".into(),
        |goal: &mut Goal| goal.visibility = Visibility::User("bob".into()),
    ] {
        let mut snapshot = original.clone();
        mutate(snapshot.state.goals.get_mut("parent").unwrap());
        assert!(
            policy
                .rank_with_state(&snapshot, std::slice::from_ref(&child), NOW)
                .unwrap()
                .is_empty()
        );
    }
    let mut bad_child = original.clone();
    bad_child
        .state
        .goals
        .get_mut(&child_id)
        .unwrap()
        .expires_at_ms = Some(NOW);
    let expired_child = bad_child.state.goals[&child_id].clone();
    assert!(
        policy
            .rank_with_state(&bad_child, &[expired_child], NOW)
            .unwrap()
            .is_empty()
    );
    let mut broken = original.clone();
    broken
        .state
        .events
        .retain(|event| !event.id.starts_with("eve.reflection.input."));
    assert!(
        policy
            .rank_with_state(&broken, std::slice::from_ref(&child), NOW)
            .is_err()
    );
    assert!(evaluate_reflection_agenda(&broken, &options(), 0, NOW).is_err());
    let mut wrong_subject = original.clone();
    wrong_subject.subject_id = "other-eve".into();
    assert_eq!(
        policy.rank_with_state(&wrong_subject, &[child], NOW),
        Err(LoopError::Cognition(CognitionError::SubjectMismatch))
    );
    assert_eq!(admin.snapshot().unwrap(), original);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn completed_or_cancelled_current_reflections_are_not_rederived_and_parent_filters_are_explained()
 {
    let (kernel, admin) = open().await;
    let mut expired = parent("expired", 90);
    expired.expires_at_ms = Some(NOW);
    let mut denied = parent("denied", 90);
    denied.source.channel = "not-allowed".into();
    let mut hidden = parent("invisible", 90);
    hidden.visibility = Visibility::User("bob".into());
    seed(
        &admin,
        vec![
            parent("parent", 50),
            expired,
            denied,
            hidden,
            ready("ready", 90),
        ],
    );
    let child_id = EndogenousPlanner::new(Arc::new(admin.clone()), options())
        .unwrap()
        .reconcile(NOW)
        .unwrap()
        .created_goal_ids
        .remove(0);
    let original = admin.snapshot().unwrap();
    for status in [
        GoalStatus::Ready,
        GoalStatus::Cancelled,
        GoalStatus::Blocked,
        GoalStatus::Completed,
    ] {
        let mut snapshot = original.clone();
        let child = snapshot.state.goals.get_mut(&child_id).unwrap();
        child.status = status;
        if child.status == GoalStatus::Blocked {
            child.block_reason = Some(BlockReason::Interrupted);
        }
        if matches!(child.status, GoalStatus::Completed | GoalStatus::Blocked) {
            child.execution = Some(ExecutionAttempt {
                attempt_id: "attempt".into(),
                session_id: "session".into(),
                task_id: "task".into(),
                turn_id: Some(1),
                started_at_ms: NOW,
            });
        }
        if child.status == GoalStatus::Completed {
            child.feedback = Some(Feedback {
                commit: ExecutionCommit::Completed,
                verification_met: true,
                started_tools: Some(0),
                summary: "Reflection artifact saved; parent remains waiting".into(),
                at_ms: NOW,
            });
        }
        let mut invalid = parent("invalid-budget", 100);
        invalid.budget.max_attempts = 0;
        snapshot.state.goals.insert(invalid.id.clone(), invalid);
        let preview = evaluate_reflection_agenda(&snapshot, &options(), 0, NOW).unwrap();
        assert!(preview.ranked.is_empty());
        for (id, reason) in [
            ("parent", AgendaExclusionReason::AlreadyDerived),
            ("expired", AgendaExclusionReason::Expired),
            ("denied", AgendaExclusionReason::ScopeDenied),
            ("ready", AgendaExclusionReason::NotReady),
            ("invalid-budget", AgendaExclusionReason::InvalidBudget),
        ] {
            assert!(preview.excluded.contains(&AgendaExclusion {
                goal_id: id.into(),
                reason
            }));
        }
        assert!(!format!("{preview:?}").contains("invisible"));
    }
    assert!(
        current_reflection(&original.state, "eve", &original.state.goals["parent"])
            .unwrap()
            .is_some()
    );
    assert!(ReflectionVerifier.supports(&original.state.goals[&child_id]));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn equal_score_order_is_preserved_from_parent_preview_to_hashed_reflection_ids() {
    let (kernel, admin) = open().await;
    seed(
        &admin,
        (0..16)
            .map(|index| parent(&format!("parent-{index:02}"), 50))
            .collect(),
    );
    let options = EndogenousOptions {
        max_derivations: 16,
        ..options()
    };
    let before = admin.snapshot().unwrap();
    let preview = evaluate_reflection_agenda(&before, &options, 0, NOW).unwrap();
    let planner = EndogenousPlanner::new(Arc::new(admin.clone()), options).unwrap();
    let mut ids = Vec::new();
    for _ in 0..16 {
        ids.extend(planner.reconcile(NOW).unwrap().created_goal_ids);
    }
    let snapshot = admin.snapshot().unwrap();
    let inversion = ids
        .windows(2)
        .position(|pair| pair[0] > pair[1])
        .expect("fixture includes parent order opposite to derived SHA-256 id order");
    let pair = vec![
        snapshot.state.goals[&ids[inversion + 1]].clone(),
        snapshot.state.goals[&ids[inversion]].clone(),
    ];
    let ranked = ReflectionDrivePolicy::new(scope())
        .unwrap()
        .rank_with_state(&snapshot, &pair, NOW)
        .unwrap();
    let parents = ranked
        .iter()
        .map(|item| {
            snapshot.state.goals[&item.goal_id]
                .source
                .reference
                .as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parents,
        [
            preview.ranked[inversion].goal_id.as_str(),
            preview.ranked[inversion + 1].goal_id.as_str()
        ]
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicitly_authorized_environment_parent_preserves_age_and_order_in_child_policy() {
    let (kernel, admin) = open().await;
    let mut environment_scope = scope();
    environment_scope.sources = vec![AllowedSource {
        kind: SourceKind::Environment,
        channel: "host-observation".into(),
    }];
    let goals = (0..16)
        .map(|index| {
            let mut goal = parent(&format!("environment-{index:02}"), 50);
            goal.source.kind = SourceKind::Environment;
            goal.source.channel = "host-observation".into();
            goal
        })
        .collect();
    seed(&admin, goals);
    let before_events = admin.snapshot().unwrap();
    let mut state = before_events.state;
    state.events = state
        .goals
        .values()
        .map(|goal| initial_event(goal, 1))
        .collect();
    admin.replace(before_events.revision, state).unwrap();
    let options = EndogenousOptions {
        scope: environment_scope.clone(),
        max_derivations: 16,
        timeout_ms: 1000,
    };
    let now = 43_200_001;
    let before = admin.snapshot().unwrap();
    let preview = evaluate_reflection_agenda(&before, &options, 0, now).unwrap();
    assert!(preview.ranked.iter().all(|item| item.strength == 52));
    let planner = EndogenousPlanner::new(Arc::new(admin.clone()), options).unwrap();
    let mut children = Vec::new();
    for _ in 0..16 {
        children.extend(planner.reconcile(now).unwrap().created_goal_ids);
    }
    let snapshot = admin.snapshot().unwrap();
    let inversion = children
        .windows(2)
        .position(|pair| pair[0] > pair[1])
        .expect("fixture includes an inversion in the SHA-256 child IDs");
    let pair = vec![
        snapshot.state.goals[&children[inversion + 1]].clone(),
        snapshot.state.goals[&children[inversion]].clone(),
    ];
    let policy = ReflectionDrivePolicy::new(environment_scope).unwrap();
    let ranked = policy.rank_with_state(&snapshot, &pair, now).unwrap();
    assert_eq!(
        ranked.iter().map(|item| item.strength).collect::<Vec<_>>(),
        [52, 52]
    );
    assert_eq!(
        ranked
            .iter()
            .map(|item| snapshot.state.goals[&item.goal_id]
                .source
                .reference
                .as_str())
            .collect::<Vec<_>>(),
        [
            preview.ranked[inversion].goal_id.as_str(),
            preview.ranked[inversion + 1].goal_id.as_str()
        ]
    );
    // The normal user-only host does not inherit this separate authorization.
    assert!(
        ReflectionDrivePolicy::new(scope())
            .unwrap()
            .rank_with_state(&snapshot, &pair, now)
            .unwrap()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}
