//! 冻结输入测试的历史见证可复用，但模型文字、未知副作用和跨范围回执不能验收。
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, ModelSession, ToolCall, ToolOutput, ToolResult,
    TurnCheckpoint,
};
use eve_plugin_api::{PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{
    TaskController, TaskPlugin, evaluate_checkpoint_completion, evaluate_completion_contract,
    evaluate_task_completion,
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const MAX_STEPS: u32 = 8;

/// 测试中的可信运行器账本。只返回显式记录的见证，不从工具 JSON 推导成功。
#[derive(Default)]
struct EvidenceSource {
    evidence: Mutex<Vec<TaskFrozenNodeEvidence>>,
    fail: AtomicBool,
    captures: AtomicUsize,
}
impl EvidenceSource {
    fn record(&self, evidence: TaskFrozenNodeEvidence) {
        self.evidence.lock().unwrap().push(evidence);
    }
}
impl TaskCompletionEvidenceSource for EvidenceSource {
    fn capture(
        &self,
        _: &TaskOwner,
        _: &TurnCheckpoint,
    ) -> TaskResult<Vec<TaskFrozenNodeEvidence>> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(TaskError::Unavailable)
        } else {
            Ok(self.evidence.lock().unwrap().clone())
        }
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<dyn StateStore>,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(FileStateStore::open(directory.path().join("state")).unwrap());
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(
        &self,
        source: Option<Arc<dyn TaskCompletionEvidenceSource>>,
    ) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let mut plugin = TaskPlugin::new()?;
        if let Some(source) = source {
            plugin = plugin.with_completion_evidence_source(source);
        }
        let tasks = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, tasks))
    }
}

fn owner() -> TaskOwner {
    TaskOwner {
        session_id: "frozen-node-session".into(),
        user_id: "alice".into(),
    }
}
fn spec() -> TaskSpec {
    TaskSpec {
        title: "验证冻结的纯 JS 输入".into(),
        instructions: "按冻结合同验证，采用已有真实见证".into(),
        max_steps: MAX_STEPS,
        context: None,
    }
}
fn digest(label: &str) -> String {
    task_strategy_digest(&json!(label))
}
fn contract() -> TaskCompletionContract {
    TaskCompletionContract::FrozenNodeTest {
        input_sha256: digest("input-v1"),
        runtime_sha256: digest("runtime-v1"),
        command_sha256: digest("command-v1"),
    }
}
fn frame(anchor: &str) -> TurnCheckpoint {
    let owner = owner();
    let scope = ContextScope {
        session_id: owner.session_id,
        user_id: owner.user_id,
    };
    TurnCheckpoint {
        model_session: Some(
            ModelSession::anchored("task", Some(&scope), anchor)
                .unwrap()
                .with_turn(anchor)
                .unwrap(),
        ),
        scope,
        pending_model_request: None,
        pending_request_fingerprint: None,
        version: 1,
        messages: vec![ChatMessage::text(ChatRole::User, "请验证冻结输入")],
        pending_calls: vec![],
        used_tool_calls: 0,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 0,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: Some("冻结输入测试通过；未验证活工作区或交付。".into()),
    }
}
fn batch(frame: &mut TurnCheckpoint, call: ToolCall, output: ToolOutput) -> ToolResult {
    let receipt = ToolResult {
        call_id: call.id.clone(),
        output,
    };
    frame
        .messages
        .push(ChatMessage::assistant_tool_calls(vec![call]).unwrap());
    frame
        .messages
        .push(ChatMessage::tool_results(vec![receipt.clone()]).unwrap());
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
    receipt
}
fn test_receipt(
    frame: &mut TurnCheckpoint,
    id: &str,
    outcome: TaskFrozenNodeOutcome,
) -> TaskFrozenNodeEvidence {
    let arguments = json!({"bundle": "frozen-fixture", "command": "node --test"});
    let receipt = batch(
        frame,
        ToolCall {
            id: id.into(),
            name: FROZEN_NODE_TEST_TOOL.into(),
            arguments: arguments.clone(),
        },
        // 这些可见字段不充当真实见证；失败/未知用例会保留相同的“成功” JSON。
        ToolOutput::Success(json!({"exit_code": 0, "verified": true})),
    );
    TaskFrozenNodeEvidence {
        owner: owner(),
        model_session: frame.model_session.clone().unwrap(),
        call_id: id.into(),
        arguments_sha256: task_strategy_digest(&arguments),
        tool_result_sha256: task_strategy_digest(&receipt),
        input_sha256: digest("input-v1"),
        runtime_sha256: digest("runtime-v1"),
        command_sha256: digest("command-v1"),
        outcome,
    }
}
fn success(frame: &mut TurnCheckpoint, id: &str) -> TaskFrozenNodeEvidence {
    test_receipt(
        frame,
        id,
        TaskFrozenNodeOutcome::Exited {
            exit_code: Some(0),
            signal: None,
        },
    )
}
fn read_log(frame: &mut TurnCheckpoint) {
    batch(
        frame,
        ToolCall {
            id: "read-log".into(),
            name: "read_file".into(),
            arguments: json!({"path": "test.log"}),
        },
        ToolOutput::Success(json!({"text": "已读取测试日志"})),
    );
}
fn assess(
    contract: &TaskCompletionContract,
    frame: &TurnCheckpoint,
    evidence: &[TaskFrozenNodeEvidence],
) -> TaskCompletionAssessment {
    evaluate_checkpoint_completion(
        contract,
        &owner(),
        frame,
        MAX_STEPS as usize,
        frame.result.as_deref().unwrap_or("声称完成"),
        evidence,
    )
}
fn create(tasks: &TaskController, source: &str, contract: TaskCompletionContract) -> Task {
    let task = tasks
        .create_authorized(
            &owner(),
            spec(),
            source,
            vec!["请验证冻结输入".into()],
            Some(contract),
            Some(u64::MAX),
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap()
}
fn saved(tasks: &TaskController, id: &str) -> Task {
    tasks.snapshot().unwrap().get(id).unwrap().clone()
}
fn save(tasks: &TaskController, task: &Task, frame: &TurnCheckpoint, now: u64) -> Task {
    frame.validate(MAX_STEPS as usize).unwrap();
    tasks
        .checkpoint(&task.id, frame.clone(), u64::MAX, now)
        .unwrap()
}
fn finish(tasks: &TaskController, task: &Task, frame: &TurnCheckpoint) -> Task {
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: frame.result.clone().unwrap(),
            },
            TaskUsage {
                tool_calls: frame.used_tool_calls as u32,
                model_requests: frame.provider_requests as u32,
            },
            50,
        )
        .unwrap()
}

