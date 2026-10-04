use eve_app::{CognitionOptions, run_cognition, run_cognition_with_planner_factory};
use eve_cognition_api::{CognitionAdmin, CognitionError, GoalStatus, ReadAccess, SourceKind};
use eve_cognition_loop_api::{
    EndogenousOptions, EndogenousPlannerFactory, EndogenousPlanning, EndogenousReport, LoopError,
    LoopResult,
};
use std::{
    ffi::OsString,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn options(directory: &Path, command: &[&str]) -> CognitionOptions {
    let mut args = vec![OsString::from("--state-dir"), directory.as_os_str().into()];
    args.extend(command.iter().map(OsString::from));
    CognitionOptions::parse(args).unwrap().unwrap()
}

async fn seed(directory: &Path) -> Vec<u8> {
    run_cognition(options(
        directory,
        &[
            "add",
            "--id",
            "waiting",
            "--text",
            "尚未解决的事项",
            "--user",
            "alice",
        ],
    ))
    .await
    .unwrap();
    std::fs::read(directory.join("state.json")).unwrap()
}

struct HoldPlanner {
    admin: Arc<dyn CognitionAdmin>,
    ticks: Arc<AtomicUsize>,
}
impl EndogenousPlanning for HoldPlanner {
    fn reconcile(&self, now_ms: u64) -> LoopResult<EndogenousReport> {
        assert!(now_ms > 0);
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(EndogenousReport {
            created_goal_ids: vec![],
            invalidated_goal_ids: vec![],
            revision: self.admin.snapshot()?.revision,
        })
    }
}

struct HoldFactory {
    creates: Arc<AtomicUsize>,
    ticks: Arc<AtomicUsize>,
}
impl EndogenousPlannerFactory for HoldFactory {
    fn create(
        &self,
        admin: Arc<dyn CognitionAdmin>,
        options: EndogenousOptions,
    ) -> LoopResult<Arc<dyn EndogenousPlanning>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        options.validate()?;
        assert_eq!(options.scope.subject_id, "eve");
        assert_eq!(options.scope.access, ReadAccess::Internal);
        assert_eq!(options.scope.sources.len(), 1);
        assert_eq!(options.scope.sources[0].kind, SourceKind::User);
        assert_eq!(options.scope.sources[0].channel, "cognition.cli");
        assert_eq!(options.max_derivations, 2);
        assert_eq!(options.timeout_ms, 30_000);
        let snapshot = admin.snapshot()?;
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.state.goals["waiting"].status, GoalStatus::Waiting);
        assert!(options.scope.permits(&snapshot.state.goals["waiting"]));
        Ok(Arc::new(HoldPlanner {
            admin,
            ticks: self.ticks.clone(),
        }))
    }
}

#[tokio::test]
async fn replacement_planner_controls_run_after_recovery_without_rewriting_state() {
    let directory = tempfile::tempdir().unwrap();
    let before = seed(directory.path()).await;
    let creates = Arc::new(AtomicUsize::new(0));
    let ticks = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(HoldFactory {
        creates: creates.clone(),
        ticks: ticks.clone(),
    });
    let report = run_cognition_with_planner_factory(
        options(
            directory.path(),
            &["run", "--seconds", "1", "--max-executions", "2"],
        ),
        factory,
    )
    .await
    .unwrap();
    assert_eq!(creates.load(Ordering::SeqCst), 1);
    assert!(ticks.load(Ordering::SeqCst) > 0);
    assert_eq!(report["revision"], 1);
    assert_eq!(report["loop"]["model_requests"], 0);
    assert_eq!(report["loop"]["started_tools"], 0);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
    // 再次打开同一目录验证宿主已经停止并释放状态锁。
    run_cognition(options(directory.path(), &["status"]))
        .await
        .unwrap();
}

struct FailingFactory;
impl EndogenousPlannerFactory for FailingFactory {
    fn create(
        &self,
        _: Arc<dyn CognitionAdmin>,
        _: EndogenousOptions,
    ) -> LoopResult<Arc<dyn EndogenousPlanning>> {
        Err(LoopError::Unavailable)
    }
}

#[tokio::test]
async fn factory_is_lazy_and_failure_preserves_state_and_releases_the_directory() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Arc::new(FailingFactory);
    // 这三个命令即使注入的工厂会失败也应正常工作。
    run_cognition_with_planner_factory(
        options(
            directory.path(),
            &["add", "--id", "waiting", "--text", "本地目标"],
        ),
        factory.clone(),
    )
    .await
    .unwrap();
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    for command in [&["status"][..], &["show", "--id", "waiting"][..]] {
        run_cognition_with_planner_factory(options(directory.path(), command), factory.clone())
            .await
            .unwrap();
    }
    let error = run_cognition_with_planner_factory(
        options(directory.path(), &["run", "--seconds", "1"]),
        factory,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LoopError>(),
        Some(&LoopError::Unavailable)
    );
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
    run_cognition(options(directory.path(), &["status"]))
        .await
        .unwrap();
}

struct ConflictPlanner(AtomicUsize);
impl EndogenousPlanning for ConflictPlanner {
    fn reconcile(&self, _: u64) -> LoopResult<EndogenousReport> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(CognitionError::StaleRevision.into())
        } else {
            Err(LoopError::Unavailable)
        }
    }
}
struct ConflictFactory(Arc<ConflictPlanner>);
impl EndogenousPlannerFactory for ConflictFactory {
    fn create(
        &self,
        _: Arc<dyn CognitionAdmin>,
        _: EndogenousOptions,
    ) -> LoopResult<Arc<dyn EndogenousPlanning>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn planner_conflict_waits_for_a_fresh_tick_and_other_errors_stop_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let before = seed(directory.path()).await;
    let planner = Arc::new(ConflictPlanner(AtomicUsize::new(0)));
    let error = run_cognition_with_planner_factory(
        options(directory.path(), &["run", "--seconds", "1"]),
        Arc::new(ConflictFactory(planner.clone())),
    )
    .await
    .unwrap_err();
    assert_eq!(planner.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        error.downcast_ref::<LoopError>(),
        Some(&LoopError::Unavailable)
    );
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
    run_cognition(options(directory.path(), &["status"]))
        .await
        .unwrap();
}
