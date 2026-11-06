use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, ToolCall, ToolFailureCode, ToolOutput, ToolResult,
    TurnCheckpoint,
};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{
    FileTaskArchive, TaskController, TaskPlugin, evaluate_task_completion, evaluate_task_effect,
    extract_task_strategy,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State(
                "injected exploration storage failure".into(),
            ));
        }
        self.inner.set(namespace, key, value)
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
    archive: Option<Arc<dyn TaskArchiveStore>>,
}
impl World {
    fn new(cold: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
        });
        let archive = cold.then(|| {
            Arc::new(FileTaskArchive::open(directory.path().join("archive")).unwrap())
                as Arc<dyn TaskArchiveStore>
        });
        Self {
            _directory: directory,
            state,
            archive,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let mut plugin = TaskPlugin::new()?;
        if let Some(archive) = &self.archive {
            plugin = plugin.with_archive(archive.clone());
        }
        let tasks = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, tasks))
    }
    fn raw(&self) -> Vec<u8> {
        self.state
            .get(&PluginId::new(TASK_PLUGIN_ID).unwrap(), TASK_STATE_KEY)
            .unwrap()
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .set(
                &PluginId::new(TASK_PLUGIN_ID).unwrap(),
                TASK_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}
fn owner() -> TaskOwner {
    TaskOwner {
        session_id: "exploration-session".into(),
        user_id: "alice".into(),
    }
}
fn spec(max_steps: u32) -> TaskSpec {
    TaskSpec {
        title: "探索会话".into(),
        instructions: "可以自由看看，也可以停下来；不要求目标或成果。".into(),
        max_steps,
        context: None,
    }
}
fn create(tasks: &TaskController, source: &str, max_steps: u32, deadline: Option<u64>) -> Task {
    let task = tasks
        .create_exploration(
            &owner(),
            spec(max_steps),
            source,
            vec!["自主探索不需要目标".into()],
            deadline,
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap()
}
fn frame(result: Option<&str>) -> TurnCheckpoint {
    TurnCheckpoint {
        version: 1,
        scope: ContextScope {
            session_id: owner().session_id,
            user_id: owner().user_id,
        },
        messages: vec![ChatMessage::text(ChatRole::User, "自由探索")],
        pending_calls: vec![],
        used_tool_calls: 0,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 0,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: result.map(str::to_owned),
    }
}
fn batch(frame: &mut TurnCheckpoint, output: ToolOutput) {
    frame.messages.push(
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "observed".into(),
            name: "read_file".into(),
            arguments: json!({"path":"curiosity.txt"}),
        }])
        .unwrap(),
    );
    frame.messages.push(
        ChatMessage::tool_results(vec![ToolResult {
            call_id: "observed".into(),
            output,
        }])
        .unwrap(),
    );
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
}
fn usage(frame: &TurnCheckpoint) -> TaskUsage {
    TaskUsage {
        tool_calls: (frame.used_tool_calls + frame.delegated_tool_calls) as u32,
        model_requests: (frame.provider_requests + frame.delegated_model_requests) as u32,
    }
}
fn assert_silent(tasks: &TaskController, task: &Task) {
    assert!(task.notice.is_none());
    assert!(!task.notice_pending());
    assert!(task.execution.as_ref().unwrap().progress_notices.is_empty());
    let progress = tasks.progress_snapshot().unwrap();
    let entry = progress
        .tasks
        .iter()
        .find(|entry| entry.id == task.id)
        .unwrap();
    assert_eq!(entry.kind, TaskExecutionKind::Exploration);
    assert!(!entry.enabled);
    assert!(!entry.due(u64::MAX));
    assert!(tasks.delivery_snapshot().unwrap().due_proactive().is_none());
    assert!(
        tasks
            .delivery_snapshot()
            .unwrap()
            .due_passive(&owner().user_id, "new-message")
            .is_none()
    );
    assert_eq!(
        tasks.claim(&task.id, DeliveryChannel::Proactive, 100),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        tasks.claim(
            &task.id,
            DeliveryChannel::Passive {
                message_id: "new-message".into()
            },
            100,
        ),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        tasks.claim_progress(
            &owner(),
            &task.id,
            DeliveryChannel::Proactive,
            TASK_PROGRESS_INTERVAL_MS + 10
        ),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        TaskProgressToken::from_task(task),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        tasks.record_local_progress(
            &owner(),
            &task.id,
            1,
            "local-progress",
            TASK_PROGRESS_INTERVAL_MS + 10
        ),
        Err(TaskError::InvalidTransition)
    );
    if let Some(finished_at) = task.finished_at_ms {
        assert_eq!(
            tasks.record_local_delivery(
                &owner(),
                &task.id,
                finished_at,
                "local-notice",
                finished_at
            ),
            Err(TaskError::InvalidTransition)
        );
    }
}