#[test]
fn successful_frozen_test_survives_log_and_progress_without_changing_legacy_contract() {
    let mut frame = frame("reuse");
    let witness = success(&mut frame, "test");
    let legacy = TaskCompletionContract::Receipts {
        contract: TaskEvaluationContract {
            checks: vec![TaskReceiptCheck {
                tool: FROZEN_NODE_TEST_TOOL.into(),
                arguments: frame.messages[1].tool_calls[0].arguments.clone(),
                output_pointer: "/exit_code".into(),
                expected: json!(0),
            }],
            require_final_batch: true,
        },
    };
    assert!(assess(&legacy, &frame, &[]).verified());
    read_log(&mut frame);
    assert!(assess(&contract(), &frame, std::slice::from_ref(&witness)).verified());
    assert!(!assess(&legacy, &frame, &[]).verified());
    batch(
        &mut frame,
        ToolCall {
            id: "progress".into(),
            name: "task_progress".into(),
            arguments: json!({"status": "completed", "summary": "已整理结果"}),
        },
        ToolOutput::Success(json!({"status": "completed", "summary": "已整理结果"})),
    );
    assert!(assess(&contract(), &frame, &[witness]).verified());
    assert!(!assess(&legacy, &frame, &[]).verified());
    // 旧的无 sidecar 入口明确拒绝新合同，不能仅凭可见 JSON 升级完成状态。
    assert!(matches!(
        evaluate_completion_contract(
            &contract(),
            &frame.messages,
            frame.result.as_deref().unwrap(),
            None,
        ),
        TaskCompletionAssessment::Unverifiable { .. }
    ));
}

