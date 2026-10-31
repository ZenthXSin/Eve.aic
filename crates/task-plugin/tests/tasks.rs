use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{TaskController, TaskPlugin};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

type Mutation = Box<dyn Fn(&mut Value)>;

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
    fail_after: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-after-commit-detail".into()));
        }
        Ok(())
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
        });
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = TaskPlugin::new()?;
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(&PluginId::new(TASK_PLUGIN_ID).unwrap(), TASK_STATE_KEY)
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .inner
            .set(
                &PluginId::new(TASK_PLUGIN_ID).unwrap(),
                TASK_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}

fn owner(user: &str) -> TaskOwner {
    TaskOwner {
        session_id: format!("qq:session-{user}"),
        user_id: user.into(),
    }
}

fn spec(title: &str) -> TaskSpec {
    TaskSpec {
        title: title.into(),
        instructions: format!("{title}：读取资料并给出结论"),
        max_steps: DEFAULT_STEPS,
        context: None,
    }
}

fn usage(calls: u32) -> TaskUsage {
    TaskUsage {
        tool_calls: calls,
        model_requests: calls + 1,
    }
}

fn sent() -> DeliveryResult {
    DeliveryResult::Sent {
        platform_message_id: Some("out-1".into()),
    }
}

fn checkpoint(user: &str) -> eve_llm_api::TurnCheckpoint {
    let owner = owner(user);
    eve_llm_api::TurnCheckpoint {
        version: 1,
        scope: eve_llm_api::ContextScope {
            session_id: owner.session_id,
            user_id: owner.user_id,
        },
        messages: vec![eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "执行任务",
        )],
        pending_calls: vec![],
        used_tool_calls: 0,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 0,
        remaining: Some("核验剩余步骤".into()),
        continuation_checks: 0,
        closing: false,
        result: None,
    }
}

#[tokio::test]
async fn checkpoints_resume_with_a_bounded_retry_count_and_preserve_the_deadline() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("检查点"), "resume", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .checkpoint(&task.id, checkpoint("alice"), u64::MAX, 30)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Running);
    let execution = saved.execution.unwrap();
    assert_eq!(execution.recoveries, 1);
    assert!(execution.resume_at_ms.is_some());
    assert_eq!(execution.deadline_at_ms, Some(u64::MAX));
    assert_eq!(execution.checkpoint, Some(checkpoint("alice")));
    let mut frame = checkpoint("alice");
    for expected in 2..=MAX_AUTO_RECOVERIES {
        frame.provider_requests += 1;
        tasks
            .checkpoint(&task.id, frame.clone(), u64::MAX, 40)
            .unwrap();
        assert_eq!(
            tasks
                .retry(&task.id, 50)
                .unwrap()
                .execution
                .unwrap()
                .recoveries,
            expected
        );
    }
    frame.provider_requests += 1;
    tasks.checkpoint(&task.id, frame, u64::MAX, 60).unwrap();
    let before = world.raw();
    assert_eq!(tasks.retry(&task.id, 70), Err(TaskError::InvalidTransition));
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.snapshot().unwrap().get(&task.id).unwrap().status,
        TaskStatus::Interrupted
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn a_saved_final_result_is_settled_after_restart_without_a_second_execution() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("最终结果"), "final", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.remaining = None;
    frame.result = Some("已经完成且验证".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Completed);
    assert_eq!(saved.result.as_deref(), Some("已经完成且验证"));
    assert_eq!(saved.usage, Some(usage(0))); // 一个模型请求，没有工具。
    assert!(saved.notice_pending());
    assert_eq!(saved.execution.unwrap().recoveries, 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn blocked_result_survives_restart_and_is_never_reexecuted_or_counted_completed() {
    use eve_llm_api::{ChatMessage, ToolCall, ToolOutput, ToolResult};
    use serde_json::json;
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("待用户选择"), "blocked", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    let args = json!({"status":"blocked","remaining":"已经读完配置；部署到哪个环境？"});
    frame.messages.push(
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "question".into(),
            name: "task_progress".into(),
            arguments: args.clone(),
        }])
        .unwrap(),
    );
    frame.messages.push(
        ChatMessage::tool_results(vec![ToolResult {
            call_id: "question".into(),
            output: ToolOutput::Success(args),
        }])
        .unwrap(),
    );
    frame.used_tool_calls = 1;
    frame.tool_batches = 1;
    frame.provider_requests = 2;
    frame.remaining = None;
    frame.result = Some("已核对配置，部署到哪个环境？".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Blocked);
    assert_eq!(
        saved.result.as_deref(),
        Some("已核对配置，部署到哪个环境？")
    );
    assert!(saved.notice_pending());
    assert!(tasks.snapshot().unwrap().queued().is_empty());
    assert_eq!(saved.execution.unwrap().recoveries, 0);
    let bytes = world.raw().unwrap();
    let wire: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(wire["format_version"], 3);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.snapshot().unwrap().get(&task.id).unwrap().status,
        TaskStatus::Blocked
    );
    kernel.stop_all().await.unwrap();
    let mut old = wire;
    old["format_version"] = json!(2);
    world.write(serde_json::to_vec(&old).unwrap());
    assert!(world.open().await.is_err(), "旧格式不能伪装携带新状态");
}

