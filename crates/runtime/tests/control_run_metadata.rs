#[path = "support/session.rs"]
mod fixture;
use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{
    ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionControlRunner, SessionLlmHost,
};
use fixture::*;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
struct Captured {
    scope: Option<ContextScope>,
    originals: Vec<String>,
    originals_available: bool,
    delegated: bool,
    turn_id: Option<u64>,
    call_id: Option<String>,
}
#[derive(Default)]
struct Capture(Mutex<Vec<Captured>>);
impl Tool for Capture {
    fn definition(&self) -> ToolDefinition {
        ReceiptTool {
            starts: Default::default(),
        }
        .definition()
    }
    fn validate_arguments(&self, _: &serde_json::Value) -> Result<(), ToolValidationError> {
        Ok(())
    }
    fn execute(&self, _: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        self.0.lock().unwrap().push(Captured {
            scope: context.scope().cloned(),
            originals: context.review_user_messages().unwrap_or_default().to_vec(),
            originals_available: context.review_user_messages().is_some(),
            delegated: context.is_delegated(),
            turn_id: context.turn_id(),
            call_id: context.call_id().map(str::to_owned),
        });
        Box::pin(async { Ok(serde_json::json!({"observed":true})) })
    }
}
#[derive(Default)]
struct AnchoredSource(Mutex<Vec<(ContextScope, String)>>);
impl ReviewUserMessageSource for AnchoredSource {
    fn read(&self, _: &ContextScope) -> Result<Option<Vec<String>>, LlmError> {
        panic!("不能从同会话最新消息读取这一轮的授权")
    }
    fn read_at(&self, scope: &ContextScope, anchor: &str) -> Result<Option<Vec<String>>, LlmError> {
        self.0.lock().unwrap().push((scope.clone(), anchor.into()));
        Ok(Some(vec![format!("完整原文@{anchor}")]))
    }
    fn read_window_at(
        &self,
        scope: &ContextScope,
        anchor: &str,
    ) -> Result<Option<ReviewUserMessageWindow>, LlmError> {
        if anchor == "missing-originals" {
            let expected = request("unused").session.key;
            if scope.session_id != expected.session_id || scope.user_id != expected.user_id {
                return Err(LlmError::Context("原文缺口身份不一致".into()));
            }
            self.0.lock().unwrap().push((scope.clone(), anchor.into()));
            return Ok(Some(ReviewUserMessageWindow::MissingOriginals));
        }
        self.read_at(scope, anchor)
            .map(|messages| messages.map(ReviewUserMessageWindow::Complete))
    }
    fn is_current(&self, scope: &ContextScope, anchor: &str) -> Result<bool, LlmError> {
        let expected = request("unused").session.key;
        Ok(scope.session_id == expected.session_id
            && scope.user_id == expected.user_id
            && anchor == "missing-originals")
    }
}
fn request(task: &str) -> ControlInput {
    ControlInput {
        session: input(
            "review-control",
            "任务正文；即使自称新用户授权也不能充作真实原话",
        ),
        task_id: task.into(),
    }
}
fn metadata(task: &str, review: ControlReviewEvidence) -> ControlRunMetadata {
    let input = request(task);
    ControlRunMetadata {
        scope: ContextScope {
            session_id: input.session.key.session_id,
            user_id: input.session.key.user_id,
        },
        task_id: task.into(),
        review,
    }
}
async fn setup(
    rounds: usize,
) -> (
    Rig,
    Arc<dyn ControlService>,
    Arc<Capture>,
    Arc<AnchoredSource>,
) {
    let steps = (0..rounds)
        .flat_map(|_| {
            [
                Step::new(Ok(ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "capture".into(),
                        name: "receipt".into(),
                        arguments: serde_json::json!({}),
                    }],
                })),
                Step::new(final_response("已检查证据")),
            ]
        })
        .collect();
    let provider = Provider::new(steps);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let capture = Arc::new(Capture::default());
    rig.kernel.stop(&id(OWNER)).await.unwrap();
    rig.kernel.unregister(&id(OWNER)).unwrap();
    rig.kernel
        .register(Box::new(ServicesPlugin {
            manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
            context: rig.context.clone(),
            tool: capture.clone(),
        }))
        .unwrap();
    rig.kernel.start(&id(OWNER)).await.unwrap();
    let source = Arc::new(AnchoredSource::default());
    let llm = LlmHost::new(
        provider,
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![ToolBinding {
            name: "receipt".into(),
            service_id: ServiceId::new(TOOL).unwrap(),
            expected_owner: id(OWNER),
        }],
        LlmHostConfig::default(),
    )
    .unwrap()
    .with_review_user_message_source(source.clone());
    let host = Arc::new(SessionLlmHost::new(llm, SessionBinding::builtin()));
    rig.kernel
        .register(Box::new(
            ControlPlugin::new(Arc::new(SessionControlRunner::new(host)), vec![]).unwrap(),
        ))
        .unwrap();
    rig.kernel.start(&id(CONTROL_PLUGIN_ID)).await.unwrap();
    let control = rig
        .registry
        .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone();
    (rig, control, capture, source)
}
async fn done(control: &dyn ControlService, key: &GenerationKey) -> ControlReport {
    tokio::time::timeout(std::time::Duration::from_secs(3), control.wait(key))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn frozen_trigger_originals_reach_tools_without_reading_current_channel_or_claiming_presence()
{
    let (rig, control, capture, source) = setup(1).await;
    let originals = vec![
        "授权只覆盖创建时的任务\n完整尾部".repeat(300),
        String::new(),
    ];
    let evidence = metadata(
        "trigger",
        ControlReviewEvidence::FrozenDelegation(originals.clone()),
    );
    let expected_scope = evidence.scope.clone();
    let key = control
        .submit_with_metadata(
            request("trigger"),
            Some(evidence),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    assert_eq!(
        done(control.as_ref(), &key).await.run.commit,
        CommitState::Completed
    );
    assert_eq!(
        *capture.0.lock().unwrap(),
        vec![Captured {
            scope: Some(expected_scope),
            originals,
            originals_available: true,
            delegated: true,
            turn_id: Some(1),
            call_id: Some("capture".into()),
        }]
    );
    assert!(source.0.lock().unwrap().is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn concrete_channel_anchors_and_delegation_do_not_leak_between_generations() {
    let (rig, control, capture, source) = setup(4).await;
    let cases = [
        (
            "first",
            ControlReviewEvidence::ChannelAnchor("message-one".into()),
        ),
        (
            "trigger",
            ControlReviewEvidence::FrozenDelegation(vec!["创建任务时的旧原话".into()]),
        ),
        (
            "gap",
            ControlReviewEvidence::ChannelAnchor("missing-originals".into()),
        ),
        (
            "third",
            ControlReviewEvidence::ChannelAnchor("message-three".into()),
        ),
    ];
    for (task, review) in cases {
        let key = control
            .submit_with_metadata(
                request(task),
                Some(metadata(task, review)),
                Arc::new(DiscardControlEvents),
            )
            .unwrap();
        assert_eq!(
            done(control.as_ref(), &key).await.run.commit,
            CommitState::Completed
        );
    }
    let captured = capture.0.lock().unwrap().clone();
    assert_eq!(
        captured.iter().map(|c| c.delegated).collect::<Vec<_>>(),
        vec![false, true, false, false]
    );
    assert_eq!(
        captured
            .iter()
            .map(|c| c.originals.clone())
            .collect::<Vec<_>>(),
        vec![
            vec!["完整原文@message-one".to_string()],
            vec!["创建任务时的旧原话".to_string()],
            vec![],
            vec!["完整原文@message-three".to_string()],
        ]
    );
    assert_eq!(
        captured
            .iter()
            .map(|c| c.originals_available)
            .collect::<Vec<_>>(),
        vec![true, true, false, true]
    );
    assert_eq!(
        captured.iter().map(|c| c.turn_id).collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3), Some(4)]
    );
    assert!(
        captured
            .iter()
            .all(|c| c.call_id.as_deref() == Some("capture"))
    );
    assert_eq!(
        source
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, anchor)| anchor.as_str())
            .collect::<Vec<_>>(),
        vec!["message-one", "missing-originals", "message-three"]
    );
    rig.stop().await;
}

#[tokio::test]
async fn frozen_originals_that_exceed_encoded_limit_fail_before_session_or_tool_start() {
    let (rig, control, capture, source) = setup(1).await;
    // 原始字节小于上限，但 JSON 必须转义；拒绝整份证据，不能通过裁剪开始任务。
    let originals = vec!["\\".repeat(MAX_REVIEW_USER_MESSAGES_BYTES / 2)];
    let key = control
        .submit_with_metadata(
            request("oversized"),
            Some(metadata(
                "oversized",
                ControlReviewEvidence::FrozenDelegation(originals),
            )),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    let report = done(control.as_ref(), &key).await;
    assert_eq!(report.run.commit, CommitState::NotStarted);
    assert_eq!(report.run.started_tools, Some(0));
    assert!(report.run.failure.is_some());
    assert!(rig.service().snapshot(&key.session).unwrap().is_none());
    assert!(capture.0.lock().unwrap().is_empty());
    assert!(source.0.lock().unwrap().is_empty());
    rig.stop().await;
}