#[test]
fn changed_target_latest_failure_and_json_without_trusted_sidecar_do_not_pass() {
    let mut frame = frame("targets");
    let witness = success(&mut frame, "test-1");
    assert!(matches!(
        assess(&contract(), &frame, &[]),
        TaskCompletionAssessment::Unverifiable { .. }
    ));
    for field in ["input_sha256", "runtime_sha256", "command_sha256"] {
        let mut wire = serde_json::to_value(contract()).unwrap();
        wire[field] = json!(digest("changed-target"));
        let changed = serde_json::from_value(wire).unwrap();
        assert!(!assess(&changed, &frame, std::slice::from_ref(&witness)).verified());
    }
    for outcome in [
        TaskFrozenNodeOutcome::Exited {
            exit_code: Some(1),
            signal: None,
        },
        TaskFrozenNodeOutcome::Exited {
            exit_code: None,
            signal: Some(9),
        },
        TaskFrozenNodeOutcome::Cancelled,
        TaskFrozenNodeOutcome::Unknown,
    ] {
        let mut later = frame.clone();
        let newest = test_receipt(&mut later, "test-2", outcome.clone());
        assert!(!assess(&contract(), &later, std::slice::from_ref(&witness)).verified());
        let assessment = assess(&contract(), &later, &[witness.clone(), newest]);
        if matches!(outcome, TaskFrozenNodeOutcome::Exited { .. }) {
            assert!(matches!(
                assessment,
                TaskCompletionAssessment::Mismatch { .. }
            ));
        } else {
            assert!(matches!(
                assessment,
                TaskCompletionAssessment::Unverifiable { .. }
            ));
        }
    }
    let mut unrelated_unknown = frame.clone();
    let mut unknown = test_receipt(
        &mut unrelated_unknown,
        "other-target",
        TaskFrozenNodeOutcome::Unknown,
    );
    unknown.input_sha256 = digest("other-input");
    assert!(matches!(
        assess(&contract(), &unrelated_unknown, &[witness.clone(), unknown]),
        TaskCompletionAssessment::Unverifiable { .. }
    ));

    // 同一批次可能并行执行，消息数组顺序不能证明哪个实际最后完成。
    let mut concurrent = frame.clone();
    let failed = test_receipt(
        &mut concurrent,
        "parallel-test",
        TaskFrozenNodeOutcome::Exited {
            exit_code: Some(1),
            signal: None,
        },
    );
    let later_calls = concurrent.messages[3].tool_calls.clone();
    let later_results = concurrent.messages[4].tool_results.clone();
    concurrent.messages[1].tool_calls.extend(later_calls);
    concurrent.messages[2].tool_results.extend(later_results);
    concurrent.messages.truncate(3);
    concurrent.tool_batches = 1;
    concurrent.provider_requests = 2;
    concurrent.validate(MAX_STEPS as usize).unwrap();
    assert!(matches!(
        assess(&contract(), &concurrent, &[witness, failed]),
        TaskCompletionAssessment::Unverifiable { .. }
    ));
}

#[test]
fn witness_owner_session_call_pair_and_payload_tampering_are_rejected() {
    let mut frame = frame("pairing");
    let witness = success(&mut frame, "test");
    let mutations: [fn(&mut TaskFrozenNodeEvidence); 8] = [
        |e| e.owner.user_id = "mallory".into(),
        |e| e.owner.session_id = "other-session".into(),
        |e| e.model_session = e.model_session.fork("other-task", Some("other")).unwrap(),
        |e| e.call_id = "other-call".into(),
        |e| e.arguments_sha256 = digest("other-arguments"),
        |e| e.tool_result_sha256 = digest("other-result"),
        |e| e.input_sha256 = "not-a-sha256".into(),
        |e| {
            e.model_session = ModelSession::anchored(
                "task",
                Some(&ContextScope {
                    session_id: owner().session_id,
                    user_id: owner().user_id,
                }),
                "pairing",
            )
            .unwrap()
        },
    ];
    for mutate in mutations {
        let mut altered = witness.clone();
        mutate(&mut altered);
        assert_eq!(
            validate_frozen_node_evidence(&owner(), &frame, std::slice::from_ref(&altered)),
            Err(TaskError::CorruptState)
        );
        assert!(!assess(&contract(), &frame, &[altered]).verified());
    }
    let mut changed_arguments = frame.clone();
    changed_arguments.messages[1].tool_calls[0].arguments = json!({"bundle": "changed"});
    let mut changed_result = frame.clone();
    changed_result.messages[2].tool_results[0].output =
        ToolOutput::Success(json!({"exit_code": 1}));
    let mut unpaired = frame.clone();
    unpaired.messages[2].tool_results[0].call_id = "unknown-call".into();
    let mut other_scope = frame.clone();
    other_scope.scope.user_id = "mallory".into();
    for altered in [changed_arguments, changed_result, unpaired, other_scope] {
        assert!(!assess(&contract(), &altered, std::slice::from_ref(&witness)).verified());
    }
    assert!(!assess(&contract(), &frame, &[witness.clone(), witness.clone()]).verified());
    let mut replacement = witness.clone();
    replacement.outcome = TaskFrozenNodeOutcome::Unknown;
    assert_eq!(
        merge_frozen_node_evidence(&owner(), &frame, &[witness], vec![replacement]),
        Err(TaskError::CorruptState)
    );
}