#[tokio::test]
async fn experience_acknowledgement_is_scope_bound_persistent_and_does_not_rewrite_the_session() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(&tasks, "ack", 4, None);
        let checkpoint = frame(Some("随意看过一点"));
        tasks
            .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
            .unwrap();
        assert_eq!(
            tasks.acknowledge_exploration(&owner(), &task.id, 40, 50),
            Err(TaskError::InvalidTransition)
        );
        let ended = tasks
            .finish(
                &task.id,
                TaskEnd::SessionEnded {
                    result: "随意看过一点".into(),
                },
                usage(&checkpoint),
                40,
            )
            .unwrap();
        assert!(ended.settled_at().is_none());
        assert!(
            ended
                .execution
                .as_ref()
                .unwrap()
                .exploration_acknowledged_at_ms
                .is_none()
        );
        let before = world.raw();
        let foreign = TaskOwner {
            session_id: "other".into(),
            ..owner()
        };
        assert_eq!(
            tasks.acknowledge_exploration(&foreign, &task.id, 40, 50),
            Err(TaskError::NotFound)
        );
        assert_eq!(
            tasks.acknowledge_exploration(&owner(), &task.id, 41, 50),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
        assert_eq!(restored, ended);
        assert!(restored.settled_at().is_none());
        let acknowledged = tasks
            .acknowledge_exploration(&owner(), &task.id, 40, 50)
            .unwrap();
        let mut expected = ended;
        expected
            .execution
            .as_mut()
            .unwrap()
            .exploration_acknowledged_at_ms = Some(50);
        assert_eq!(acknowledged, expected);
        assert_eq!(acknowledged.settled_at(), Some(50));
        let before = world.raw();
        assert_eq!(
            tasks
                .acknowledge_exploration(&owner(), &task.id, 40, 99)
                .unwrap(),
            expected
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        assert_eq!(tasks.get_owned(&owner(), &task.id).unwrap(), Some(expected));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn capacity_never_evicts_an_unacknowledged_experience_even_during_an_ack_race() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let mut owners = Vec::new();
        let mut ids = Vec::new();
        for index in 0..MAX_TASKS {
            let owner = TaskOwner {
                session_id: format!("scope-{index}"),
                user_id: format!("owner-{index}"),
            };
            let task = tasks
                .create_exploration(&owner, spec(1), "fill", vec![], None, 10)
                .unwrap();
            // 即便没有执行的取消，也必须先把该次经历持久化，才能释放正文。
            let cancelled = tasks.cancel(&owner.user_id, &task.id, 20).unwrap();
            assert!(cancelled.settled_at().is_none());
            owners.push(owner);
            ids.push(task.id);
        }
        let other = TaskOwner {
            session_id: "new-scope".into(),
            user_id: "new-user".into(),
        };
        let before = world.raw();
        assert_eq!(
            tasks.create_exploration(&other, spec(1), "new", vec![], None, 30),
            Err(TaskError::LimitReached)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        assert_eq!(tasks.snapshot().unwrap().tasks.len(), MAX_TASKS);
        let task_controller = tasks.clone();
        let ack_owner = owners[0].clone();
        let ack_id = ids[0].clone();
        let create_controller = tasks.clone();
        let create_owner = other.clone();
        let (ack, competing_create) = std::thread::scope(|scope| {
            let ack = scope.spawn(move || {
                task_controller.acknowledge_exploration(&ack_owner, &ack_id, 20, 30)
            });
            let create = scope.spawn(move || {
                create_controller.create_exploration(
                    &create_owner,
                    spec(1),
                    "new",
                    vec![],
                    None,
                    30,
                )
            });
            (ack.join().unwrap(), create.join().unwrap())
        });
        ack.unwrap();
        match competing_create {
            Ok(_) => {}
            Err(TaskError::LimitReached) => {
                tasks
                    .create_exploration(&other, spec(1), "new", vec![], None, 30)
                    .unwrap();
            }
            other => panic!("unexpected capacity result: {other:?}"),
        }
        assert!(tasks.get_owned(&owners[0], &ids[0]).unwrap().is_none());
        for (owner, id) in owners.iter().zip(&ids).skip(1) {
            let retained = tasks.get_owned(owner, id).unwrap().unwrap();
            assert!(retained.settled_at().is_none());
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn acknowledgement_storage_failure_keeps_the_unconsumed_experience_after_restart() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let task = tasks
            .create_exploration(&owner(), spec(1), "ack-failure", vec![], None, 10)
            .unwrap();
        let ended = tasks.cancel(&owner().user_id, &task.id, 20).unwrap();
        let before = world.raw();
        world.state.fail.store(true, Ordering::SeqCst);
        assert_eq!(
            tasks.acknowledge_exploration(&owner(), &task.id, 20, 30),
            Err(TaskError::Storage)
        );
        assert_eq!(world.raw(), before);
        world.state.fail.store(false, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
        assert_eq!(restored, ended);
        assert!(restored.settled_at().is_none());
        let acknowledged = tasks
            .acknowledge_exploration(&owner(), &task.id, 20, 30)
            .unwrap();
        assert_eq!(acknowledged.settled_at(), Some(30));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn creation_freezes_mode_identity_authorization_and_deadline_without_a_goal() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let created = tasks
            .create_exploration(
                &owner(),
                spec(4),
                "explore",
                vec!["自由探索".into()],
                Some(1000),
                10,
            )
            .unwrap();
        let execution = created.execution.as_ref().unwrap();
        assert_eq!(execution.kind, TaskExecutionKind::Exploration);
        assert!(execution.completion_contract.is_none());
        assert!(execution.session_end_reason.is_none());
        assert_eq!(execution.authorization_deadline_at_ms, Some(1000));
        let duplicate = tasks
            .create_exploration(
                &owner(),
                spec(10),
                "explore",
                vec!["新授权不能覆盖".into()],
                None,
                11,
            )
            .unwrap();
        assert_eq!(duplicate, created);
        assert_eq!(
            tasks.create(&owner(), spec(4), "explore", 11),
            Err(TaskError::InvalidTransition)
        );
        let ordinary = tasks.create(&owner(), spec(4), "ordinary", 11).unwrap();
        assert_eq!(
            tasks.create_exploration(&owner(), spec(4), "ordinary", vec![], None, 12),
            Err(TaskError::InvalidTransition)
        );
        let old_json = serde_json::to_value(&ordinary).unwrap();
        assert!(old_json["execution"].get("kind").is_none());
        assert!(old_json["execution"].get("session_end_reason").is_none());
        assert!(
            old_json["execution"]
                .get("exploration_acknowledged_at_ms")
                .is_none()
        );
        let old_roundtrip: Task = serde_json::from_value(old_json.clone()).unwrap();
        assert_eq!(serde_json::to_value(old_roundtrip).unwrap(), old_json);
        let foreign = TaskOwner {
            session_id: "other-session".into(),
            ..owner()
        };
        assert_eq!(
            tasks.create_exploration(&foreign, spec(4), "explore", vec![], None, 12),
            Err(TaskError::NotFound)
        );
        let mut wrong_context = spec(4);
        wrong_context.context = Some(DelegatedContext {
            owner: foreign,
            messages_json: serde_json::to_string(&frame(None).messages).unwrap(),
        });
        assert_eq!(
            tasks.create_exploration(&owner(), wrong_context, "wrong-context", vec![], None, 12),
            Err(TaskError::InvalidInput)
        );
        assert_silent(&tasks, &created);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&owner(), &created.id).unwrap().unwrap();
        assert_eq!(restored, created);
        let scheduling = tasks.scheduling_snapshot().unwrap();
        assert!(
            scheduling
                .tasks
                .iter()
                .find(|entry| entry.id == created.id)
                .unwrap()
                .matches(&created)
        );
        let entry = scheduling
            .tasks
            .iter()
            .find(|entry| entry.id == created.id)
            .unwrap();
        let delivery = TaskDeliveryEntry::from(&created);
        let progress = TaskProgressEntry::from(&created);
        let mut substituted = created.clone();
        substituted.execution.as_mut().unwrap().kind = TaskExecutionKind::Task;
        assert!(!entry.matches(&substituted));
        assert!(!delivery.matches(&substituted));
        assert!(!progress.matches(&substituted));
        let summary = tasks.summaries_for(&owner()).unwrap();
        assert_eq!(
            summary
                .iter()
                .find(|entry| entry.id == created.id)
                .unwrap()
                .kind,
            TaskExecutionKind::Exploration
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_directory_mode_cannot_be_changed_independently_of_the_frozen_body() {
    let world = World::new(true);
    let (kernel, tasks) = world.open().await.unwrap();
    tasks
        .create_exploration(&owner(), spec(4), "cold-mode", vec![], None, 10)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let mut index: Value = serde_json::from_slice(&world.raw()).unwrap();
    index["tasks"][0]["directory"]["execution"]
        .as_object_mut()
        .unwrap()
        .remove("kind");
    let bytes = serde_json::to_vec(&index).unwrap();
    world.write(bytes.clone());
    assert!(world.open().await.is_err());
    assert_eq!(world.raw(), bytes);
}

#[tokio::test]
async fn curiosity_can_stop_without_tools_text_or_a_successful_result() {
    for cold in [false, true] {
        for variant in 0..5 {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let task = create(&tasks, "free-stop", 4, None);
            let text = if variant == 0 {
                String::new()
            } else if variant == 4 {
                "这只是当下的想法。".repeat(2000)
            } else {
                "今天没有发现也可以停下来".into()
            };
            let mut checkpoint = frame((variant != 0).then_some(text.as_str()));
            if variant == 2 {
                checkpoint.remaining = Some("以后可能再看看，不必现在完成".into());
                checkpoint.closing = true;
            } else if variant == 3 {
                batch(
                    &mut checkpoint,
                    ToolOutput::Failure {
                        code: ToolFailureCode::PermissionDenied,
                        message: "不属于当前身份的文件".into(),
                    },
                );
            }
            tasks
                .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
                .unwrap();
            let ended = tasks
                .finish(
                    &task.id,
                    TaskEnd::SessionEnded {
                        result: text.clone(),
                    },
                    usage(&checkpoint),
                    40,
                )
                .unwrap();
            assert_eq!(ended.status, TaskStatus::SessionEnded);
            let retained = if variant == 4 {
                bounded(&text, MAX_RESULT_BYTES)
            } else {
                text.clone()
            };
            assert_eq!(ended.result.as_ref(), Some(&retained));
            assert_eq!(ended.usage, Some(usage(&checkpoint)));
            let execution = ended.execution.as_ref().unwrap();
            assert_eq!(
                execution.session_end_reason,
                Some(TaskSessionEndReason::NaturalStop)
            );
            assert_eq!(execution.recoveries, 0);
            assert_eq!(execution.checkpoint.as_ref(), Some(&checkpoint));
            assert!(!evaluate_task_completion(&ended, &text).verified());
            assert!(tasks.strategy_guidance(&ended).unwrap().is_none());
            let contract = TaskEvaluationContract {
                checks: vec![TaskReceiptCheck {
                    tool: "read_file".into(),
                    arguments: json!({"path":"curiosity.txt"}),
                    output_pointer: "/text".into(),
                    expected: json!("hello"),
                }],
                require_final_batch: true,
            };
            assert_eq!(
                evaluate_task_effect(&ended, &contract).unwrap().effect,
                TaskEffect::Inconclusive
            );
            let strategy_source = TaskStrategySource {
                task_id: ended.id.clone(),
                scope: TaskStrategyScope {
                    owner: owner(),
                    family: "curiosity".into(),
                    runtime_sha256: "a".repeat(64),
                    evaluator_version: RECEIPT_EVALUATOR_VERSION.into(),
                },
                input_sha256: task_strategy_input(&ended),
                contract: contract.clone(),
                max_steps: ended.spec.max_steps,
                observation: Some(evaluate_task_effect(&ended, &contract).unwrap()),
                extracted: false,
            };
            assert!(
                extract_task_strategy(&ended, &strategy_source)
                    .unwrap()
                    .is_none()
            );
            assert_silent(&tasks, &ended);
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            assert_eq!(tasks.get_owned(&owner(), &task.id).unwrap(), Some(ended));
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn session_end_requires_its_saved_result_exact_usage_and_identity() {
    let world = World::new(false);
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "evidence", 4, None);
    assert_eq!(
        tasks.finish(
            &task.id,
            TaskEnd::SessionEnded {
                result: String::new()
            },
            TaskUsage::default(),
            25
        ),
        Err(TaskError::InvalidTransition)
    );
    let mut checkpoint = frame(Some("实际小结"));
    let mut foreign = checkpoint.clone();
    foreign.scope.user_id = "other".into();
    assert_eq!(
        tasks.checkpoint_with_deadline(&task.id, foreign, None, 30),
        Err(TaskError::InvalidInput)
    );
    batch(
        &mut checkpoint,
        ToolOutput::Success(json!({"text":"真实读取"})),
    );
    tasks
        .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
        .unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.finish(
            &task.id,
            TaskEnd::Completed {
                result: "实际小结".into()
            },
            usage(&checkpoint),
            40
        ),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        tasks.finish(
            &task.id,
            TaskEnd::SessionEnded {
                result: "另一轮小结".into()
            },
            usage(&checkpoint),
            40
        ),
        Err(TaskError::InvalidInput)
    );
    for claimed in [
        TaskUsage::default(),
        TaskUsage {
            model_requests: 8,
            ..usage(&checkpoint)
        },
    ] {
        assert_eq!(
            tasks.finish(
                &task.id,
                TaskEnd::SessionEnded {
                    result: "实际小结".into()
                },
                claimed,
                40
            ),
            Err(TaskError::InvalidInput)
        );
    }
    assert_eq!(world.raw(), before);
    let ended = tasks
        .finish(
            &task.id,
            TaskEnd::SessionEnded {
                result: "实际小结".into(),
            },
            usage(&checkpoint),
            40,
        )
        .unwrap();
    assert_eq!(ended.status, TaskStatus::SessionEnded);
    let ordinary = tasks.create(&owner(), spec(4), "ordinary", 50).unwrap();
    tasks.start(&ordinary.id, 60).unwrap();
    assert_eq!(
        tasks.finish(
            &ordinary.id,
            TaskEnd::SessionEnded {
                result: String::new()
            },
            TaskUsage::default(),
            70
        ),
        Err(TaskError::InvalidTransition)
    );
    let completed = tasks
        .finish(
            &ordinary.id,
            TaskEnd::Completed {
                result: "旧路径保留".into(),
            },
            TaskUsage::default(),
            70,
        )
        .unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    assert!(completed.notice_pending());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn saved_final_result_ends_directly_after_restart_even_when_the_deadline_passed() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(&tasks, "saved-final", 4, Some(100));
        let mut checkpoint = frame(Some("换个兴趣也很好"));
        checkpoint.remaining = Some("原来的方向暂时不看了".into());
        checkpoint.closing = true;
        tasks
            .checkpoint(&task.id, checkpoint.clone(), 100, 30)
            .unwrap();
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let ended = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
        assert_eq!(ended.status, TaskStatus::SessionEnded);
        assert_eq!(ended.result, checkpoint.result);
        assert_eq!(ended.usage, Some(usage(&checkpoint)));
        let execution = ended.execution.as_ref().unwrap();
        assert_eq!(
            execution.session_end_reason,
            Some(TaskSessionEndReason::NaturalStop)
        );
        assert_eq!(execution.recoveries, 0);
        assert_eq!(execution.deadline_at_ms, Some(100));
        assert_eq!(execution.authorization_deadline_at_ms, Some(100));
        assert_eq!(execution.checkpoint.as_ref(), Some(&checkpoint));
        assert!(
            tasks
                .scheduling_snapshot()
                .unwrap()
                .running()
                .next()
                .is_none()
        );
        assert_silent(&tasks, &ended);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_resources_end_a_session_without_a_goal_repair_or_notice() {
    for cold in [false, true] {
        for by_deadline in [false, true] {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let deadline = by_deadline.then_some(100);
            let task = create(&tasks, "resource-stop", 1, deadline);
            let mut checkpoint = frame(None);
            if !by_deadline {
                batch(
                    &mut checkpoint,
                    ToolOutput::Success(json!({"text":"路过看到的东西"})),
                );
            }
            checkpoint.remaining = Some("还有没看完的内容，无需完成它".into());
            tasks
                .checkpoint_with_deadline(&task.id, checkpoint.clone(), deadline, 30)
                .unwrap();
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let ended = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
            assert_eq!(ended.status, TaskStatus::SessionEnded);
            assert_eq!(ended.result.as_deref(), Some(""));
            let execution = ended.execution.as_ref().unwrap();
            assert_eq!(
                execution.session_end_reason,
                Some(if by_deadline {
                    TaskSessionEndReason::DeadlineExpired
                } else {
                    TaskSessionEndReason::ToolBudgetExhausted
                })
            );
            assert_eq!(execution.checkpoint.as_ref(), Some(&checkpoint));
            assert_eq!(execution.recoveries, 0);
            assert!(execution.resume_at_ms.is_none());
            assert_silent(&tasks, &ended);
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn unknown_effects_are_retained_and_never_replayed_or_notified() {
    for cold in [false, true] {
        for pending in [false, true] {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let task = create(&tasks, "unknown-effect", 4, Some(100));
            let mut checkpoint = frame(None);
            if pending {
                checkpoint.pending_calls.push(ToolCall {
                    id: "in-flight".into(),
                    name: "run_command".into(),
                    arguments: json!({"command":"record-once"}),
                });
                checkpoint.used_tool_calls = 1;
                checkpoint.tool_batches = 1;
            } else {
                batch(
                    &mut checkpoint,
                    ToolOutput::Failure {
                        code: ToolFailureCode::ExecutionFailed,
                        message: "effect unknown".into(),
                    },
                );
            }
            tasks
                .checkpoint(&task.id, checkpoint.clone(), 100, 30)
                .unwrap();
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let interrupted = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
            assert_eq!(interrupted.status, TaskStatus::Interrupted);
            assert_eq!(
                interrupted.failure.as_deref(),
                Some("unknown_tool_effect_not_replayed")
            );
            assert_eq!(interrupted.usage, Some(usage(&checkpoint)));
            assert_eq!(
                interrupted.execution.as_ref().unwrap().checkpoint.as_ref(),
                Some(&checkpoint)
            );
            assert!(
                interrupted
                    .execution
                    .as_ref()
                    .unwrap()
                    .resume_at_ms
                    .is_none()
            );
            assert_eq!(
                tasks.retry(&task.id, 200),
                Err(TaskError::InvalidTransition)
            );
            assert_silent(&tasks, &interrupted);
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            assert_eq!(
                tasks.get_owned(&owner(), &task.id).unwrap(),
                Some(interrupted)
            );
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn failure_blocking_and_cancellation_stay_silent() {
    for cold in [false, true] {
        for variant in 0..4 {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let task = create(&tasks, "non-success", 4, None);
            let checkpoint = frame(None);
            tasks
                .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
                .unwrap();
            let (end, expected) = match variant {
                0 => (
                    TaskEnd::Failed {
                        reason: "已确认模型请求返回400".into(),
                    },
                    TaskStatus::Failed,
                ),
                1 => (
                    TaskEnd::Blocked {
                        result: "暂时无法访问这个位置".into(),
                    },
                    TaskStatus::Blocked,
                ),
                _ => {
                    tasks.cancel(&owner().user_id, &task.id, 35).unwrap();
                    (
                        TaskEnd::Failed {
                            reason: "取消与失败同时到达".into(),
                        },
                        TaskStatus::Cancelled,
                    )
                }
            };
            let ended = if variant == 3 {
                kernel.stop_all().await.unwrap();
                let (reopened, tasks) = world.open().await.unwrap();
                let ended = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
                assert_silent(&tasks, &ended);
                reopened.stop_all().await.unwrap();
                ended
            } else {
                let ended = tasks.finish(&task.id, end, usage(&checkpoint), 40).unwrap();
                assert_silent(&tasks, &ended);
                kernel.stop_all().await.unwrap();
                ended
            };
            assert_eq!(ended.status, expected);
            assert_eq!(ended.usage, Some(usage(&checkpoint)));
            let (kernel, tasks) = world.open().await.unwrap();
            if expected == TaskStatus::Blocked {
                assert_eq!(
                    tasks.cancel(&owner().user_id, &task.id, 100),
                    Err(TaskError::InvalidTransition)
                );
                assert_eq!(
                    tasks.resume_blocked(&owner(), &task.id, "另一次好奇", 100),
                    Err(TaskError::InvalidTransition)
                );
            }
            assert_eq!(tasks.get_owned(&owner(), &task.id).unwrap(), Some(ended));
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn safe_recovery_keeps_the_original_session_and_stops_when_recoveries_are_exhausted() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(&tasks, "safe-recovery", 4, None);
        let checkpoint = frame(None);
        tasks
            .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
            .unwrap();
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let resumed = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
        assert_eq!(resumed.status, TaskStatus::Running);
        assert_eq!(resumed.execution.as_ref().unwrap().recoveries, 1);
        assert!(resumed.execution.as_ref().unwrap().resume_at_ms.is_some());
        assert_eq!(
            resumed.execution.as_ref().unwrap().checkpoint.as_ref(),
            Some(&checkpoint)
        );
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let still_waiting = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
        assert_eq!(still_waiting, resumed);
        kernel.stop_all().await.unwrap();
    }

    let world = World::new(false);
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "recovery-limit", 4, None);
    let checkpoint = frame(None);
    tasks
        .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let mut ledger: Value = serde_json::from_slice(&world.raw()).unwrap();
    // 可信持久状态模拟已经使用过全部接续机会；没有重新执行任何工具。
    ledger["tasks"][0]["execution"]["recoveries"] = json!(MAX_AUTO_RECOVERIES);
    world.write(serde_json::to_vec(&ledger).unwrap());
    let (kernel, tasks) = world.open().await.unwrap();
    let ended = tasks.get_owned(&owner(), &task.id).unwrap().unwrap();
    assert_eq!(ended.status, TaskStatus::SessionEnded);
    assert_eq!(
        ended.execution.as_ref().unwrap().session_end_reason,
        Some(TaskSessionEndReason::RecoveryLimitReached)
    );
    assert_eq!(
        ended.execution.as_ref().unwrap().checkpoint.as_ref(),
        Some(&checkpoint)
    );
    assert_silent(&tasks, &ended);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn persisted_exploration_cannot_be_reinterpreted_as_task_success_or_notified() {
    let world = World::new(false);
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "tamper", 4, None);
    let checkpoint = frame(Some("停一会儿"));
    tasks
        .checkpoint_with_deadline(&task.id, checkpoint.clone(), None, 30)
        .unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::SessionEnded {
                result: "停一会儿".into(),
            },
            usage(&checkpoint),
            40,
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    let original: Value = serde_json::from_slice(&world.raw()).unwrap();
    for variant in 0..6 {
        let mut changed = original.clone();
        let task = &mut changed["tasks"][0];
        match variant {
            0 => task["status"] = json!("Completed"),
            1 => {
                task["execution"].as_object_mut().unwrap().remove("kind");
            }
            2 => task["notice"] = json!({"status":"Pending","attempts":[],"closed_at_ms":null}),
            3 => task["usage"]["model_requests"] = json!(2),
            4 => task["execution"]["exploration_acknowledged_at_ms"] = json!(1),
            _ => task["result"] = json!("未发生的经历"),
        }
        let bytes = serde_json::to_vec(&changed).unwrap();
        world.write(bytes.clone());
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(), bytes, "损坏原文必须保留");
    }
}
