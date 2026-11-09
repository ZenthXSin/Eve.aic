use super::*;
use eve_kernel::{KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole, LlmProvider, ModelRequest, ModelResponse, ModelSession};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct Model {
    replies: Mutex<VecDeque<Result<ModelResponse, LlmError>>>,
    requests: Mutex<Vec<ModelRequest>>,
    resolutions: AtomicUsize,
    unavailable: AtomicBool,
}
impl Model {
    fn scripted(replies: Vec<Result<ModelResponse, LlmError>>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            ..Self::default()
        })
    }
}
impl LlmProvider for Model {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            request.validate()?;
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("没有额外模型请求")
        })
    }
}
struct Resolver(Arc<Model>);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<eve_llm_api::ModelSelection, LlmError> {
        self.0.resolutions.fetch_add(1, Ordering::SeqCst);
        if self.0.unavailable.load(Ordering::SeqCst) {
            return Err(LlmError::Configuration("测试模型配置暂不可用".into()));
        }
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
#[derive(Default)]
struct Probe(Mutex<Vec<ToolExecutionContext>>);
struct BlockProgress;
impl Tool for BlockProgress {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_progress".into(),
            description: "记录真实未完成事项".into(),
            argument_schema: json!({"type":"object","properties":{
                "status":{"const":"blocked"},"remaining":{"type":"string"}},
                "required":["status","remaining"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value["status"] == "blocked"
            && value["remaining"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        {
            Ok(())
        } else {
            Err(invalid("缺少真实阻塞事项"))
        }
    }
    fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move { Ok(call.arguments) })
    }
}

impl Tool for Probe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect_source".into(),
            description: "核对已提供源码".into(),
            argument_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value == &json!({}) {
            Ok(())
        } else {
            Err(invalid("参数须为空对象"))
        }
    }
    fn execute(&self, _: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            context.check_review_evidence()?;
            self.0.lock().unwrap().push(context);
            Ok(json!({"source_checked":true}))
        })
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    kernel: Kernel,
    late: Arc<Late>,
    probe: Arc<Probe>,
}
async fn open_fixture(task: Arc<Model>, coding: Arc<Model>, authorized: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let backends = KernelServices {
        state: Arc::new(FileStateStore::open(directory.path().join("state")).unwrap()),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
        .unwrap();
    kernel
        .register(Box::new(
            eve_config_plugin::ConfigPlugin::new(eve_config_plugin::ConfigBootstrap::new(
                directory.path().join("config"),
                vec![schema(), crate::config::history_schema()],
            ))
            .unwrap(),
        ))
        .unwrap();
    let probe = Arc::new(Probe::default());
    kernel
        .register(Box::new(
            services::CoreServices::new()
                .unwrap()
                .with_tools(vec![probe.clone(), Arc::new(BlockProgress)]),
        ))
        .unwrap();
    kernel.start_all().await.unwrap();
    let worker = Worker::install(
        &kernel,
        registry.clone(),
        permissions,
        &["inspect_source".into(), "task_progress".into()],
        LlmHostConfig::default(),
        WorkerModels {
            task: Arc::new(Resolver(task)),
            coding: Arc::new(Resolver(coding)),
        },
        Arc::new(NoDigest),
        Arc::new(SqliteRunStorage::open(&directory.path().join("subagent-runs.sqlite3")).unwrap()),
    )
    .await
    .unwrap();
    worker.authorize_with(crate::qq_admin::Admin::stub(authorized));
    let late = Arc::new(Late::default());
    assert!(
        late.config
            .set(config_service(registry.as_ref()).unwrap())
            .is_ok()
    );
    assert!(late.worker.set(worker).is_ok());
    assert!(
        late.profiles
            .set(Arc::new(
                eve_subagent_profile_sqlite::SqliteSubagentProfiles::open(
                    &directory.path().join("profiles.sqlite3")
                )
                .unwrap()
            ))
            .is_ok()
    );
    Fixture {
        _directory: directory,
        kernel,
        late,
        probe,
    }
}
fn scope() -> ContextScope {
    ContextScope {
        session_id: "qq:test-window".into(),
        user_id: "qq:test-owner".into(),
    }
}
fn context(call: &str, pool: eve_llm_api::SharedToolSteps) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(scope()))
        .with_turn_id(Some(7))
        .with_call_id(call.into())
        .with_shared_steps(pool)
        .with_model_session(
            ModelSession::scoped("main", &scope())
                .unwrap()
                .with_turn("7")
                .unwrap(),
        )
        .with_review_user_messages(Arc::new(vec!["读取本项目源码并核验".into()]))
        .with_conversation(Arc::new(vec![ChatMessage::text(
            ChatRole::User,
            "已有源码线索，不要重做已完成操作",
        )]))
}
fn final_text(text: &str) -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::Final { text: text.into() })
}
async fn invoke(
    fixture: &Fixture,
    id: &str,
    context: ToolExecutionContext,
    text_only: bool,
) -> Result<Value, ToolExecutionError> {
    let mut arguments =
        json!({"agent_id":id,"task":"完成委托并报告真实结果","include_context":true});
    if text_only {
        arguments["verification"] = json!({"kind":"text","required_fragments":["结果"]});
    }
    let tool = SubagentTool {
        late: fixture.late.clone(),
    };
    tool.validate_arguments(&arguments).unwrap();
    tool.execute(
        ToolCall {
            id: context.call_id().unwrap().into(),
            name: SUBAGENT_TOOL.into(),
            arguments,
        },
        context,
    )
    .await
}