#[tokio::test]
async fn explicit_blocked_finish_persists_progress_and_pending_notice() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待选择"), "blocked-finish", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let saved = tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "已检查；保留哪份文件？".into(),
            },
            usage(1),
            30,
        )
        .unwrap();
    assert_eq!(saved.status, TaskStatus::Blocked);
    assert!(saved.notice_pending());
    assert_eq!(saved.result.as_deref(), Some("已检查；保留哪份文件？"));
    assert!(saved.failure.is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unfinished_recovery_result_still_requires_budget_approval_after_restart() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待预算"), "failure:budget", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.closing = true;
    frame.result = Some("已有进度，尚未完成".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Failed);
    assert!(saved.result.is_none());
    assert!(saved.needs_step_approval());
    assert!(saved.notice_pending());
    assert_eq!(saved.execution.unwrap().recoveries, 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_or_cross_scope_checkpoints_never_replace_the_saved_bytes() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("校验"), "strict", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.checkpoint(&task.id, checkpoint("bob"), u64::MAX, 30),
        Err(TaskError::InvalidInput)
    );
    let mut bad = checkpoint("alice");
    bad.version = 999;
    assert_eq!(
        tasks.checkpoint(&task.id, bad, u64::MAX, 30),
        Err(TaskError::InvalidInput)
    );
    assert_eq!(world.raw(), before);
    tasks
        .checkpoint(&task.id, checkpoint("alice"), u64::MAX, 30)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let good: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    for mutate in [0, 1, 2] {
        let mut bad = good.clone();
        match mutate {
            0 => bad["tasks"][0]["execution"]["checkpoint"]["scope"]["user_id"] = "bob".into(),
            1 => bad["tasks"][0]["execution"]["checkpoint"]["version"] = 999.into(),
            _ => bad["tasks"][0]["execution"]["checkpoint"]["used_tool_calls"] = 51.into(),
        }
        let bytes = serde_json::to_vec(&bad).unwrap();
        world.write(bytes.clone());
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(), Some(bytes));
    }
}

