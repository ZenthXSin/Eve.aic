use super::*;
use eve_kernel::backends::FileStateStore;
use std::path::Path;

async fn open(root: &Path) -> Fixture {
    let memory =
        Arc::new(eve_subject_sqlite::SqliteIdleMemory::open(&root.join("mind.sqlite3")).unwrap());
    Fixture::with_services(
        Some(memory),
        KernelServices {
            state: Arc::new(FileStateStore::open(root.join("state")).unwrap()),
            ..KernelServices::default()
        },
    )
    .await
}

fn focus(f: &Fixture, scope: &SubjectScope, id: &str) -> SubjectFocus {
    f.subject
        .snapshot_for(scope)
        .unwrap()
        .foci
        .into_iter()
        .find(|focus| focus.id == id)
        .unwrap()
}

async fn draft(
    f: &Fixture,
    continuous: bool,
    max_goals: u32,
    revisit_after_minutes: u32,
) -> (SubjectScope, SubjectFocus, Task, u64) {
    f.config()
        .execute(
            call(
                "configure_subject_autonomy",
                json!({
                    "action":"configure_autonomy", "enabled":true,
                    "continuous":continuous, "idle_reflection":true,
                    "max_goals":max_goals, "steps_per_goal":10
                }),
            ),
            authorized("a"),
        )
        .await
        .unwrap();
    let scope = scope(&trusted("a", "owner"));
    let at = focus(f, &scope, POLICY_ID).updated_at_ms + AUTONOMY_INTERVAL_MS;
    let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
    let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
    let first = plan_next(&subject, &tasks, &scope, at).unwrap().unwrap();
    dispatch_focus(
        &subject,
        &tasks,
        &(Arc::new(Owner) as Arc<dyn AutonomyOwner>),
        first,
        at,
    )
    .unwrap();
    let review = first_idle(f);
    f.finish(
        &review.id,
        TaskEnd::Completed {
            result: json!({
                "choice":"explore", "thought":"想随便看看新东西",
                "reason":"好奇", "judgments":[],
                "revisit_after_minutes":revisit_after_minutes
            })
            .to_string(),
        },
        at + 1,
    );
    reconcile_tasks(&subject, &tasks, at + 2).unwrap();
    let draft = plan_next(&subject, &tasks, &scope, at + 3)
        .unwrap()
        .unwrap();
    assert!(draft.goal.starts_with(crate::qq_exploration::INPUT_PREFIX));
    assert!(draft.task_id.is_none());
    assert_eq!(draft.id, "auto-1-1");
    (scope, draft, review, at + 3)
}

fn ingress(f: &Fixture, scope: &SubjectScope, priority: EventPriority, at: u64) -> SubjectEvent {
    f.subject
        .observe_completed(
            SubjectEventInput {
                scope: scope.clone(),
                source: EventSource::UserIngress,
                dedupe_key: format!("new-message-{at}"),
                priority,
                kind: "message".into(),
                payload: "新的用户输入".into(),
            },
            at,
        )
        .unwrap()
}

fn dispatch(f: &Fixture, focus: SubjectFocus, at: u64) -> SubjectResult<SubjectDispatch> {
    dispatch_focus(
        &(f.subject.clone() as Arc<dyn SubjectAdmin>),
        &(f.tasks.clone() as Arc<dyn TaskAdmin>),
        &(Arc::new(Owner) as Arc<dyn AutonomyOwner>),
        focus,
        at,
    )
}