#[tokio::test]
async fn persisted_witness_and_final_checkpoint_settle_without_source_or_reexecution_on_restart() {
    for finish_before_restart in [false, true] {
        let world = World::new();
        let source = Arc::new(EvidenceSource::default());
        let (kernel, tasks) = world.open(Some(source.clone())).await.unwrap();
        let task = create(&tasks, "historical-result", contract());
        let mut frame = frame(&task.id);
        let witness = success(&mut frame, "test");
        source.record(witness.clone());
        read_log(&mut frame);
        let checkpointed = save(&tasks, &task, &frame, 30);
        let execution = checkpointed.execution.unwrap();
        assert_eq!(execution.checkpoint, Some(frame.clone()));
        assert_eq!(
            execution.frozen_node_evidence.as_slice(),
            std::slice::from_ref(&witness)
        );
        // 完成及关闭不读取运行器；模拟运行器已经不可用。
        source.fail.store(true, Ordering::SeqCst);
        if finish_before_restart {
            assert_eq!(finish(&tasks, &task, &frame).status, TaskStatus::Completed);
        }
        kernel.stop_all().await.unwrap();
        let captured = source.captures.load(Ordering::SeqCst);
        assert_eq!(captured, 1);
        let (kernel, tasks) = world.open(None).await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_eq!(restored.status, TaskStatus::Completed);
        assert_eq!(restored.result, frame.result);
        assert_eq!(
            restored.usage.unwrap().tool_calls,
            frame.used_tool_calls as u32
        );
        let execution = restored.execution.unwrap();
        assert_eq!(execution.checkpoint, Some(frame));
        assert_eq!(execution.frozen_node_evidence, [witness]);
        assert_eq!(execution.recoveries, 0);
        assert_eq!(source.captures.load(Ordering::SeqCst), captured);
        assert!(tasks.snapshot().unwrap().queued().is_empty());
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn failed_capture_leaves_previous_checkpoint_and_witness_atomic() {
    let world = World::new();
    let source = Arc::new(EvidenceSource::default());
    let (kernel, tasks) = world.open(Some(source.clone())).await.unwrap();
    let task = create(&tasks, "capture-failure", contract());
    let mut frame = frame(&task.id);
    frame.result = None;
    let witness = success(&mut frame, "test");
    source.record(witness.clone());
    let before = save(&tasks, &task, &frame, 30);
    let mut later = frame.clone();
    read_log(&mut later);
    later.result = Some("已整理结果".into());
    source.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        tasks.checkpoint(&task.id, later, u64::MAX, 40),
        Err(TaskError::Unavailable)
    ));
    assert!(saved(&tasks, &task.id) == before);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open(None).await.unwrap();
    let restored = saved(&tasks, &task.id);
    let execution = restored.execution.unwrap();
    assert_eq!(execution.checkpoint, Some(frame));
    assert_eq!(execution.frozen_node_evidence, [witness]);
    assert_ne!(restored.status, TaskStatus::Completed);
    assert_eq!(source.captures.load(Ordering::SeqCst), 2);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn pending_side_effect_and_cancellation_cannot_be_overridden_by_earlier_success() {
    for cancel in [false, true] {
        let world = World::new();
        let source = Arc::new(EvidenceSource::default());
        let (kernel, tasks) = world.open(Some(source.clone())).await.unwrap();
        let task = create(&tasks, "stop-boundary", contract());
        let mut frame = frame(&task.id);
        let witness = success(&mut frame, "test");
        source.record(witness.clone());
        if !cancel {
            frame.result = None;
            frame.pending_calls.push(ToolCall {
                id: "pending-external-effect".into(),
                name: "send_file".into(),
                arguments: json!({"path": "report.txt"}),
            });
            frame.used_tool_calls += 1;
            frame.tool_batches += 1;
            frame.provider_requests += 1;
        }
        save(&tasks, &task, &frame, 30);
        if cancel {
            tasks.cancel(&owner().user_id, &task.id, 40).unwrap();
            assert_eq!(finish(&tasks, &task, &frame).status, TaskStatus::Cancelled);
        }
        source.fail.store(true, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open(None).await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_eq!(
            restored.status,
            if cancel {
                TaskStatus::Cancelled
            } else {
                TaskStatus::Blocked
            }
        );
        let execution = restored.execution.unwrap();
        assert_eq!(execution.checkpoint, Some(frame));
        assert_eq!(execution.frozen_node_evidence, [witness]);
        assert!(execution.resume_at_ms.is_none());
        assert_eq!(source.captures.load(Ordering::SeqCst), 1);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn missing_source_rejects_new_frozen_contract_without_changing_legacy_tasks() {
    let world = World::new();
    let (kernel, tasks) = world.open(None).await.unwrap();
    assert!(matches!(
        tasks.create_authorized(
            &owner(),
            spec(),
            "no-runner",
            vec!["请验证冻结输入".into()],
            Some(contract()),
            Some(u64::MAX),
            10,
        ),
        Err(TaskError::Unavailable)
    ));
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    let legacy = create(
        &tasks,
        "legacy-text",
        TaskCompletionContract::Text {
            required_fragments: vec!["已完成".into()],
            json_equals: None,
            valid_json: false,
            json_required_pointers: vec![],
        },
    );
    let mut frame = frame(&legacy.id);
    frame.result = Some("已完成".into());
    save(&tasks, &legacy, &frame, 30);
    assert_eq!(
        finish(&tasks, &legacy, &frame).status,
        TaskStatus::Completed
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn same_owner_and_contract_cannot_transfer_another_tasks_checkpoint_or_evidence() {
    let world = World::new();
    let source = Arc::new(EvidenceSource::default());
    let (kernel, tasks) = world.open(Some(source.clone())).await.unwrap();
    let task_a = create(&tasks, "original-task", contract());
    let task_b = create(&tasks, "other-task", contract());
    let before_b = saved(&tasks, &task_b.id);
    let mut frame_a = frame(&task_a.id);
    let witness_a = success(&mut frame_a, "original-test");
    source.record(witness_a);
    let stored_a = save(&tasks, &task_a, &frame_a, 30);
    assert_eq!(
        frame_a.model_session,
        Some(frozen_node_task_session(&owner(), &task_a.id).unwrap())
    );
    assert!(matches!(
        tasks.checkpoint(&task_b.id, frame_a.clone(), u64::MAX, 40),
        Err(TaskError::InvalidInput)
    ));
    assert!(saved(&tasks, &task_b.id) == before_b);

    // 即使调用者把整套看似自洽的 execution 复制进内存对象，结算还须绑定任务 ID。
    let mut copied = before_b;
    copied.execution = stored_a.execution;
    assert!(matches!(
        evaluate_task_completion(&copied, frame_a.result.as_deref().unwrap()),
        TaskCompletionAssessment::Unverifiable { .. }
    ));
    assert_eq!(source.captures.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unfinished_unknown_or_cancelled_witness_blocks_retry_restart_and_manual_resume() {
    for outcome in [
        TaskFrozenNodeOutcome::Unknown,
        TaskFrozenNodeOutcome::Cancelled,
    ] {
        let world = World::new();
        let source = Arc::new(EvidenceSource::default());
        let (kernel, tasks) = world.open(Some(source.clone())).await.unwrap();
        let task = create(&tasks, "unknown-before-final-result", contract());
        let mut frame = frame(&task.id);
        frame.result = None;
        let witness = test_receipt(&mut frame, "unfinished-test", outcome);
        assert!(matches!(
            frame.messages[2].tool_results[0].output,
            ToolOutput::Success(_)
        ));
        source.record(witness.clone());
        let before_retry = save(&tasks, &task, &frame, 30);
        assert!(matches!(
            tasks.retry(&task.id, 40),
            Err(TaskError::InvalidTransition)
        ));
        assert!(saved(&tasks, &task.id) == before_retry);
        source.fail.store(true, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();

        let (kernel, tasks) = world.open(None).await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_eq!(restored.status, TaskStatus::Blocked);
        let execution = restored.execution.as_ref().unwrap();
        assert_eq!(execution.recoveries, 0);
        assert!(execution.resume_at_ms.is_none());
        assert_eq!(execution.checkpoint, Some(frame));
        assert_eq!(execution.frozen_node_evidence, [witness]);
        assert_eq!(source.captures.load(Ordering::SeqCst), 1);
        assert!(matches!(
            tasks.resume_blocked(
                &owner(),
                &task.id,
                "文字声称已经核验不能消除未知副作用",
                restored.finished_at_ms.unwrap() + 1,
            ),
            Err(TaskError::InvalidTransition)
        ));
        assert!(saved(&tasks, &task.id) == restored);
        assert_eq!(source.captures.load(Ordering::SeqCst), 1);
        kernel.stop_all().await.unwrap();
    }
}
