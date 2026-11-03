use super::*;
use eve_control_api::{
    CancelDisposition, ControlError, ControlEvent, ControlEventSink, ControlFuture, ControlReport,
    ControlResult, ControlRunner, ControlServiceHandle, GenerationKey, RunFuture, RunReport,
};
use eve_qqbot_plugin::{QqAdmission, QqAdmissionResult};
use eve_subject_api::{EventPriority, EventSource, EventStatus, SubjectAdmin, SubjectScope};

struct NoSubmission;
impl ControlService for NoSubmission {
    fn submit(
        &self,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        panic!("主体准入繁忙时不得启动模型执行")
    }
    fn submit_if_current(
        &self,
        _: &GenerationKey,
        _: ControlInput,
        _: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        panic!("主体准入繁忙时不得替代已有执行")
    }
    fn cancel(&self, _: &GenerationKey) -> ControlResult<CancelDisposition> {
        Err(ControlError::Unavailable)
    }
    fn wait(&self, _: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        Box::pin(async { Err(ControlError::Unavailable) })
    }
    fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        Ok(None)
    }
    fn accepts(&self, _: &ControlEvent) -> bool {
        false
    }
}

#[tokio::test]
async fn busy_web_admission_removes_its_queued_event_so_background_work_can_continue() {
    let root = tempfile::tempdir().unwrap();
    let kernel = eve_kernel::Kernel::with_services(eve_kernel::KernelServices::default());
    let plugin = eve_subject_plugin::SubjectPlugin::new().unwrap();
    let subject = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let admission = Arc::new(crate::qq_subject_admission::Admission(subject.clone()));

    // 模拟后台同步受理仍持有全局租约，Web 在该窗口收到真实新请求。
    let running = key(&format!("{:032x}", 1)).unwrap();
    let ticket = admission
        .begin(&running, "already-admitting")
        .unwrap()
        .unwrap();
    let chat = Arc::new(
        WebChat::open(Arc::new(NoSubmission), root.path())
            .unwrap()
            .with_admission(admission.clone()),
    );
    let conversation_id = format!("{:032x}", 2);
    let request_id = format!("{:032x}", 3);
    let rejected = chat
        .submit(ChatSubmit {
            conversation_id: conversation_id.clone(),
            request_id: request_id.clone(),
            text: "请继续项目工作".into(),
        })
        .unwrap();
    assert_eq!(rejected.status, "rejected");
    assert!(rejected.generation.is_none());
    let refused = key(&conversation_id).unwrap();
    let snapshot = subject
        .snapshot_for(&SubjectScope {
            channel: "web".into(),
            conversation_id: refused.session_id,
            principal_id: refused.user_id,
        })
        .unwrap();
    let event = snapshot
        .events
        .iter()
        .find(|event| event.input.dedupe_key == format!("web-request:{request_id}"))
        .unwrap();
    assert_eq!(event.status, EventStatus::Cancelled);
    assert_eq!(
        chat.load(&ChatLookup {
            conversation_id: conversation_id.clone(),
            request_id: request_id.clone(),
        })
        .unwrap()
        .unwrap()
        .state
        .status,
        "rejected"
    );
    assert_eq!(
        chat.submit(ChatSubmit {
            conversation_id,
            request_id,
            text: "请继续项目工作".into(),
        })
        .unwrap()
        .status,
        "rejected"
    );

    admission
        .finish(&ticket, QqAdmissionResult::Started)
        .unwrap();
    // 自主工作优先级更低；若拒绝的 Web 工作仍在队头，这里永远无法领取。
    let background = key(&format!("{:032x}", 4)).unwrap();
    let next = admission
        .begin_work(
            &background,
            "background-after-web-busy",
            EventPriority::Autonomy,
            EventSource::AutonomyScheduler,
        )
        .unwrap()
        .expect("未受理的 Web 请求不能堵住后续后台工作");
    admission.finish(&next, QqAdmissionResult::Started).unwrap();
    kernel.stop_all().await.unwrap();
}

struct AwaitCancellation;
impl ControlRunner for AwaitCancellation {
    fn run<'a>(
        &'a self,
        _: SessionInput,
        sink: &'a dyn eve_llm_api::TurnEventSink,
    ) -> RunFuture<'a> {
        Box::pin(async move {
            let _ = sink.closed().await;
            RunReport {
                turn_id: Some(1),
                commit: CommitState::Failed,
                text: None,
                transcript: None,
                started_tools: Some(0),
                tool_results: Vec::new(),
                failure: Some(RunFailure::Execution(LlmError::Cancelled)),
            }
        })
    }
}

struct FailAdmissionFinish;
impl QqAdmission for FailAdmissionFinish {
    fn begin(&self, _: &SessionKey, _: &str) -> eve_plugin_api::PluginResult<Option<String>> {
        Ok(Some("held-admission".into()))
    }
    fn begin_control(
        &self,
        session: &SessionKey,
        source: &str,
        _: bool,
    ) -> eve_plugin_api::PluginResult<Option<String>> {
        self.begin(session, source)
    }
    fn finish(&self, _: &str, _: QqAdmissionResult) -> eve_plugin_api::PluginResult<()> {
        Err(eve_plugin_api::PluginError::State(
            "模拟控制已启动后的主体结算存储故障".into(),
        ))
    }
    fn abandon(&self, _: &SessionKey, _: &str) -> eve_plugin_api::PluginResult<()> {
        Ok(())
    }
    fn completed(&self, _: &SessionKey, _: &str, _: bool) -> eve_plugin_api::PluginResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn accepted_execution_is_cancelled_and_receipted_when_admission_settlement_fails() {
    let root = tempfile::tempdir().unwrap();
    let services = eve_kernel::KernelServices::default();
    let registry = services.registry.clone();
    let kernel = eve_kernel::Kernel::with_services(services);
    kernel
        .register(Box::new(
            eve_control_plugin::ControlPlugin::new(Arc::new(AwaitCancellation), Vec::new())
                .unwrap(),
        ))
        .unwrap();
    kernel.start_all().await.unwrap();
    let control = registry
        .get(&eve_plugin_api::ServiceId::new(eve_control_api::CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone();
    let chat = Arc::new(
        WebChat::open(control.clone(), root.path())
            .unwrap()
            .with_admission(Arc::new(FailAdmissionFinish)),
    );
    let conversation_id = format!("{:032x}", 5);
    let request_id = format!("{:032x}", 6);
    assert!(matches!(
        chat.submit(ChatSubmit {
            conversation_id: conversation_id.clone(),
            request_id: request_id.clone(),
            text: "等待安全取消".into(),
        }),
        Err(PanelError::Unavailable)
    ));
    let session = key(&conversation_id).unwrap();
    let snapshot = control.snapshot(&session).unwrap().unwrap();
    assert!(snapshot.cancel_requested);
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        control.wait(&snapshot.key),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(report.cancel_requested);
    assert_eq!(report.run.commit, CommitState::Failed);
    let lookup = ChatLookup {
        conversation_id,
        request_id,
    };
    // 只查看落盘回执；不通过 chat.read 的补保存掩盖 observer 丢失。
    let saved = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let receipt = chat.load(&lookup).unwrap().unwrap();
            if receipt.state.status == "failed" {
                break receipt.state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(saved.generation.as_ref(), Some(&report.key));
    assert!(saved.failure.unwrap().starts_with("已停止本轮"));
    chat.shutdown().await.unwrap();
    kernel.stop_all().await.unwrap();
}
