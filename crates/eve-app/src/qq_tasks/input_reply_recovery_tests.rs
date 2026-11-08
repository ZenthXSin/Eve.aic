use super::*;
use eve_control_api::{CommitState, ControlReport, GenerationKey, RunFailure, RunReport};
use eve_kernel::{KernelServices, backends::FileStateStore};
use eve_llm_api::{
    ChatMessage, ChatRole, InputReplyDelivery, InputReplyHandoff, LlmProvider, ModelRequest,
    ModelResponse, SteeringBatch, SteeringUpdate, TurnSteering, TurnSteeringSource,
};
use eve_plugin_api::ServiceRegistry;
use eve_qqbot_plugin::QqFailureRecovery;
use eve_runtime::{SessionBinding, SessionLlmHost, SessionRunError};
use eve_session_api::{SessionInput, SessionService, SessionServiceHandle, SessionTurnStatus};
use eve_session_plugin::{FileSessionArchive, SessionPlugin};
use eve_task_plugin::{TaskController, TaskPlugin};
use std::{collections::VecDeque, path::Path, sync::Mutex};

const ORIGINAL: &str = "检查原任务源码";
const FOLLOWUP: &str = "你还在吗？也检查 {{NAME_ab12cd34}} 的补充业务";

struct ScriptedProvider {
    responses: Mutex<VecDeque<Result<ModelResponse, LlmError>>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl ScriptedProvider {
    fn new(responses: Vec<Result<ModelResponse, LlmError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        })
    }
}
impl LlmProvider for ScriptedProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            request.validate().unwrap();
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("不应增加模型判断或重复请求")
        })
    }
}
struct Resolver(Arc<ScriptedProvider>);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<eve_llm_api::ModelSelection, LlmError> {
        Ok(eve_llm_api::ModelSelection {
            provider: self.0.clone(),
            provider_timeout: Duration::from_secs(5),
        })
    }
}
struct NoDigest;
impl eve_llm_api::HistoryDigests for NoDigest {
    fn digest(
        &self,
        _: Option<&ContextScope>,
        _: &[ChatMessage],
    ) -> Result<Option<eve_llm_api::HistoryDigest>, LlmError> {
        Ok(None)
    }
}
struct Steering(Mutex<VecDeque<SteeringUpdate>>);
impl TurnSteering for Steering {
    fn take(&self, _: bool) -> Result<Vec<String>, LlmError> {
        panic!("必须原子消费业务原话与交接事实")
    }
    fn take_update(&self, _: bool) -> Result<SteeringUpdate, LlmError> {
        Ok(self.0.lock().unwrap().pop_front().unwrap_or_default())
    }
}
struct SteeringSource(Arc<Steering>);
impl TurnSteeringSource for SteeringSource {
    fn open(&self, _: &ContextScope, _: u64) -> Result<Arc<dyn TurnSteering>, LlmError> {
        Ok(self.0.clone())
    }
}
struct Originals;
impl eve_llm_api::ReviewUserMessageSource for Originals {
    fn read(&self, _: &ContextScope) -> Result<Option<Vec<String>>, LlmError> {
        Ok(Some(vec![ORIGINAL.into()]))
    }
    fn read_at(&self, _: &ContextScope, anchor: &str) -> Result<Option<Vec<String>>, LlmError> {
        assert_eq!(anchor, "followup-1");
        Ok(Some(vec![ORIGINAL.into(), FOLLOWUP.into()]))
    }
    fn is_current(&self, _: &ContextScope, anchor: &str) -> Result<bool, LlmError> {
        Ok(anchor == "followup-1")
    }
}
struct Probe(Mutex<Vec<(String, Vec<String>)>>);
impl Tool for Probe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect_source".into(),
            description: "记录实际源码核验与可信原话".into(),
            argument_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_some_and(|value| value.is_empty()) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "参数必须为空对象".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            context.check_review_evidence()?;
            self.0.lock().unwrap().push((
                call.id,
                context
                    .review_user_messages()
                    .expect("原话独立继承")
                    .to_vec(),
            ));
            Ok(json!({"source_checked":true}))
        })
    }
}
struct Fixture {
    kernel: Kernel,
    registry: Arc<dyn ServiceRegistry>,
    permissions: Arc<dyn PermissionChecker>,
    sessions: Arc<dyn SessionService>,
    tasks: Arc<TaskController>,
    probe: Arc<Probe>,
}
async fn open(root: &Path) -> Fixture {
    let services = KernelServices {
        state: Arc::new(FileStateStore::open(root.join("state")).unwrap()),
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let task_plugin = TaskPlugin::new().unwrap();
    let tasks = Arc::new(task_plugin.controller());
    kernel.register(Box::new(task_plugin)).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap().with_archive(
            Arc::new(FileSessionArchive::open(root.join("archive")).unwrap()),
        )))
        .unwrap();
    kernel
        .register(Box::new(
            eve_config_plugin::ConfigPlugin::new(
                eve_config_plugin::ConfigBootstrap::new(
                    root.join("config"),
                    vec![schema(), crate::config::history_schema()],
                )
                .with_environment(BTreeMap::from([(
                    "EVE_TASK_AUTO_RECOVER_FAILURES".into(),
                    "true".into(),
                )])),
            )
            .unwrap(),
        ))
        .unwrap();
    let probe = Arc::new(Probe(Mutex::new(vec![])));
    kernel
        .register(Box::new(
            services::CoreServices::new()
                .unwrap()
                .with_tools(vec![probe.clone()]),
        ))
        .unwrap();
    kernel.start_all().await.unwrap();
    let sessions = registry
        .get(&ServiceId::new(eve_session_api::SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    Fixture {
        kernel,
        registry,
        permissions,
        sessions,
        tasks,
        probe,
    }
}
fn tool_response(id: &str) -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::ToolCalls {
        calls: vec![ToolCall {
            id: id.into(),
            name: "inspect_source".into(),
            arguments: json!({}),
        }],
    })
}
fn handoff(delivery: InputReplyDelivery) -> InputReplyHandoff {
    InputReplyHandoff {
        input_message_id: "followup-1".into(),
        input_text: FOLLOWUP.into(),
        event_id: "foreground:event-1".into(),
        reply_text: "我在处理，命令仍在运行".into(),
        delivery,
    }
}
fn host(fixture: &Fixture, provider: Arc<ScriptedProvider>) -> LlmHost {
    LlmHost::new(
        provider,
        fixture.registry.clone(),
        fixture.kernel.clone(),
        fixture.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(services::CONTEXT).unwrap(),
            expected_owner: PluginId::new(services::OWNER).unwrap(),
        },
        vec![ToolBinding {
            name: "inspect_source".into(),
            service_id: ServiceId::new(services::tool_service_id("inspect_source")).unwrap(),
            expected_owner: PluginId::new(services::OWNER).unwrap(),
        }],
        LlmHostConfig::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn failure_handoff_survives_archive_and_task_restart_then_executes_followup_without_extra_requests()
 {
    for delivery in [
        InputReplyDelivery::Sending,
        InputReplyDelivery::Sent {
            platform_message_id: "platform-1".into(),
        },
        InputReplyDelivery::Unknown,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let fixture = open(directory.path()).await;
        let key = SessionKey::new("qq:owner", "qq:owner").unwrap();
        let sending = handoff(InputReplyDelivery::Sending);
        let expected = handoff(delivery.clone());
        let main = ScriptedProvider::new(vec![
            tool_response("initial-work"),
            Err(LlmError::ProviderTimeout),
        ]);
        let update = SteeringUpdate {
            batch: SteeringBatch {
                messages: vec![FOLLOWUP.into()],
                review_anchor: Some(sending.input_message_id.clone()),
            },
            input_replies: vec![sending],
        };
        let terminal = if delivery == InputReplyDelivery::Sending {
            SteeringUpdate::default()
        } else {
            SteeringUpdate {
                batch: SteeringBatch::default(),
                input_replies: vec![expected.clone()],
            }
        };
        let source = Arc::new(SteeringSource(Arc::new(Steering(Mutex::new(
            VecDeque::from([update, terminal]),
        )))));
        let result = SessionLlmHost::new(
            host(&fixture, main.clone())
                .with_review_user_message_source(Arc::new(Originals))
                .with_steering(source),
            SessionBinding::builtin(),
        )
        .run_turn(SessionInput {
            key: key.clone(),
            text: ORIGINAL.into(),
        })
        .await;
        let SessionRunError::Turn(failed) = result.unwrap_err() else {
            panic!("前台模型失败应已存盘");
        };
        // 前台终态在原任务工具被模型选中后到达；它不能丢弃工具或增加模型判断。
        assert_eq!(failed.error, LlmError::ProviderTimeout);
        assert_eq!(failed.diagnostics.provider_requests, 2);
        assert_eq!(failed.diagnostics.started_tools, 1);
        let stored = fixture.sessions.snapshot(&key).unwrap().unwrap();
        let SessionTurnStatus::Failed { failure } = &stored.turns[0].status else {
            panic!("失败证据必须保存");
        };
        assert_eq!(
            failure.evidence.as_ref().unwrap().input_replies,
            vec![expected.clone()]
        );
        assert_eq!(
            failure.evidence.as_ref().unwrap().review_user_messages,
            Some(vec![ORIGINAL.into(), FOLLOWUP.into()])
        );
        assert_eq!(
            *fixture.probe.0.lock().unwrap(),
            vec![(
                "initial-work".into(),
                vec![ORIGINAL.into(), FOLLOWUP.into()]
            )]
        );
        let report = ControlReport {
            key: GenerationKey {
                session: key.clone(),
                task_id: "foreground-task".into(),
                controller_epoch: [1; 16],
                generation: 1,
            },
            cancel_requested: false,
            run: RunReport {
                turn_id: Some(1),
                commit: CommitState::Failed,
                text: None,
                transcript: None,
                started_tools: Some(1),
                tool_results: failed.diagnostics.tool_results.clone(),
                failure: Some(RunFailure::Execution(failed.error)),
            },
        };
        fixture.kernel.stop_all().await.unwrap();
        drop(fixture);

        let fixture = open(directory.path()).await;
        assert_eq!(fixture.sessions.snapshot(&key).unwrap().unwrap(), stored);
        let recovery = crate::qq_failure_recovery::Recovery {
            tasks: fixture.tasks.clone(),
            sessions: fixture.sessions.clone(),
            settings: config_service(fixture.registry.as_ref()).unwrap(),
            authorization: crate::qq_admin::Admin::stub(true),
            digests: Arc::new(NoDigest),
        };
        assert!(
            recovery
                .recover(&report)
                .unwrap()
                .unwrap()
                .contains("已保存并转交后台任务")
        );
        let task = fixture.tasks.snapshot().unwrap().tasks.remove(0);
        assert_eq!(
            task.execution.as_ref().unwrap().review_messages,
            vec![ORIGINAL, FOLLOWUP]
        );
        let messages: Vec<ChatMessage> =
            serde_json::from_str(&task.spec.context.as_ref().unwrap().messages_json).unwrap();
        let facts: Value =
            serde_json::from_str(messages.last().unwrap().text.as_deref().unwrap()).unwrap();
        assert_eq!(messages.last().unwrap().role, ChatRole::System);
        assert_eq!(facts["kind"], "eve.input_reply_handoffs");
        assert_eq!(
            facts["input_replies"],
            serde_json::to_value(vec![expected.clone()]).unwrap()
        );
        assert!(
            facts["instruction"]
                .as_str()
                .unwrap()
                .contains("补充业务仍须")
        );
        let original: Value = serde_json::from_str(
            messages
                .iter()
                .find(|message| {
                    message.role == ChatRole::User
                        && message
                            .text
                            .as_ref()
                            .is_some_and(|text| text.contains("eve.failed_turn"))
                })
                .unwrap()
                .text
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert!(
            original["failure"]["evidence"]
                .get("input_replies")
                .is_none()
        );
        drop(recovery);
        fixture.kernel.stop_all().await.unwrap();
        drop(fixture);

        let fixture = open(directory.path()).await;
        let restored = fixture
            .tasks
            .get_owned(&task.owner, &task.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&restored).unwrap(),
            serde_json::to_value(&task).unwrap()
        );
        let background = ScriptedProvider::new(vec![
            tool_response("followup-work"),
            Ok(ModelResponse::Final {
                text: "补充业务已按真实回执核验".into(),
            }),
        ]);
        let worker = Worker::install(
            &fixture.kernel,
            fixture.registry.clone(),
            fixture.permissions.clone(),
            &["inspect_source".into()],
            LlmHostConfig::default(),
            WorkerModels {
                task: Arc::new(Resolver(background.clone())),
                coding: Arc::new(Resolver(ScriptedProvider::new(vec![]))),
            },
            Arc::new(NoDigest),
        )
        .await
        .unwrap();
        worker.authorize_with(crate::qq_admin::Admin::stub(true));
        let started = fixture.tasks.start(&restored.id, now_ms()).unwrap();
        let (_stop, signal) = watch::channel(false);
        let run = worker
            .run_request(WorkerRequest {
                exploration: false,
                model_session: None,
                idle_review: false,
                idle_input: None,
                instructions: instructions_with_context(
                    started.spec.instructions.clone(),
                    started.spec.context.as_ref(),
                    &started.owner,
                )
                .unwrap(),
                delegated_context: started.spec.context.clone(),
                scope: Some(ContextScope {
                    session_id: started.owner.session_id.clone(),
                    user_id: started.owner.user_id.clone(),
                }),
                steps: started.spec.max_steps,
                timeout: None,
                stop: signal,
                journal: Some(Arc::new(TaskJournal {
                    tasks: fixture.tasks.clone(),
                    owner: started.owner.clone(),
                    id: started.id.clone(),
                    deadline_at_ms: None,
                })),
                shared_steps: None,
                profile: None,
                review_messages: crate::qq_task_review::restore(&started, &started.owner).unwrap(),
                completion_contract: started
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.completion_contract.clone()),
                subject_plan: None,
                background_task_id: Some(started.id.clone()),
            })
            .await;
        assert!(matches!(run.end, TaskEnd::Completed { .. }));
        fixture
            .tasks
            .finish(&started.id, run.end, run.usage, now_ms())
            .unwrap();
        assert_eq!(
            *fixture.probe.0.lock().unwrap(),
            vec![(
                "followup-work".into(),
                vec![ORIGINAL.into(), FOLLOWUP.into()]
            )]
        );
        assert_eq!(main.requests.lock().unwrap().len(), 2);
        {
            let requests = background.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "只核验业务并自然结束，没有额外交接判断");
            assert!(requests[0].messages.iter().any(|message| {
                message.text.as_ref().is_some_and(|text| {
                    text.contains("eve.input_reply_handoffs") && text.contains("{{NAME_ab12cd34}}")
                })
            }));
            assert_ne!(
                requests[0].session.as_ref().unwrap().session_id(),
                main.requests.lock().unwrap()[0]
                    .session
                    .as_ref()
                    .unwrap()
                    .session_id()
            );
        }
        let completed = fixture
            .tasks
            .get_owned(&started.owner, &started.id)
            .unwrap()
            .unwrap();
        let checkpoint = completed.execution.unwrap().checkpoint.unwrap();
        assert_eq!(checkpoint.used_tool_calls, 1);
        assert_eq!(checkpoint.provider_requests, 2);
        assert_eq!(
            checkpoint
                .messages
                .iter()
                .flat_map(|message| &message.tool_results)
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["followup-work"]
        );
        fixture.kernel.stop_all().await.unwrap();
    }
}