#[tokio::test]
async fn coding_subagent_routes_only_its_model_and_preserves_context_budget_and_session() {
    let task = Model::scripted(vec![]);
    let coding = Model::scripted(vec![
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "inspect-1".into(),
                name: "inspect_source".into(),
                arguments: json!({}),
            }],
        }),
        final_text("已按实际工具结果核对源码"),
    ]);
    let fixture = open_fixture(task.clone(), coding.clone(), true).await;
    let pool = eve_llm_api::SharedToolSteps::new(5).unwrap();
    assert!(pool.reserve(1).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let caller = context("coding-call", pool.clone()).with_deadline(Some(deadline));
    let parent = caller.model_session().unwrap().clone();
    let expected = parent
        .fork(
            "subagent",
            Some(&format!("{}:coding-call", parent.turn_id().unwrap())),
        )
        .unwrap();
    let result = invoke(&fixture, "coding", caller, false).await.unwrap();
    assert_eq!(
        result["completed"], false,
        "未给验收合同不能把工具执行夸成完成"
    );
    assert_eq!(result["blocked"], false, "缺少验收合同不表示实际工作被阻塞");
    assert_eq!(result["status"], "returned_pending_verification");
    assert_eq!(
        result["verification"],
        json!({
            "status":"unverifiable", "reason":"missing_frozen_completion_contract"
        })
    );
    assert_eq!(
        result["result"], "已按实际工具结果核对源码",
        "保留真实输出，不追加失败自述"
    );
    assert_eq!(result["usage"]["steps_used"], 1);
    assert_eq!(result["usage"]["model_requests"], 2);
    assert_eq!(pool.remaining().unwrap(), 3);
    assert!(task.requests.lock().unwrap().is_empty());
    assert_eq!(coding.resolutions.load(Ordering::SeqCst), 1);
    {
        let requests = coding.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                request.session,
                Some(expected.for_request(index as u64 + 1))
            );
            assert_ne!(
                request.session.as_ref().unwrap().prompt_cache_key(),
                parent.prompt_cache_key()
            );
            assert!(
                request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == ChatRole::User)
                    .unwrap()
                    .text
                    .as_ref()
                    .unwrap()
                    .len()
                    < 8192
            );
            assert!(
                !request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == ChatRole::User)
                    .unwrap()
                    .text
                    .as_ref()
                    .unwrap()
                    .contains("已有源码线索")
            );
            assert!(
                request
                    .tools
                    .iter()
                    .all(|tool| !ORCHESTRATION_TOOLS.contains(&tool.name.as_str()))
            );
            assert!(
                request
                    .messages
                    .iter()
                    .filter_map(|m| m.text.as_deref())
                    .any(|text| text.contains("已有源码线索"))
            );
        }
        assert!(
            requests[1]
                .messages
                .iter()
                .flat_map(|m| &m.tool_results)
                .any(|r| r.call_id == "inspect-1")
        );
    }
    {
        let contexts = fixture.probe.0.lock().unwrap();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].scope(), Some(&scope()));
        assert!(contexts[0].is_delegated());
        assert!(
            contexts[0]
                .deadline()
                .unwrap()
                .saturating_duration_since(std::time::Instant::now())
                <= Duration::from_secs(5),
            "工具继续继承父期限，不能重新领取默认工具期限"
        );
        assert_eq!(
            contexts[0].review_user_messages().unwrap(),
            &["读取本项目源码并核验"]
        );
    }
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn ordinary_and_custom_coding_named_roles_and_background_keep_task_model() {
    let task = Model::scripted((0..5).map(|_| final_text("普通任务结果")).collect());
    let coding = Model::scripted(vec![]);
    coding.unavailable.store(true, Ordering::SeqCst);
    let fixture = open_fixture(task.clone(), coding.clone(), true).await;
    let custom = fixture
        .late
        .profiles
        .get()
        .unwrap()
        .create(
            &scope(),
            eve_task_api::SubagentProfileDraft {
                name: "coding".into(),
                instructions: "模型不能由显示名称改变".into(),
                tools: None,
            },
        )
        .unwrap();
    for id in ["general", "research", "review", custom.id.as_str()] {
        let result = invoke(
            &fixture,
            id,
            context(id, eve_llm_api::SharedToolSteps::new(5).unwrap()),
            true,
        )
        .await
        .unwrap();
        assert_eq!(result["completed"], true);
        assert_eq!(result["result"], "普通任务结果");
    }
    let (_stop, stopped) = watch::channel(false);
    let run = fixture
        .late
        .worker
        .get()
        .unwrap()
        .run_request(WorkerRequest {
            exploration: false,
            model_session: None,
            idle_review: false,
            idle_input: None,
            instructions: "源码调试字样不改变普通后台模型".into(),
            delegated_context: None,
            scope: Some(scope()),
            steps: 5,
            timeout: None,
            stop: stopped,
            progress: None,
            journal: None,
            shared_steps: None,
            profile: None,
            review_messages: vec![],
            completion_contract: None,
            subject_plan: None,
            background_task_id: Some("task-existing".into()),
        })
        .await;
    assert!(matches!(run.end, TaskEnd::Completed { .. }));
    assert_eq!(task.requests.lock().unwrap().len(), 5);
    assert_eq!(coding.resolutions.load(Ordering::SeqCst), 0);
    assert!(coding.requests.lock().unwrap().is_empty());
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn coding_failure_and_revoked_identity_never_fall_back_or_restart_work() {
    let task = Model::scripted(vec![final_text("普通结果"), Err(LlmError::ProviderTimeout)]);
    let coding = Model::scripted(vec![Err(LlmError::ProviderTimeout), final_text("编程结果")]);
    let fixture = open_fixture(task.clone(), coding.clone(), true).await;
    let call = |id| context(id, eve_llm_api::SharedToolSteps::new(5).unwrap());
    assert!(
        invoke(&fixture, "coding", call("failed-coding"), true)
            .await
            .is_err()
    );
    assert!(task.requests.lock().unwrap().is_empty());
    assert_eq!(coding.requests.lock().unwrap().len(), 1);
    assert_eq!(
        invoke(&fixture, "general", call("healthy-task"), true)
            .await
            .unwrap()["completed"],
        true
    );
    assert!(
        invoke(&fixture, "general", call("failed-task"), true)
            .await
            .is_err()
    );
    assert_eq!(
        invoke(&fixture, "coding", call("healthy-coding"), true)
            .await
            .unwrap()["completed"],
        true
    );
    coding.unavailable.store(true, Ordering::SeqCst);
    assert!(
        invoke(&fixture, "coding", call("missing-config"), true)
            .await
            .is_err()
    );
    assert_eq!(task.requests.lock().unwrap().len(), 2);
    assert_eq!(coding.requests.lock().unwrap().len(), 2);
    fixture.kernel.stop_all().await.unwrap();
    let rejected_task = Model::scripted(vec![]);
    let rejected_coding = Model::scripted(vec![]);
    let rejected = open_fixture(rejected_task.clone(), rejected_coding.clone(), false).await;
    assert!(
        invoke(&rejected, "coding", call("revoked"), true)
            .await
            .is_err()
    );
    assert!(rejected_task.requests.lock().unwrap().is_empty());
    assert!(rejected_coding.requests.lock().unwrap().is_empty());
    assert_eq!(rejected_coding.resolutions.load(Ordering::SeqCst), 0);
    rejected.kernel.stop_all().await.unwrap();
}

#[test]
fn nested_delegation_flattens_original_records_without_wrapping_current_instructions() {
    let original = vec![
        ChatMessage::text(ChatRole::System, "旧系统资料"),
        ChatMessage::text(ChatRole::User, "旧用户原话"),
    ];
    let current = vec![
        ChatMessage::text(ChatRole::System, "后台系统资料"),
        ChatMessage::text(ChatRole::User, "后台当前任务"),
    ];
    let caller = context("flatten", eve_llm_api::SharedToolSteps::new(5).unwrap())
        .with_delegated_context(Arc::new(original.clone()))
        .with_conversation(Arc::new(current.clone()));
    let captured = capture_context(&json!({"include_context":true}), &caller)
        .unwrap()
        .unwrap();
    let messages: Vec<ChatMessage> = serde_json::from_str(&captured.messages_json).unwrap();
    assert_eq!(
        messages,
        original.into_iter().chain(current).collect::<Vec<_>>()
    );
    assert!(!captured.messages_json.contains("eve.delegated_context"));
    assert_eq!(
        instructions_with_context("本次编程委托".into(), Some(&captured), &captured.owner).unwrap(),
        "本次编程委托"
    );
    assert_eq!(
        caller.review_user_messages().unwrap(),
        ["读取本项目源码并核验"]
    );
    let stranger = TaskOwner {
        user_id: "someone-else".into(),
        ..captured.owner.clone()
    };
    assert!(instructions_with_context("本次编程委托".into(), Some(&captured), &stranger).is_err());
}

#[test]
fn subagent_result_preserves_pending_mismatch_and_actual_blocking_as_distinct_outcomes() {
    use eve_task_api::TaskCompletionAssessment;
    let original = "已取得真实工具回执，仍需核验整体目标";
    let usage = json!({"steps_used":2,"model_requests":3});
    for (assessment, status, blocked) in [
        (
            TaskCompletionAssessment::Unverifiable {
                reason: "missing_frozen_completion_contract".into(),
            },
            "returned_pending_verification",
            false,
        ),
        (
            TaskCompletionAssessment::Unverifiable {
                reason: "unknown_tool_effect_not_replayed".into(),
            },
            "returned_pending_verification",
            false,
        ),
        (
            TaskCompletionAssessment::Mismatch {
                reason: "receipt_effect_mismatch".into(),
            },
            "verification_mismatch",
            false,
        ),
        (
            TaskCompletionAssessment::Blocked {
                reason: "还缺用户指定的输入文件".into(),
            },
            "blocked",
            true,
        ),
    ] {
        let result = subagent_result(
            TaskEnd::Blocked {
                result: original.into(),
            },
            Some(assessment.clone()),
            usage.clone(),
            true,
        )
        .unwrap();
        assert_eq!(result["status"], status);
        assert_eq!(result["blocked"], blocked);
        assert_eq!(result["completed"], false);
        assert_eq!(
            result["verification"],
            serde_json::to_value(assessment).unwrap()
        );
        assert_eq!(result["result"], original);
        assert_eq!(result["usage"], usage);
    }
    let legacy = subagent_result(
        TaskEnd::Blocked {
            result: original.into(),
        },
        None,
        usage.clone(),
        false,
    )
    .unwrap();
    assert_eq!(
        legacy["blocked"], true,
        "没有独立分类的既有真实阻塞不能冒充已核验"
    );
    assert_eq!(legacy["completed"], false);
    let completed = subagent_result(
        TaskEnd::Completed {
            result: "固定合同满足".into(),
        },
        Some(TaskCompletionAssessment::Verified),
        usage.clone(),
        true,
    )
    .unwrap();
    assert_eq!(completed["completed"], true);
    assert_eq!(completed["blocked"], false);
    assert_eq!(completed["status"], "completed");
    assert!(
        subagent_result(
            TaskEnd::Completed {
                result: original.into()
            },
            None,
            usage.clone(),
            false
        )
        .is_err()
    );
    assert!(
        subagent_result(
            TaskEnd::Blocked {
                result: original.into()
            },
            Some(TaskCompletionAssessment::Verified),
            usage,
            false
        )
        .is_err()
    );
}

#[tokio::test]
async fn coding_explicit_blocking_receipt_remains_blocked_even_without_a_completion_contract() {
    let task = Model::scripted(vec![]);
    let coding = Model::scripted(vec![
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "blocked-step".into(),
                name: "task_progress".into(),
                arguments: json!({"status":"blocked","remaining":"仍缺指定的输入文件"}),
            }],
        }),
        final_text("已有检查结果，缺少输入文件无法继续"),
    ]);
    let fixture = open_fixture(task.clone(), coding.clone(), true).await;
    let pool = eve_llm_api::SharedToolSteps::new(5).unwrap();
    assert!(pool.reserve(1).unwrap());
    let result = invoke(
        &fixture,
        "coding",
        context("blocked-coding", pool.clone()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["blocked"], true);
    assert_eq!(result["completed"], false);
    assert_eq!(
        result["verification"],
        json!({"status":"blocked","reason":"仍缺指定的输入文件"})
    );
    assert_eq!(result["result"], "已有检查结果，缺少输入文件无法继续");
    assert_eq!(pool.remaining().unwrap(), 3);
    assert_eq!(coding.requests.lock().unwrap().len(), 2);
    assert!(task.requests.lock().unwrap().is_empty());
    fixture.kernel.stop_all().await.unwrap();
}