#[tokio::test]
async fn newer_input_withdraws_draft_then_restart_reflects_without_replaying_or_regranting() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, review, at) = draft(&f, false, 4, 120).await;
    let input = ingress(&f, &scope, EventPriority::User, at + 1);
    assert!(matches!(
        dispatch(&f, draft.clone(), at + 2).unwrap(),
        SubjectDispatch::Cancelled
    ));
    let cancelled = focus(&f, &scope, &draft.id);
    let saved_input =
        crate::qq_exploration::ExplorationInput::decode(&cancelled.goal, &scope).unwrap();
    assert_eq!(cancelled.status, FocusStatus::Cancelled);
    assert!(cancelled.task_id.is_none());
    assert_eq!(
        saved_input.superseded_by_input,
        Some(crate::qq_exploration::SupersedingInput {
            event_id: input.id,
            created_at_ms: at + 1,
            reflection_created_at_ms: review.created_at_ms,
        })
    );
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 1);
    let policy = focus(&f, &scope, POLICY_ID);
    let grant: Grant = serde_json::from_str(&policy.goal).unwrap();
    assert_eq!(grant.issued, 2);
    let next_at = f
        .subject
        .idle_memory()
        .unwrap()
        .snapshot(&scope)
        .unwrap()
        .next_at_ms;
    assert!(next_at > at + AUTONOMY_INTERVAL_MS);
    f.kernel.stop_all().await.unwrap();
    drop(f);

    let f = open(root.path()).await;
    let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
    let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
    assert!(focus(&f, &scope, &draft.id) == cancelled);
    for now in [at + 3, at + AUTONOMY_INTERVAL_MS + 2, next_at - 1] {
        assert!(plan_next(&subject, &tasks, &scope, now).unwrap().is_none());
        assert_eq!(focus(&f, &scope, POLICY_ID).goal, policy.goal);
    }
    let next = plan_next(&subject, &tasks, &scope, next_at)
        .unwrap()
        .unwrap();
    assert_eq!(next.id, "auto-1-2");
    assert!(next.goal.starts_with(IDLE_INPUT_PREFIX));
    assert!(matches!(
        dispatch(&f, cancelled.clone(), next_at).unwrap(),
        SubjectDispatch::Cancelled
    ));
    dispatch(&f, next.clone(), next_at + 1).unwrap();
    for n in 2..6 {
        assert!(
            plan_next(&subject, &tasks, &scope, next_at + n)
                .unwrap()
                .is_none()
        );
    }
    let (_, after) = grant_for(&subject, &scope).unwrap().unwrap();
    assert_eq!(after.issued, 3);
    assert_eq!(after.generation, grant.generation);
    assert_eq!(after.evidence_digest, grant.evidence_digest);
    assert_eq!(after.configured_turn_id, grant.configured_turn_id);
    assert_eq!(after.expires_at_ms, grant.expires_at_ms);
    assert_eq!(after.steps_per_goal, grant.steps_per_goal);
    assert_eq!(
        f.subject
            .idle_memory()
            .unwrap()
            .exploration_source(&scope, &review.id)
            .unwrap(),
        Some(draft.id)
    );
    let all = f.tasks.snapshot().unwrap();
    assert_eq!(all.tasks.len(), 2);
    assert!(all.tasks.iter().all(|task| !task.is_exploration()));
    assert!(all.tasks.iter().all(|task| task.notice.is_none()));
    assert!(focus(&f, &scope, &cancelled.id) == cancelled);
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn scheduled_check_does_not_hide_input_or_adopt_a_candidate_before_new_reflection() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, _, at) = draft(&f, false, 4, 30).await;
    ingress(&f, &scope, EventPriority::User, at + 1);
    f.subject.schedule_due(at + AUTONOMY_INTERVAL_MS).unwrap();
    let scheduled = focus(&f, &scope, &draft.id);
    assert_eq!(scheduled.updated_at_ms, at + AUTONOMY_INTERVAL_MS);
    assert!(matches!(
        dispatch(&f, scheduled, at + AUTONOMY_INTERVAL_MS).unwrap(),
        SubjectDispatch::Cancelled
    ));
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 1);
    f.subject
        .upsert_focus(
            SubjectFocus {
                id: "candidate-before-new-message".into(),
                scope: scope.clone(),
                title: "先前的候选".into(),
                goal: "候选只供这次重新回顾参考".into(),
                task_id: None,
                status: FocusStatus::Blocked,
                next_check_at_ms: at,
                updated_at_ms: at,
            },
            at + AUTONOMY_INTERVAL_MS + 1,
        )
        .unwrap();
    let next = plan_next(
        &(f.subject.clone() as Arc<dyn SubjectAdmin>),
        &(f.tasks.clone() as Arc<dyn TaskAdmin>),
        &scope,
        at + AUTONOMY_INTERVAL_MS + 2,
    )
    .unwrap()
    .unwrap();
    assert!(next.goal.starts_with(IDLE_INPUT_PREFIX));
    assert_eq!(
        focus(&f, &scope, "candidate-before-new-message").status,
        FocusStatus::Blocked
    );
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn frequent_input_waits_and_continuous_window_keeps_the_original_budget() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, _, at) = draft(&f, true, 2, 30).await;
    ingress(&f, &scope, EventPriority::User, at + 1);
    dispatch(&f, draft, at + 2).unwrap();
    let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
    let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
    let (_, initial) = grant_for(&subject, &scope).unwrap().unwrap();
    let window_end = initial.window.as_ref().unwrap().next_at_ms().unwrap();
    for now in [at + 3, at + AUTONOMY_INTERVAL_MS + 2, window_end - 1] {
        assert!(plan_next(&subject, &tasks, &scope, now).unwrap().is_none());
        let (_, grant) = grant_for(&subject, &scope).unwrap().unwrap();
        assert_eq!(grant.issued, 2);
        assert_eq!(grant.window.as_ref().unwrap().used, 2);
    }
    ingress(&f, &scope, EventPriority::User, window_end);
    assert!(
        plan_next(&subject, &tasks, &scope, window_end + 1)
            .unwrap()
            .is_none()
    );
    ingress(
        &f,
        &scope,
        EventPriority::User,
        window_end + AUTONOMY_INTERVAL_MS - 1,
    );
    let quiet_at = window_end + 2 * AUTONOMY_INTERVAL_MS - 1;
    assert!(
        plan_next(&subject, &tasks, &scope, quiet_at - 1)
            .unwrap()
            .is_none()
    );
    let next = plan_next(&subject, &tasks, &scope, quiet_at)
        .unwrap()
        .unwrap();
    assert_eq!(next.id, "auto-1-2");
    assert!(next.goal.starts_with(IDLE_INPUT_PREFIX));
    let (_, after) = grant_for(&subject, &scope).unwrap().unwrap();
    assert_eq!(after.issued, 3);
    assert_eq!(after.window.as_ref().unwrap().used, 1);
    assert_eq!(after.window.as_ref().unwrap().started_at_ms, window_end);
    assert_eq!(after.generation, initial.generation);
    assert_eq!(after.evidence_digest, initial.evidence_digest);
    assert_eq!(after.configured_turn_id, initial.configured_turn_id);
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 1);
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicit_stop_pause_revoke_expiry_and_exhaustion_never_resume() {
    for stop in [
        "stop-input",
        "later-stop-input",
        "cancelled",
        "paused",
        "revoked",
        "expired",
        "exhausted",
    ] {
        let root = tempfile::tempdir().unwrap();
        let f = open(root.path()).await;
        let (scope, mut draft, _, at) = draft(&f, false, 4, 30).await;
        let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
        let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
        if stop == "cancelled" {
            draft.status = FocusStatus::Cancelled;
            f.subject.upsert_focus(draft, at + 1).unwrap();
        } else {
            let priority = if stop == "stop-input" {
                EventPriority::Stop
            } else {
                EventPriority::User
            };
            ingress(&f, &scope, priority, at + 1);
            dispatch(&f, draft, at + 2).unwrap();
        }
        if stop == "later-stop-input" {
            ingress(&f, &scope, EventPriority::Stop, at + 3);
        }
        let mut policy = focus(&f, &scope, POLICY_ID);
        let mut grant: Grant = serde_json::from_str(&policy.goal).unwrap();
        match stop {
            "paused" => policy.status = FocusStatus::Cancelled,
            "expired" => grant.expires_at_ms = at + 3,
            "exhausted" => grant.max_goals = 2,
            _ => {}
        }
        policy.goal = serde_json::to_string(&grant).unwrap();
        f.subject.upsert_focus(policy, at + 3).unwrap();
        if stop == "revoked" {
            reconcile_autonomy(
                &subject,
                &tasks,
                &(Arc::new(FormerOwner) as Arc<dyn AutonomyOwner>),
                &scope,
                at + 4,
            )
            .unwrap();
        }
        let now = at + 2 * AUTONOMY_INTERVAL_MS;
        reconcile_autonomy(
            &subject,
            &tasks,
            &(Arc::new(Owner) as Arc<dyn AutonomyOwner>),
            &scope,
            now,
        )
        .unwrap();
        assert!(
            plan_next(&subject, &tasks, &scope, now).unwrap().is_none(),
            "{stop}"
        );
        assert!(grant_for(&subject, &scope).unwrap().is_none(), "{stop}");
        assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 1, "{stop}");
        let final_grant: Grant = serde_json::from_str(&focus(&f, &scope, POLICY_ID).goal).unwrap();
        assert_eq!(final_grant.issued, 2, "{stop}");
        f.kernel.stop_all().await.unwrap();
    }
}