#[tokio::test]
async fn tasks_run_once_finish_with_a_notice_and_replays_return_the_original() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let created = tasks
        .create(&alice, spec("整理日志"), "call-1", 100)
        .unwrap();
    assert_eq!(created.status, TaskStatus::Queued);
    assert!(created.id.starts_with("task-"));
    // 同一次派发重复执行：返回原记录，不另建。
    let replay = tasks
        .create(&alice, spec("另一个标题"), "call-1", 200)
        .unwrap();
    assert!(replay == created);
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    assert_eq!(tasks.snapshot().unwrap().queued()[0].id, created.id);

    let running = tasks.start(&created.id, 300).unwrap();
    assert_eq!(running.status, TaskStatus::Running);
    assert_eq!(
        tasks.start(&created.id, 301).unwrap_err(),
        TaskError::InvalidTransition
    );
    // 用量不能超过步数预算；没有取消请求时不能以取消结束。
    assert_eq!(
        tasks
            .finish(
                &created.id,
                TaskEnd::Completed {
                    result: "好".into()
                },
                usage(DEFAULT_STEPS + 1),
                400
            )
            .unwrap_err(),
        TaskError::InvalidInput
    );
    assert_eq!(
        tasks
            .finish(&created.id, TaskEnd::Cancelled, usage(1), 400)
            .unwrap_err(),
        TaskError::InvalidTransition
    );
    let long = "结论".repeat(MAX_RESULT_BYTES);
    let done = tasks
        .finish(
            &created.id,
            TaskEnd::Completed { result: long },
            usage(7),
            400,
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    let result = done.result.clone().unwrap();
    assert!(result.len() <= MAX_RESULT_BYTES && result.ends_with("（已截断）"));
    assert_eq!(done.usage, Some(usage(7)));
    assert!(done.notice_pending());

    // 主动私聊至多一次；没写出时回到等待，随下一条私聊回复附带。
    let snapshot = tasks.snapshot().unwrap();
    assert_eq!(snapshot.due_proactive().unwrap().id, created.id);
    tasks
        .claim(&created.id, DeliveryChannel::Proactive, 500)
        .unwrap();
    assert!(
        tasks
            .snapshot()
            .unwrap()
            .due_passive("alice", "m1")
            .is_none()
    );
    tasks
        .record_delivery(&created.id, DeliveryResult::NotSent, 501)
        .unwrap();
    let snapshot = tasks.snapshot().unwrap();
    assert!(snapshot.due_proactive().is_none());
    assert_eq!(snapshot.due_passive("alice", "m1").unwrap().id, created.id);
    assert!(snapshot.due_passive("bob", "m1").is_none());
    tasks
        .claim(
            &created.id,
            DeliveryChannel::Passive {
                message_id: "m1".into(),
            },
            502,
        )
        .unwrap();
    let delivered = tasks.record_delivery(&created.id, sent(), 503).unwrap();
    assert_eq!(
        delivered.notice.as_ref().unwrap().status,
        NoticeStatus::Delivered
    );
    assert_eq!(delivered.settled_at(), Some(503));
    kernel.stop_all().await.unwrap();

    let (kernel, tasks) = world.open().await.unwrap();
    assert!(tasks.snapshot().unwrap().tasks[0] == delivered);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn owners_are_isolated_and_limits_refuse_without_writing() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    for index in 0..MAX_ACTIVE_PER_OWNER {
        tasks
            .create(&alice, spec("并行任务"), &format!("active-{index}"), 100)
            .unwrap();
    }
    let before = world.raw();
    assert_eq!(
        tasks
            .create(&alice, spec("第五个"), "active-extra", 100)
            .unwrap_err(),
        TaskError::LimitReached
    );
    assert_eq!(world.raw(), before, "拒绝时不写状态");
    // 别人的任务如同不存在；自己的排队任务直接取消。
    let id = tasks.snapshot().unwrap().tasks[0].id.clone();
    assert_eq!(
        tasks.cancel("bob", &id, 150).unwrap_err(),
        TaskError::NotFound
    );
    let cancelled = tasks.cancel("alice", &id, 150).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.notice.is_none());
    assert_eq!(
        tasks.cancel("alice", &id, 151).unwrap_err(),
        TaskError::InvalidTransition
    );
    // 24 小时内的派发数另有上限，结束了的也计入。
    let bob = owner("bob");
    for index in 0..MAX_CREATED_PER_OWNER_PER_DAY {
        let task = tasks
            .create(&bob, spec("短任务"), &format!("daily-{index}"), 1_000)
            .unwrap();
        tasks.cancel("bob", &task.id, 1_001).unwrap();
    }
    assert_eq!(
        tasks
            .create(&bob, spec("超出"), "daily-extra", 1_002)
            .unwrap_err(),
        TaskError::LimitReached
    );
    assert!(
        tasks
            .create(&bob, spec("第二天"), "next-day", 1_000 + DAY_MS)
            .is_ok()
    );
    // 无效输入不写入。
    let mut bad = spec("坏");
    bad.max_steps = 0;
    assert_eq!(
        tasks.create(&bob, bad, "bad", 2_000).unwrap_err(),
        TaskError::InvalidInput
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn running_cancellation_is_requested_then_confirmed_by_the_executor() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("长任务"), "c", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let requested = tasks.cancel("alice", &task.id, 30).unwrap();
    assert_eq!(requested.status, TaskStatus::Running);
    assert!(requested.cancel_requested);
    assert_eq!(
        tasks.cancel("alice", &task.id, 31).unwrap_err(),
        TaskError::InvalidTransition
    );
    let ended = tasks
        .finish(&task.id, TaskEnd::Cancelled, usage(3), 40)
        .unwrap();
    assert_eq!(ended.status, TaskStatus::Cancelled);
    assert!(ended.notice.is_none(), "用户取消的任务不再通知");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancellation_wins_over_an_error_arriving_at_the_same_time() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("取消竞态"), "cancel-race", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks.cancel("alice", &task.id, 30).unwrap();
    let saved = tasks
        .finish(
            &task.id,
            TaskEnd::Failed {
                reason: "模型已返回错误".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    assert_eq!(saved.status, TaskStatus::Cancelled);
    assert!(saved.notice.is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_running_tasks_and_marks_in_flight_notices_unknown() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let running = tasks.create(&alice, spec("执行中"), "run", 10).unwrap();
    tasks.start(&running.id, 20).unwrap();
    let cancelling = tasks.create(&alice, spec("取消中"), "cancel", 10).unwrap();
    tasks.start(&cancelling.id, 20).unwrap();
    tasks.cancel("alice", &cancelling.id, 25).unwrap();
    let failed = tasks.create(&alice, spec("会失败"), "fail", 10).unwrap();
    tasks.start(&failed.id, 20).unwrap();
    tasks
        .finish(
            &failed.id,
            TaskEnd::Failed {
                reason: "模型不可用".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    tasks
        .claim(&failed.id, DeliveryChannel::Proactive, 40)
        .unwrap();
    let queued = tasks.create(&owner("bob"), spec("排队"), "q", 10).unwrap();
    kernel.stop_all().await.unwrap();
    // 迁移前的任务没有执行日志，继续保留“不猜测、不自动重跑”的旧语义。
    let mut old: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    old["tasks"][0].as_object_mut().unwrap().remove("execution");
    world.write(serde_json::to_vec(&old).unwrap());

    let (kernel, tasks) = world.open().await.unwrap();
    let snapshot = tasks.snapshot().unwrap();
    let interrupted = snapshot.get(&running.id).unwrap();
    assert_eq!(interrupted.status, TaskStatus::Interrupted);
    assert!(interrupted.notice_pending(), "中断也要告诉用户");
    let cancelled = snapshot.get(&cancelling.id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.notice.is_none());
    let unknown = snapshot.get(&failed.id).unwrap();
    assert_eq!(
        unknown.notice.as_ref().unwrap().status,
        NoticeStatus::Unknown
    );
    // 结果未知不重发，也不随回复附带。
    assert!(
        snapshot
            .due_proactive()
            .is_some_and(|task| task.id == running.id)
    );
    assert_eq!(snapshot.get(&queued.id).unwrap().status, TaskStatus::Queued);
    assert_eq!(snapshot.queued().len(), 1, "排队任务重启后照常等待执行");
    kernel.stop_all().await.unwrap();

    // 再次重启不再改写。
    let before = world.raw();
    let (kernel, _) = world.open().await.unwrap();
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_unknown_or_inconsistent_ledgers_refuse_to_open_and_keep_bytes() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("任务"), "c", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "完成".into(),
            },
            usage(2),
            30,
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    let good: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let mutations: Vec<Mutation> = vec![
        Box::new(|v| v["format_version"] = 999.into()),
        Box::new(|v| v["tasks"][0]["id"] = "task-forged".into()),
        Box::new(|v| v["tasks"][0]["result"] = Value::Null),
        Box::new(|v| v["tasks"][0]["usage"]["tool_calls"] = 9_999.into()),
        Box::new(|v| v["tasks"][0]["notice"] = Value::Null),
        Box::new(|v| v["tasks"][0]["extra"] = true.into()),
        Box::new(|v| v["tasks"][0]["spec"]["max_steps"] = 0.into()),
        Box::new(|v| v["tasks"][0]["started_at_ms"] = 1.into()),
    ];
    for mutate in mutations {
        let mut value = good.clone();
        mutate(&mut value);
        let bytes = serde_json::to_vec(&value).unwrap();
        world.write(bytes.clone());
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(), Some(bytes), "拒绝打开时保留原字节");
    }
    let mut duplicate = serde_json::to_vec(&good).unwrap();
    let text = String::from_utf8(duplicate.clone()).unwrap();
    duplicate = text
        .replacen(
            "\"format_version\":2",
            "\"format_version\":2,\"format_version\":2",
            1,
        )
        .into_bytes();
    world.write(duplicate);
    assert!(world.open().await.is_err());
}

#[tokio::test]
async fn failed_writes_close_the_instance_instead_of_trusting_the_cache() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .create(&owner("alice"), spec("任务"), "c", 10)
            .unwrap_err(),
        TaskError::Storage
    );
    assert_eq!(tasks.snapshot().unwrap_err(), TaskError::Unavailable);
    world.state.fail.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lost_create_acknowledgement_reopens_committed_task_without_duplicate_creation() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    world.state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .create(&alice, spec("已写入但确认丢失"), "lost-create", 10)
            .unwrap_err(),
        TaskError::Storage
    );
    assert_eq!(tasks.snapshot().unwrap_err(), TaskError::Unavailable);
    world.state.fail_after.store(false, Ordering::SeqCst);
    let committed = world.raw().unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let recovered = tasks.snapshot().unwrap().tasks.pop().unwrap();
    assert_eq!(recovered.status, TaskStatus::Queued);
    assert!(
        tasks
            .create(&alice, spec("不得覆盖原任务"), "lost-create", 20)
            .unwrap()
            == recovered
    );
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    assert_eq!(world.raw().unwrap(), committed);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lost_finish_acknowledgement_reopens_completed_result_and_never_rewinds_it() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec("确认丢失的结局"), "lost-finish", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    world.state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "已有真实回执".into()
                },
                usage(1),
                30
            )
            .unwrap_err(),
        TaskError::Storage
    );
    assert_eq!(
        tasks.get_owned(&alice, &task.id).unwrap_err(),
        TaskError::Unavailable
    );
    world.state.fail_after.store(false, Ordering::SeqCst);
    let committed = world.raw().unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let recovered = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
    assert_eq!(recovered.status, TaskStatus::Completed);
    assert_eq!(recovered.result.as_deref(), Some("已有真实回执"));
    assert_eq!(recovered.usage, Some(usage(1)));
    assert_eq!(
        tasks.start(&task.id, 40).unwrap_err(),
        TaskError::InvalidTransition
    );
    assert_eq!(world.raw().unwrap(), committed);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn notices_expire_after_the_window_and_full_ledgers_prune_only_settled_tasks() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("旧任务"), "old", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Failed {
                reason: "超时".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    assert_eq!(
        tasks.expire(&task.id, 30 + DELIVERY_WINDOW_MS).unwrap_err(),
        TaskError::InvalidTransition
    );
    assert_eq!(
        tasks
            .snapshot()
            .unwrap()
            .overdue(31 + DELIVERY_WINDOW_MS)
            .count(),
        1
    );
    let expired = tasks.expire(&task.id, 31 + DELIVERY_WINDOW_MS).unwrap();
    assert_eq!(expired.notice.unwrap().status, NoticeStatus::Expired);
    // 填满账本：每位用户一个已取消的任务；再派发时腾出最早了结的一条。
    let mut now = 100 + DELIVERY_WINDOW_MS;
    for index in 1..MAX_TASKS {
        let user = format!("user-{index}");
        let created = tasks
            .create(&owner(&user), spec("填充"), "fill", now)
            .unwrap();
        tasks.cancel(&user, &created.id, now).unwrap();
        now += 1;
    }
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), MAX_TASKS);
    tasks
        .create(&owner("newcomer"), spec("新任务"), "new", now)
        .unwrap();
    let snapshot = tasks.snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), MAX_TASKS);
    assert!(snapshot.get(&task.id).is_none(), "最早了结的一条被腾出");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_keeps_the_original_task_checkpoint_budget_and_deadline() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待构建"), "condition-resume", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.result = Some("构建尚未完成".into());
    frame.remaining = None;
    tasks.checkpoint(&task.id, frame.clone(), 1000, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "构建尚未完成".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    assert_eq!(
        tasks.resume_blocked(&owner("bob"), &task.id, "构建退出", 50),
        Err(TaskError::NotFound)
    );
    let resumed = tasks
        .resume_blocked(&owner("alice"), &task.id, "本地构建已退出，只检查日志", 50)
        .unwrap();
    assert_eq!(resumed.id, task.id);
    assert_eq!(resumed.spec.max_steps, task.spec.max_steps);
    assert_eq!(resumed.status, TaskStatus::Running);
    let execution = resumed.execution.unwrap();
    assert_eq!(execution.deadline_at_ms, Some(1000));
    assert_eq!(execution.resume_at_ms, Some(50));
    assert_eq!(execution.recoveries, 1);
    assert_eq!(
        execution.checkpoint.as_ref().unwrap().used_tool_calls,
        frame.used_tool_calls
    );
    assert_eq!(
        execution.checkpoint.as_ref().unwrap().provider_requests,
        frame.provider_requests
    );
    assert_eq!(execution.checkpoint.as_ref().unwrap().result, None);
    assert!(execution.checkpoint.as_ref().unwrap().messages.len() > frame.messages.len());
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "重复事件", 60),
        Err(TaskError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let snapshot = tasks.snapshot().unwrap();
    let restored = snapshot.get(&task.id).unwrap();
    assert_eq!(restored.id, task.id);
    assert_eq!(
        restored.execution.as_ref().unwrap().deadline_at_ms,
        Some(1000)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_rejects_unknown_side_effects_and_expired_deadlines() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    for (source, unknown, now) in [("unknown", true, 50), ("expired", false, 1000)] {
        let task = tasks
            .create(&owner("alice"), spec("保留现场"), source, 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut frame = checkpoint("alice");
        if unknown {
            frame.pending_calls.push(eve_llm_api::ToolCall {
                id: "unknown".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({}),
            });
            frame.used_tool_calls = 1;
            frame.tool_batches = 1;
        }
        tasks.checkpoint(&task.id, frame, 1000, 30).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "等待核对".into(),
                },
                usage(0),
                40,
            )
            .unwrap();
        let before = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
        assert_eq!(
            tasks.resume_blocked(&owner("alice"), &task.id, "条件成立", now),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(tasks.snapshot().unwrap().get(&task.id).unwrap(), &before);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_blocked_task_prevents_future_condition_resume() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待后撤回"), "cancel-blocked", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.remaining = None;
    frame.result = Some("等待条件".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待条件".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    let cancelled = tasks.cancel("alice", &task.id, 50).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "旧条件满足", 60),
        Err(TaskError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.snapshot().unwrap().get(&task.id).unwrap().status,
        TaskStatus::Cancelled
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn bounded_create_keeps_original_authorization_deadline_atomically_across_restart() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let task = admin
        .create_bounded(&owner("a"), spec("原授权目标"), "bounded", 1000, 10)
        .unwrap();
    assert_eq!(
        task.execution
            .as_ref()
            .unwrap()
            .authorization_deadline_at_ms,
        Some(1000)
    );
    let duplicate = admin
        .create_bounded(&owner("a"), spec("不能刷新期限"), "bounded", 2000, 20)
        .unwrap();
    assert_eq!(
        duplicate
            .execution
            .as_ref()
            .unwrap()
            .authorization_deadline_at_ms,
        Some(1000)
    );
    assert!(
        admin
            .create_bounded(&owner("a"), spec("已过期"), "expired", 30, 30)
            .is_err()
    );
    let mut foreign = owner("a");
    foreign.session_id = "other-window".into();
    assert!(
        admin
            .create_bounded(&foreign, spec("不能跨范围"), "bounded", 2000, 20)
            .is_err()
    );
    admin.start(&task.id, 21).unwrap();
    assert!(
        admin
            .checkpoint(&task.id, checkpoint("a"), 2000, 22)
            .is_err()
    );
    admin
        .checkpoint(&task.id, checkpoint("a"), 1000, 22)
        .unwrap();
    kernel.stop_all().await.unwrap();
    drop(kernel);
    let (_kernel, restored) = world.open().await.unwrap();
    assert_eq!(
        restored
            .snapshot()
            .unwrap()
            .get(&task.id)
            .unwrap()
            .execution
            .as_ref()
            .unwrap()
            .authorization_deadline_at_ms,
        Some(1000)
    );
}

#[tokio::test]
async fn oversized_escaped_context_keeps_committed_tasks_and_handle_usable_after_rejection() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let large = |owner: &TaskOwner| {
        let mut task = spec("保留完整上下文");
        task.context = Some(DelegatedContext {
            owner: owner.clone(),
            messages_json: serde_json::to_string(&vec![eve_llm_api::ChatMessage::text(
                eve_llm_api::ChatRole::User,
                "\\\"".repeat(300_000),
            )])
            .unwrap(),
        });
        task
    };
    let alice = owner("large-alice");
    let original = admin
        .create(&alice, large(&alice), "first-large", 100)
        .unwrap();
    let before = world.raw().unwrap();
    let bob = owner("large-bob");
    assert!(matches!(
        admin.create(&bob, large(&bob), "rejected-large", 101),
        Err(TaskError::LimitReached)
    ));
    assert_eq!(world.raw().unwrap(), before);
    assert!(admin.get_owned(&alice, &original.id).unwrap() == Some(original.clone()));
    let small = admin
        .create(&bob, spec("另一个小任务"), "small", 102)
        .unwrap();
    admin.start(&small.id, 103).unwrap();
    admin
        .finish(
            &small.id,
            TaskEnd::Completed {
                result: "已核对".into(),
            },
            usage(0),
            104,
        )
        .unwrap();
    assert!(admin.get_owned(&alice, &original.id).unwrap() == Some(original.clone()));
    kernel.stop_all().await.unwrap();
    let (kernel, restored) = world.open().await.unwrap();
    assert!(restored.get_owned(&alice, &original.id).unwrap() == Some(original));
    assert_eq!(
        restored.get_owned(&bob, &small.id).unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(restored.summaries_for(&bob).unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn scoped_task_directory_omits_heavy_context_and_full_reads_keep_both_identity_fields() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let alice = owner("alice");
    let mut heavy = spec("自己的任务");
    heavy.context = Some(DelegatedContext {
        owner: alice.clone(),
        messages_json: serde_json::to_string(&vec![eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "CONFIDENTIAL_CONTEXT_秘密".repeat(10_000),
        )])
        .unwrap(),
    });
    let own = admin.create(&alice, heavy, "own", 100).unwrap();
    let other_session = TaskOwner {
        session_id: "another-session".into(),
        ..alice.clone()
    };
    let other_user = TaskOwner {
        user_id: "bob".into(),
        ..alice.clone()
    };
    let foreign = admin
        .create(&other_session, spec("另一个会话"), "other-session", 101)
        .unwrap();
    admin
        .create(&other_user, spec("另一个用户"), "other-user", 102)
        .unwrap();
    let before = world.raw();
    let summaries = admin.summaries_for(&alice).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, own.id);
    assert_eq!(summaries[0].instructions, own.spec.instructions);
    let encoded = serde_json::to_string(&summaries).unwrap();
    assert!(!encoded.contains("CONFIDENTIAL_CONTEXT"));
    assert!(encoded.len() < 1024);
    let body = admin.get_owned(&alice, &own.id).unwrap().unwrap();
    assert!(body == own);
    assert!(admin.get_owned(&alice, &foreign.id).unwrap().is_none());
    assert!(admin.get_owned(&other_session, &own.id).unwrap().is_none());
    assert!(admin.get_owned(&other_user, &own.id).unwrap().is_none());
    assert!(admin.get_owned(&alice, "unknown").unwrap().is_none());
    let invalid = TaskOwner {
        user_id: String::new(),
        ..alice.clone()
    };
    assert_eq!(admin.summaries_for(&invalid), Err(TaskError::InvalidInput));
    assert!(matches!(
        admin.get_owned(&invalid, &own.id),
        Err(TaskError::InvalidInput)
    ));
    assert!(matches!(
        admin.get_owned(&alice, ""),
        Err(TaskError::InvalidInput)
    ));
    assert_eq!(
        world.raw(),
        before,
        "只读目录、正文与错误不能写状态或新建任务"
    );
    let old = admin.clone();
    kernel.stop_all().await.unwrap();
    assert_eq!(old.summaries_for(&alice), Err(TaskError::Unavailable));
    assert!(matches!(
        old.get_owned(&alice, &own.id),
        Err(TaskError::Unavailable)
    ));
    let (kernel, restored) = world.open().await.unwrap();
    assert_eq!(restored.summaries_for(&alice).unwrap(), summaries);
    assert!(restored.get_owned(&alice, &own.id).unwrap().unwrap() == own);
    assert!(matches!(
        old.get_owned(&alice, &own.id),
        Err(TaskError::Unavailable)
    ));
    kernel.stop_all().await.unwrap();
}
