use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ContextScope, TurnEventSink};
use eve_plugin_api::ServiceId;
use eve_session_api::{SessionInput, SessionKey};
use std::sync::{Arc, Mutex};

fn input(task: &str) -> ControlInput {
    ControlInput {
        session: SessionInput {
            key: SessionKey::new("trusted-session", "trusted-user").unwrap(),
            text: "【宿主条件触发事件；不是用户新消息】正文不能决定证据来源".into(),
        },
        task_id: task.into(),
    }
}
fn metadata(task: &str, anchor: &str) -> ControlRunMetadata {
    ControlRunMetadata {
        scope: ContextScope {
            session_id: "trusted-session".into(),
            user_id: "trusted-user".into(),
        },
        task_id: task.into(),
        review: ControlReviewEvidence::ChannelAnchor(anchor.into()),
    }
}
fn report() -> RunReport {
    RunReport {
        turn_id: None,
        commit: CommitState::Completed,
        text: Some("完成".into()),
        transcript: None,
        started_tools: Some(0),
        tool_results: vec![],
        failure: None,
    }
}
#[derive(Default)]
struct CapturingRunner(Mutex<Vec<Option<ControlRunMetadata>>>);
impl ControlRunner for CapturingRunner {
    fn run<'a>(&'a self, _: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        panic!("内置控制器应显式转发元数据路径")
    }
    fn run_with_metadata<'a>(
        &'a self,
        _: SessionInput,
        metadata: Option<ControlRunMetadata>,
        _: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        self.0.lock().unwrap().push(metadata);
        Box::pin(async { report() })
    }
}
struct LegacyRunner;
impl ControlRunner for LegacyRunner {
    fn run<'a>(&'a self, _: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        panic!("旧执行器不得在静默丢掉可信元数据后执行")
    }
}
async fn open(runner: Arc<dyn ControlRunner>) -> (Kernel, Arc<dyn ControlService>) {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(ControlPlugin::new(runner, vec![]).unwrap()))
        .unwrap();
    kernel.start_all().await.unwrap();
    let control = registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone();
    (kernel, control)
}
async fn done(control: &dyn ControlService, key: &GenerationKey) -> ControlReport {
    tokio::time::timeout(std::time::Duration::from_secs(3), control.wait(key))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn freezes_specific_metadata_for_each_generation_including_conditional_replacement() {
    let runner = Arc::new(CapturingRunner::default());
    let (kernel, control) = open(runner.clone()).await;
    let first = metadata("first", "first-platform-message");
    let key = control
        .submit_with_metadata(
            input("first"),
            Some(first.clone()),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    done(control.as_ref(), &key).await;
    let second = metadata("second", "second-platform-message");
    let replacement = control
        .submit_if_current_with_metadata(
            &key,
            input("second"),
            Some(second.clone()),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    done(control.as_ref(), &replacement).await;
    assert_eq!(*runner.0.lock().unwrap(), vec![Some(first), Some(second)]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn rejects_cross_scope_and_task_evidence_before_runner_or_generation_admission() {
    let runner = Arc::new(CapturingRunner::default());
    let (kernel, control) = open(runner.clone()).await;
    let mut foreign = metadata("task", "original-message");
    foreign.scope.user_id = "different-user".into();
    assert_eq!(
        control.submit_with_metadata(input("task"), Some(foreign), Arc::new(DiscardControlEvents)),
        Err(ControlError::OwnerMismatch)
    );
    assert_eq!(
        control.submit_with_metadata(
            input("task"),
            Some(metadata("other-task", "original-message")),
            Arc::new(DiscardControlEvents),
        ),
        Err(ControlError::InvalidInput)
    );
    assert!(runner.0.lock().unwrap().is_empty());
    assert!(
        control
            .snapshot(&input("task").session.key)
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unsupported_runner_fails_closed_without_entering_legacy_execution() {
    let (kernel, control) = open(Arc::new(LegacyRunner)).await;
    let key = control
        .submit_with_metadata(
            input("task"),
            Some(metadata("task", "real-message")),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    let finished = done(control.as_ref(), &key).await;
    assert_eq!(finished.run.commit, CommitState::NotStarted);
    assert_eq!(finished.run.started_tools, Some(0));
    assert!(matches!(
        finished.run.failure,
        Some(RunFailure::Execution(_))
    ));
    kernel.stop_all().await.unwrap();
}