fn save_task_before_focus(f: &Fixture, draft: &SubjectFocus, changed: bool, at: u64) -> Task {
    let mut input =
        crate::qq_exploration::ExplorationInput::decode(&draft.goal, &draft.scope).unwrap();
    if changed {
        input.thought = "另一份冻结内容".into();
    }
    let owner = TaskOwner {
        session_id: draft.scope.conversation_id.clone(),
        user_id: draft.scope.principal_id.clone(),
    };
    let (_, grant) = grant_for(&(f.subject.clone() as Arc<dyn SubjectAdmin>), &draft.scope)
        .unwrap()
        .unwrap();
    f.tasks
        .create_exploration(
            &owner,
            TaskSpec {
                title: "探索会话".into(),
                instructions: crate::qq_exploration::instructions(&input).unwrap(),
                max_steps: grant.steps_per_goal,
                context: Some(
                    crate::qq_exploration::context(&input, vec![], owner.clone()).unwrap(),
                ),
            },
            &exploration_task_source(draft),
            grant.review_messages,
            grant.window.is_none().then_some(grant.expires_at_ms),
            at,
        )
        .unwrap()
}

#[tokio::test]
async fn half_committed_task_is_recovered_without_replay_and_mismatched_input_is_rejected() {
    for changed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let f = open(root.path()).await;
        let (scope, draft, _, at) = draft(&f, false, 4, 30).await;
        let task = save_task_before_focus(&f, &draft, changed, at + 1);
        ingress(&f, &scope, EventPriority::User, at + 2);
        let result = dispatch(&f, draft.clone(), at + 3);
        if changed {
            assert!(matches!(result, Err(SubjectError::CorruptState)));
            assert!(focus(&f, &scope, &draft.id).task_id.is_none());
            assert!(matches!(
                reconcile_tasks(
                    &(f.subject.clone() as Arc<dyn SubjectAdmin>),
                    &(f.tasks.clone() as Arc<dyn TaskAdmin>),
                    at + 4
                ),
                Err(SubjectError::CorruptState)
            ));
            assert!(focus(&f, &scope, &draft.id).task_id.is_none());
        } else {
            assert!(matches!(result.unwrap(), SubjectDispatch::Completed { .. }));
            let saved = focus(&f, &scope, &draft.id);
            assert_eq!(saved.task_id.as_deref(), Some(task.id.as_str()));
            assert_eq!(saved.status, FocusStatus::Active);
            assert_eq!(saved.goal, draft.goal);
            assert_eq!(
                saved_exploration_task(&(f.tasks.clone() as Arc<dyn TaskAdmin>), &saved).unwrap(),
                Some(task.clone())
            );
        }
        assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 2);
        assert_eq!(
            f.tasks.get_owned(&task.owner, &task.id).unwrap(),
            Some(task)
        );
        f.kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_executed_exploration_is_never_mistaken_for_an_undispatched_draft() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, _, at) = draft(&f, false, 4, 30).await;
    dispatch(&f, draft.clone(), at + 1).unwrap();
    let saved = focus(&f, &scope, &draft.id);
    let task_id = saved.task_id.unwrap();
    f.tasks.cancel("owner", &task_id, at + 2).unwrap();
    ingress(&f, &scope, EventPriority::User, at + 3);
    let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
    let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
    reconcile_tasks(&subject, &tasks, at + 4).unwrap();
    let cancelled = focus(&f, &scope, &draft.id);
    assert_eq!(cancelled.status, FocusStatus::Cancelled);
    assert_eq!(
        superseded_exploration_input(&subject, &tasks, &cancelled).unwrap(),
        None
    );
    assert!(
        plan_next(&subject, &tasks, &scope, at + 2 * AUTONOMY_INTERVAL_MS)
            .unwrap()
            .is_none()
    );
    assert!(grant_for(&subject, &scope).unwrap().is_none());
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 2);
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicit_cancel_clears_host_withdrawal_reason_and_tools_cannot_forge_it() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let scope = scope(&trusted("a", "owner"));
    let at = crate::qq_tasks::now_ms() - 100;
    let event = ingress(&f, &scope, EventPriority::User, at);
    let input = crate::qq_exploration::ExplorationInput {
        scope: scope.clone(),
        reflection_id: "review-one".into(),
        exploration: None,
        thought: "先前的好奇".into(),
        superseded_by_input: Some(crate::qq_exploration::SupersedingInput {
            event_id: event.id,
            created_at_ms: at,
            reflection_created_at_ms: at - 1,
        }),
    };
    let focus = SubjectFocus {
        id: "auto-1-1".into(),
        scope: scope.clone(),
        title: "探索会话".into(),
        goal: input.encode().unwrap(),
        task_id: None,
        status: FocusStatus::Cancelled,
        next_check_at_ms: at,
        updated_at_ms: at,
    };
    f.subject.upsert_focus(focus.clone(), at).unwrap();
    let tool = FocusTool(f.subject.clone());
    for action in ["save", "discover"] {
        assert!(
            tool.validate_arguments(&json!({
                "action":action, "id":"forged", "title":"伪造宿主记录",
                "goal":focus.goal, "completion_contract":file_contract()
            }))
            .is_err()
        );
    }
    assert!(
        tool.validate_arguments(&json!({
            "action":"cancel", "id":focus.id,
            "superseded_by_input":input.superseded_by_input
        }))
        .is_err()
    );
    let result = tool
        .execute(
            call(
                "manage_subject_focus",
                json!({"action":"cancel","id":focus.id}),
            ),
            authorized("a"),
        )
        .await
        .unwrap();
    let cancelled: SubjectFocus = serde_json::from_value(result["focus"].clone()).unwrap();
    assert_eq!(cancelled.status, FocusStatus::Cancelled);
    assert!(
        crate::qq_exploration::ExplorationInput::decode(&cancelled.goal, &scope)
            .unwrap()
            .superseded_by_input
            .is_none()
    );
    // 工具持有主体和 SQLite 句柄；重启前与原宿主一起释放。
    drop(tool);
    f.kernel.stop_all().await.unwrap();
    drop(f);
    let f = open(root.path()).await;
    let saved = f.subject.snapshot_for(&scope).unwrap().foci.remove(0);
    assert_eq!(saved, cancelled);
    assert!(
        superseded_exploration_input(
            &(f.subject.clone() as Arc<dyn SubjectAdmin>),
            &(f.tasks.clone() as Arc<dyn TaskAdmin>),
            &saved
        )
        .unwrap()
        .is_none()
    );
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn foreign_input_does_not_withdraw_the_original_scoped_exploration() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, _, at) = draft(&f, false, 4, 30).await;
    for foreign in [
        super::scope(&trusted("another", "owner")),
        super::scope(&trusted("a", "other")),
    ] {
        ingress(&f, &foreign, EventPriority::User, at + 1);
    }
    assert!(matches!(
        dispatch(&f, draft.clone(), at + 2).unwrap(),
        SubjectDispatch::Completed { .. }
    ));
    let saved = focus(&f, &scope, &draft.id);
    assert_eq!(saved.status, FocusStatus::Active);
    assert!(saved.task_id.is_some());
    assert_eq!(saved.goal, draft.goal);
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 2);
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn withdrawal_cannot_resume_with_a_mismatched_source_event_or_existing_task() {
    for corrupt in ["source", "event", "task", "task-id"] {
        let root = tempfile::tempdir().unwrap();
        let f = open(root.path()).await;
        let (scope, draft, _, at) = draft(&f, false, 4, 30).await;
        ingress(&f, &scope, EventPriority::User, at + 1);
        dispatch(&f, draft.clone(), at + 2).unwrap();
        let mut cancelled = focus(&f, &scope, &draft.id);
        let mut input =
            crate::qq_exploration::ExplorationInput::decode(&cancelled.goal, &scope).unwrap();
        match corrupt {
            "source" => input.reflection_id = "another-reflection".into(),
            "event" => input.superseded_by_input.as_mut().unwrap().created_at_ms += 1,
            "task" => {
                save_task_before_focus(&f, &draft, false, at + 3);
            }
            "task-id" => cancelled.task_id = Some("task-unconfirmed".into()),
            _ => unreachable!(),
        }
        cancelled.goal = input.encode().unwrap();
        f.subject.upsert_focus(cancelled, at + 4).unwrap();
        let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
        let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
        let before = focus(&f, &scope, POLICY_ID).goal;
        let result = plan_next(&subject, &tasks, &scope, at + 2 * AUTONOMY_INTERVAL_MS);
        if matches!(corrupt, "source" | "event") {
            assert!(
                matches!(result, Err(SubjectError::CorruptState)),
                "{corrupt}"
            );
            assert_eq!(focus(&f, &scope, POLICY_ID).goal, before);
        } else {
            assert!(result.unwrap().is_none(), "{corrupt}");
            assert!(grant_for(&subject, &scope).unwrap().is_none(), "{corrupt}");
        }
        let grant: Grant = serde_json::from_str(&focus(&f, &scope, POLICY_ID).goal).unwrap();
        assert_eq!(grant.issued, 2);
        f.kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn in_flight_effect_at_restart_is_retained_and_never_enters_draft_resumption() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, _, at) = draft(&f, false, 4, 30).await;
    dispatch(&f, draft.clone(), at + 1).unwrap();
    let task_id = focus(&f, &scope, &draft.id).task_id.unwrap();
    let task = f.tasks.snapshot().unwrap().get(&task_id).unwrap().clone();
    f.tasks.start(&task_id, at + 2).unwrap();
    let mut checkpoint = frame(&task);
    checkpoint.pending_calls.push(ToolCall {
        id: "unconfirmed-effect".into(),
        name: "run_command".into(),
        arguments: json!({"command":"record-once"}),
    });
    checkpoint.used_tool_calls = 1;
    checkpoint.tool_batches = 1;
    let deadline = task
        .execution
        .as_ref()
        .unwrap()
        .authorization_deadline_at_ms;
    f.tasks
        .checkpoint_with_deadline(&task_id, checkpoint.clone(), deadline, at + 3)
        .unwrap();
    ingress(&f, &scope, EventPriority::User, at + 4);
    f.kernel.stop_all().await.unwrap();
    drop(f);

    let f = open(root.path()).await;
    let subject = f.subject.clone() as Arc<dyn SubjectAdmin>;
    let tasks = f.tasks.clone() as Arc<dyn TaskAdmin>;
    reconcile_tasks(&subject, &tasks, at + 5).unwrap();
    assert!(
        plan_next(&subject, &tasks, &scope, at + 2 * AUTONOMY_INTERVAL_MS)
            .unwrap()
            .is_none()
    );
    assert!(grant_for(&subject, &scope).unwrap().is_none());
    let retained = f.tasks.get_owned(&task.owner, &task_id).unwrap().unwrap();
    assert_eq!(retained.status, TaskStatus::Interrupted);
    assert_eq!(
        retained.execution.as_ref().unwrap().checkpoint.as_ref(),
        Some(&checkpoint)
    );
    assert!(retained.notice.is_none());
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 2);
    f.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_tick_keeps_user_admission_first_and_later_starts_only_a_new_reflection() {
    let root = tempfile::tempdir().unwrap();
    let f = open(root.path()).await;
    let (scope, draft, _, at) = draft(&f, true, 4, 30).await;
    f.subject
        .enqueue(
            SubjectEventInput {
                scope: scope.clone(),
                source: EventSource::AutonomyScheduler,
                dedupe_key: format!("idle-ready:{}", draft.id),
                priority: EventPriority::Autonomy,
                kind: "idle_ready".into(),
                payload: draft.id.clone(),
            },
            at,
        )
        .unwrap();
    let user = f
        .subject
        .enqueue(
            SubjectEventInput {
                scope: scope.clone(),
                source: EventSource::UserIngress,
                dedupe_key: "foreground-admission".into(),
                priority: EventPriority::User,
                kind: "work_admission".into(),
                payload: "正常新消息优先".into(),
            },
            at + 1,
        )
        .unwrap();
    f.tick(at + 2);
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 1);
    assert_eq!(focus(&f, &scope, &draft.id).status, FocusStatus::Active);
    let admitted = f
        .subject
        .claim_next("foreground", at + 3, 1000)
        .unwrap()
        .unwrap();
    assert_eq!(admitted.id, user.id);
    f.subject
        .settle(
            &admitted.lease.unwrap().token,
            SubjectOutcome::Completed("已由前台处理".into()),
            at + 4,
        )
        .unwrap();
    pump(&f, at + 5);
    assert_eq!(focus(&f, &scope, &draft.id).status, FocusStatus::Cancelled);
    assert_eq!(f.tasks.snapshot().unwrap().tasks.len(), 1);
    pump(&f, at + AUTONOMY_INTERVAL_MS + 10);
    let tasks = f.tasks.snapshot().unwrap();
    assert_eq!(tasks.tasks.len(), 2);
    assert!(
        tasks
            .tasks
            .iter()
            .all(|task| task.source.starts_with(IDLE_TASK_SOURCE_PREFIX))
    );
    assert!(tasks.tasks.iter().all(|task| task.notice.is_none()));
    let (_, grant) = grant_for(&(f.subject.clone() as Arc<dyn SubjectAdmin>), &scope)
        .unwrap()
        .unwrap();
    assert_eq!(grant.issued, 3);
    f.kernel.stop_all().await.unwrap();
}
