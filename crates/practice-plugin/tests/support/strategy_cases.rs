use super::*;
use eve_practice_plugin::{StrategyController, StrategyPlugin, strategy_views};

async fn setup(store: Arc<Store>) -> (Kernel, PracticeController, StrategyController) {
    let (kernel, practice) = open(store).await;
    let plugin = StrategyPlugin::new().unwrap();
    let strategy = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(STRATEGY_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    (kernel, practice, strategy)
}

async fn execute_case(
    store: &Arc<Store>,
    practice: &PracticeController,
    strategy: &StrategyController,
    goal: &str,
    brief: &str,
    replies: &[&str],
) -> (PracticeRun, Arc<FakeDrafter>) {
    let mut task = task(goal, 1);
    task.brief = brief.into();
    let run = practice
        .begin(task, &profile(), "fake-drafter:v1", 10)
        .unwrap()
        .unwrap();
    let drafter = Arc::new(FakeDrafter {
        replies: Mutex::new(replies.iter().map(|s| Ok(draft(s))).collect()),
        requests: Mutex::new(vec![]),
        store: store.clone(),
    });
    let runner = Arc::new(FakeRunner {
        profile: profile(),
        runs: Mutex::new(vec![]),
        store: store.clone(),
    });
    let temp = tempfile::tempdir().unwrap();
    let end = Practitioner::new(Arc::new(practice.clone()), drafter.clone(), runner)
        .with_strategy(Arc::new(strategy.clone()))
        .unwrap()
        .practice(&run, temp.path(), || 10)
        .await
        .unwrap();
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    (end, drafter)
}

#[tokio::test]
async fn strategy_needs_two_distinct_verified_goals_and_regression_restores_baseline() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "origin",
        "学习制作红色方块",
        &["warn", "good"],
    )
    .await;
    assert_eq!(
        strategy_views(&strategy.snapshot().unwrap()).unwrap()[0].status,
        StrategyStatus::Candidate
    );
    let (_, first) = execute_case(
        &store,
        &practice,
        &strategy,
        "holdout-one",
        "学习制作蓝色方块",
        &["good"],
    )
    .await;
    let request = first.requests.lock().unwrap()[0].0.clone();
    assert!(request.guidance.is_some());
    assert_eq!(
        request.guidance.as_ref().unwrap().previous_issues,
        vec!["defaulting to Block"]
    );
    assert_eq!(
        strategy_views(&strategy.snapshot().unwrap()).unwrap()[0].status,
        StrategyStatus::Candidate
    );
    let (_, second) = execute_case(
        &store,
        &practice,
        &strategy,
        "holdout-two",
        "学习制作绿色方块",
        &["good"],
    )
    .await;
    assert!(second.requests.lock().unwrap()[0].0.guidance.is_some());
    assert_eq!(
        strategy_views(&strategy.snapshot().unwrap()).unwrap()[0].status,
        StrategyStatus::Active
    );
    execute_case(
        &store,
        &practice,
        &strategy,
        "regressed",
        "学习制作新的门",
        &["warn", "warn", "warn"],
    )
    .await;
    assert_eq!(
        strategy_views(&strategy.snapshot().unwrap()).unwrap()[0].status,
        StrategyStatus::Rejected
    );
    let (_, baseline) = execute_case(
        &store,
        &practice,
        &strategy,
        "baseline",
        "学习制作新的地板",
        &["good"],
    )
    .await;
    assert!(baseline.requests.lock().unwrap()[0].0.guidance.is_none());
    let before = strategy.snapshot().unwrap();
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    assert!(before == strategy.snapshot().unwrap());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn newer_strategy_rolls_back_to_previous_verified_version_on_higher_cost() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "origin",
        "red block",
        &["warn", "good"],
    )
    .await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "first",
        "blue block",
        &["good"],
    )
    .await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "second",
        "green block",
        &["good"],
    )
    .await;
    // 与来源同题，不使用旧策略；这个独立修正来源产生候选第二版。
    execute_case(
        &store,
        &practice,
        &strategy,
        "new-origin",
        "red block",
        &["warn", "good"],
    )
    .await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "third",
        "yellow block",
        &["good"],
    )
    .await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "fourth",
        "white block",
        &["good"],
    )
    .await;
    let views = strategy_views(&strategy.snapshot().unwrap()).unwrap();
    assert_eq!(
        (views[0].status, views[1].status),
        (StrategyStatus::Retired, StrategyStatus::Active)
    );
    // 第二版验证成本为一次，后续即使成功但用了两次，也保守回退。
    execute_case(
        &store,
        &practice,
        &strategy,
        "expensive",
        "gray block",
        &["warn", "good"],
    )
    .await;
    let views = strategy_views(&strategy.snapshot().unwrap()).unwrap();
    assert_eq!(
        (views[0].status, views[1].status),
        (StrategyStatus::Active, StrategyStatus::Rejected)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn same_question_goal_owner_runtime_and_drafter_are_not_independent_validation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    let (origin, _) = execute_case(
        &store,
        &practice,
        &strategy,
        "origin",
        "Red block",
        &["warn", "good"],
    )
    .await;
    let mut next = practice
        .begin(task("different", 1), &profile(), "fake-drafter:v1", 10)
        .unwrap()
        .unwrap();
    next.task.brief = " rEd  block \n".into();
    assert!(strategy.select(&next).unwrap().is_none());
    next.task.brief = "new task".into();
    next.task.owner = "user-b".into();
    assert!(strategy.select(&next).unwrap().is_none());
    next.task.owner = origin.task.owner.clone();
    next.runner.runtime = "another version".into();
    assert!(strategy.select(&next).unwrap().is_none());
    next.runner = profile();
    next.drafter_version = "different-drafter:v1".into();
    assert!(strategy.select(&next).unwrap().is_none());
    next.drafter_version = "fake-drafter:v1".into();
    next.task.goal_id = "origin".into();
    next.task.goal_revision = 2;
    next.id = run_id("origin", 2);
    assert!(strategy.select(&next).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancelled_and_crashed_selected_runs_remain_inconclusive_and_do_not_replay() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "origin",
        "red block",
        &["warn", "good"],
    )
    .await;
    let selected = practice
        .begin(task("cancelled", 1), &profile(), "fake-drafter:v1", 10)
        .unwrap()
        .unwrap();
    assert!(strategy.select(&selected).unwrap().is_some());
    practice
        .abandon(&selected.id, 10, PracticeFailure::Cancelled)
        .unwrap();
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    let mut task = task("crashed", 1);
    task.brief = "another task".into();
    let selected = practice
        .begin(task, &profile(), "fake-drafter:v1", 10)
        .unwrap()
        .unwrap();
    assert!(strategy.select(&selected).unwrap().is_some());
    kernel.stop_all().await.unwrap();
    assert!(strategy.snapshot().is_err());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    let snapshot = strategy.snapshot().unwrap();
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|e| matches!(
                e,
                StrategyEvent::Observed {
                    effect: StrategyEffect::Inconclusive,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        strategy_views(&snapshot).unwrap()[0].status,
        StrategyStatus::Candidate
    );
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    assert!(snapshot == strategy.snapshot().unwrap());
    assert!(strategy.select(&selected).is_err());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn selection_commit_failure_closes_handle_and_reopens_actual_before_or_after_commit_state() {
    for after in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path());
        let (kernel, practice, strategy) = setup(store.clone()).await;
        execute_case(
            &store,
            &practice,
            &strategy,
            "origin",
            "red block",
            &["warn", "good"],
        )
        .await;
        let run = practice
            .begin(task("selected", 1), &profile(), "fake-drafter:v1", 10)
            .unwrap()
            .unwrap();
        if after {
            store.fail_after.store(true, Ordering::SeqCst);
        } else {
            store.fail_before.store(true, Ordering::SeqCst);
        }
        assert_eq!(strategy.select(&run).err(), Some(PracticeError::Storage));
        assert!(strategy.snapshot().is_err());
        store.fail_before.store(false, Ordering::SeqCst);
        store.fail_after.store(false, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
        let (kernel, practice, strategy) = setup(store.clone()).await;
        strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
        assert_eq!(
            strategy
                .snapshot()
                .unwrap()
                .events
                .iter()
                .filter(|e| matches!(e, StrategyEvent::Selected(_)))
                .count(),
            usize::from(after)
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_future_or_duplicate_key_strategy_state_is_preserved_and_refused() {
    for bytes in [br#"{"format_version":99,"snapshot":{"events":[]}}"#.to_vec(),br#"{"format_version":1,"format_version":1,"snapshot":{"events":[]}}"#.to_vec(),br#"{"format_version":1,"snapshot":{"events":[{"Observed":{"run_id":"fake","evidence_sha256":"aaaa","effect":"Failed"}}]}}"#.to_vec()] {
        let temp=tempfile::tempdir().unwrap();let store=Store::open(temp.path());let id=PluginId::new(STRATEGY_PLUGIN_ID).unwrap();
        store.set(&id,STRATEGY_STATE_KEY.into(),bytes.clone()).unwrap();
        let kernel=Kernel::with_services(KernelServices {state:store.clone(),..KernelServices::default()});
        let plugin=StrategyPlugin::new().unwrap();kernel.register(Box::new(plugin)).unwrap();
        assert!(kernel.start_all().await.is_err());
        assert_eq!(store.get(&id,STRATEGY_STATE_KEY).unwrap(),Some(bytes));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn changed_or_missing_source_cannot_keep_supporting_a_strategy() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "origin",
        "red block",
        &["warn", "good"],
    )
    .await;
    let mut snapshot = practice.snapshot().unwrap();
    snapshot.runs[0].task.brief = "tampered".into();
    assert_eq!(
        strategy.reconcile(&snapshot),
        Err(PracticeError::CorruptState)
    );
    assert_eq!(
        strategy.reconcile(&PracticeSnapshot { runs: vec![] }),
        Err(PracticeError::CorruptState)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn model_provider_failure_and_runtime_timeout_do_not_reject_the_strategy() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path());
    let (kernel, practice, strategy) = setup(store.clone()).await;
    execute_case(
        &store,
        &practice,
        &strategy,
        "origin",
        "red block",
        &["warn", "good"],
    )
    .await;
    let run = practice
        .begin(
            task("provider-failed", 1),
            &profile(),
            "fake-drafter:v1",
            10,
        )
        .unwrap()
        .unwrap();
    assert!(strategy.select(&run).unwrap().is_some());
    practice
        .record_draft(&run.id, 10, Err(PracticeFailure::Provider), vec![])
        .unwrap();
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    let mut task = task("runtime-timeout", 1);
    task.brief = "timeout independent".into();
    let run = practice
        .begin(task, &profile(), "fake-drafter:v1", 10)
        .unwrap()
        .unwrap();
    assert!(strategy.select(&run).unwrap().is_some());
    for _ in 0..MAX_ATTEMPTS {
        let draft = draft("good");
        practice
            .record_draft(&run.id, 10, Ok(draft.clone()), vec![])
            .unwrap();
        let mut evidence = evidence(&draft, false);
        evidence.exit = RunExit::Timeout;
        practice.record_evidence(&run.id, 10, evidence).unwrap();
    }
    strategy.reconcile(&practice.snapshot().unwrap()).unwrap();
    let snapshot = strategy.snapshot().unwrap();
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|e| matches!(
                e,
                StrategyEvent::Observed {
                    effect: StrategyEffect::Inconclusive,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        strategy_views(&snapshot).unwrap()[0].status,
        StrategyStatus::Candidate
    );
    kernel.stop_all().await.unwrap();
}
