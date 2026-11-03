use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, ModelRequest, ToolCall, ToolFailureCode, ToolOutput,
    ToolResult, TurnCheckpoint,
};
use eve_plugin_api::{PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{TaskController, TaskPlugin, evaluate_completion_contract};
use serde_json::{Value, json};
use std::sync::Arc;

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
    async fn open(&self) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = TaskPlugin::new()?;
        let tasks = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, tasks))
    }
}

fn owner() -> TaskOwner {
    TaskOwner {
        session_id: "completion-session".into(),
        user_id: "alice".into(),
    }
}
fn spec(max_steps: u32) -> TaskSpec {
    TaskSpec {
        title: "冻结验收条件".into(),
        instructions: "完成并按派发时的合同验收".into(),
        max_steps,
        context: None,
    }
}
fn text_contract() -> TaskCompletionContract {
    TaskCompletionContract::Text {
        required_fragments: vec!["验收通过".into(), "交付物".into()],
        json_equals: None,
        valid_json: false,
        json_required_pointers: vec![],
    }
}
fn receipt_contract() -> TaskCompletionContract {
    TaskCompletionContract::Receipts {
        contract: TaskEvaluationContract {
            checks: vec![TaskReceiptCheck {
                tool: "verify".into(),
                arguments: json!({"target": "fixture"}),
                output_pointer: "/valid".into(),
                expected: json!(true),
            }],
            require_final_batch: true,
        },
    }
}
#[test]
fn technical_wait_registration_cannot_be_a_completion_receipt() {
    let contract = TaskCompletionContract::Receipts {
        contract: TaskEvaluationContract {
            checks: vec![TaskReceiptCheck {
                tool: "register_task_wait".into(),
                arguments: json!({"condition":{"type":"file_changed","path":"output.txt"},"reason":"仍在等待"}),
                output_pointer: "/registered".into(),
                expected: json!(true),
            }],
            require_final_batch: true,
        },
    };
    assert_eq!(contract.validate(), Err(TaskError::InvalidInput));
}
fn frame(result: &str) -> TurnCheckpoint {
    let owner = owner();
    TurnCheckpoint {
        version: 1,
        scope: ContextScope {
            session_id: owner.session_id,
            user_id: owner.user_id,
        },
        messages: vec![ChatMessage::text(ChatRole::User, "请完成并验证")],
        pending_calls: vec![],
        used_tool_calls: 0,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 0,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: Some(result.into()),
    }
}
fn batch(frame: &mut TurnCheckpoint, id: &str, tool: &str, arguments: Value, output: ToolOutput) {
    frame.messages.push(
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: id.into(),
            name: tool.into(),
            arguments,
        }])
        .unwrap(),
    );
    frame.messages.push(
        ChatMessage::tool_results(vec![ToolResult {
            call_id: id.into(),
            output,
        }])
        .unwrap(),
    );
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
}
fn verify(frame: &mut TurnCheckpoint, id: &str, valid: bool) {
    batch(
        frame,
        id,
        "verify",
        json!({"target": "fixture"}),
        ToolOutput::Success(json!({"valid": valid})),
    );
}
fn usage(frame: &TurnCheckpoint) -> TaskUsage {
    TaskUsage {
        tool_calls: (frame.used_tool_calls + frame.delegated_tool_calls) as u32,
        model_requests: (frame.provider_requests + frame.delegated_model_requests) as u32,
    }
}
fn create(
    tasks: &TaskController,
    source: &str,
    contract: TaskCompletionContract,
    max_steps: u32,
    deadline: u64,
) -> Task {
    let task = tasks
        .create_authorized(
            &owner(),
            spec(max_steps),
            source,
            vec!["请完成并验证".into()],
            Some(contract),
            Some(deadline),
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap()
}
fn save(tasks: &TaskController, task: &Task, frame: &TurnCheckpoint, deadline: u64) {
    assert_valid(frame, task.spec.max_steps);
    tasks
        .checkpoint(&task.id, frame.clone(), deadline, 30)
        .unwrap();
}
fn finish(tasks: &TaskController, task: &Task, frame: &TurnCheckpoint, now: u64) -> Task {
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: frame.result.clone().unwrap_or_else(|| "声称完成".into()),
            },
            usage(frame),
            now,
        )
        .unwrap()
}
fn saved(tasks: &TaskController, id: &str) -> Task {
    tasks.snapshot().unwrap().get(id).unwrap().clone()
}
fn assert_valid(frame: &TurnCheckpoint, max_steps: u32) {
    frame.validate(max_steps as usize).unwrap();
    ModelRequest::validate_parts(&frame.messages, &[]).unwrap();
}
fn assert_blocked(task: &Task) {
    assert_eq!(task.status, TaskStatus::Blocked);
    assert!(task.notice_pending());
    assert!(
        task.result
            .as_ref()
            .is_some_and(|result| !result.is_empty())
    );
    let execution = task.execution.as_ref().unwrap();
    assert_eq!(execution.recoveries, 0);
    assert!(execution.resume_at_ms.is_none());
}

#[test]
fn completion_contracts_have_tagged_wire_shapes_and_validate_mechanical_conditions() {
    let contract: TaskCompletionContract = serde_json::from_value(json!({
        "kind": "text", "required_fragments": ["交付物"], "json_equals": null
    }))
    .unwrap();
    contract.validate().unwrap();
    assert_eq!(serde_json::to_value(&contract).unwrap()["kind"], "text");
    assert_eq!(
        serde_json::to_value(TaskCompletionContract::Unspecified).unwrap(),
        json!({"kind": "unspecified"})
    );
    receipt_contract().validate().unwrap();
    for wire in [
        json!({"kind": "text", "required_fragments": [], "json_equals": null}),
        json!({"kind": "text", "required_fragments": ["  "], "json_equals": null}),
        json!({"kind": "text", "required_fragments": [], "json_equals": null,
            "json_required_pointers": ["invalid-pointer"]}),
        json!({"kind": "receipts", "contract": {"checks": [], "require_final_batch": true}}),
    ] {
        let invalid: TaskCompletionContract = serde_json::from_value(wire).unwrap();
        assert!(invalid.validate().is_err());
    }
}

#[tokio::test]
async fn replay_and_restart_preserve_the_original_frozen_contract_and_authorization() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let contract = text_contract();
    let original = tasks
        .create_authorized(
            &owner(),
            spec(8),
            "frozen-contract",
            vec!["真实用户原话，不改写".into()],
            Some(contract.clone()),
            Some(u64::MAX),
            10,
        )
        .unwrap();
    let replay = tasks
        .create_authorized(
            &owner(),
            spec(100),
            "frozen-contract",
            vec!["后来追加的不同原话".into()],
            Some(TaskCompletionContract::Unspecified),
            None,
            15,
        )
        .unwrap();
    assert!(replay == original);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let restored = saved(&tasks, &original.id);
    assert_eq!(restored.status, TaskStatus::Queued);
    let execution = restored.execution.unwrap();
    assert!(execution.completion_contract == Some(contract));
    assert_eq!(execution.review_messages, ["真实用户原话，不改写"]);
    assert_eq!(execution.authorization_deadline_at_ms, Some(u64::MAX));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unspecified_completion_is_blocked_in_live_and_restart_settlement() {
    for restart in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(
            &tasks,
            "unspecified",
            TaskCompletionContract::Unspecified,
            8,
            u64::MAX,
        );
        let frame = frame("已完成全部任务");
        save(&tasks, &task, &frame, u64::MAX);
        if !restart {
            assert_blocked(&finish(&tasks, &task, &frame, 40));
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_blocked(&restored);
        assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
        assert!(tasks.snapshot().unwrap().queued().is_empty());
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn matching_text_and_complete_json_are_accepted_live_and_after_restart() {
    let cases = [
        (text_contract(), "交付物已经验收通过"),
        (
            TaskCompletionContract::Text {
                required_fragments: vec![],
                json_equals: Some(json!({"valid": true, "count": 2})),
                valid_json: true,
                json_required_pointers: vec!["/valid".into()],
            },
            " { \"count\": 2, \"valid\": true } ",
        ),
        (
            TaskCompletionContract::Text {
                required_fragments: vec![],
                json_equals: None,
                valid_json: false,
                json_required_pointers: vec!["/report/valid".into()],
            },
            "{\"report\": {\"valid\": false}}",
        ),
    ];
    for (contract, result) in cases {
        for restart in [false, true] {
            let world = World::new();
            let (kernel, tasks) = world.open().await.unwrap();
            let task = create(&tasks, "text-success", contract.clone(), 8, u64::MAX);
            let frame = frame(result);
            save(&tasks, &task, &frame, u64::MAX);
            if !restart {
                assert_eq!(
                    finish(&tasks, &task, &frame, 40).status,
                    TaskStatus::Completed
                );
            }
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let restored = saved(&tasks, &task.id);
            assert_eq!(restored.status, TaskStatus::Completed);
            assert_eq!(restored.result.as_deref(), Some(result));
            assert_eq!(restored.usage, Some(usage(&frame)));
            let execution = restored.execution.unwrap();
            assert_eq!(execution.recoveries, 0);
            assert_eq!(execution.checkpoint, Some(frame));
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn mismatches_repair_the_same_task_at_most_three_times_without_resetting_usage() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "bounded-repairs", text_contract(), 8, u64::MAX);
    let mut frame = frame("只有草稿");
    frame.provider_requests = 3;
    for attempt in 0..=MAX_AUTO_RECOVERIES {
        let previous_messages = frame.messages.clone();
        let previous_usage = usage(&frame);
        save(&tasks, &task, &frame, u64::MAX);
        let settled = finish(&tasks, &task, &frame, 40 + u64::from(attempt));
        assert_eq!(settled.id, task.id);
        let execution = settled.execution.as_ref().unwrap();
        assert!(execution.completion_contract == Some(text_contract()));
        assert_eq!(execution.deadline_at_ms, Some(u64::MAX));
        assert_eq!(execution.authorization_deadline_at_ms, Some(u64::MAX));
        if attempt == MAX_AUTO_RECOVERIES {
            assert_eq!(settled.status, TaskStatus::Blocked);
            assert!(settled.notice_pending());
            assert_eq!(execution.recoveries, MAX_AUTO_RECOVERIES);
            assert!(execution.resume_at_ms.is_none());
            assert_eq!(settled.usage, Some(previous_usage));
            break;
        }
        assert_eq!(settled.status, TaskStatus::Running);
        assert!(settled.result.is_none());
        assert!(settled.finished_at_ms.is_none());
        assert!(settled.usage.is_none());
        assert!(settled.notice.is_none());
        assert_eq!(execution.recoveries, attempt + 1);
        assert!(execution.resume_at_ms.is_some());
        let repaired = execution.checkpoint.as_ref().unwrap();
        assert!(repaired.result.is_none());
        assert!(!repaired.closing);
        assert_eq!(usage(repaired), previous_usage);
        assert!(repaired.messages.starts_with(&previous_messages));
        assert!(repaired.messages.len() > previous_messages.len());
        let feedback =
            serde_json::to_string(&repaired.messages[previous_messages.len()..]).unwrap();
        assert!(feedback.contains("只有草稿"));
        assert!(feedback.contains("验收通过"));
        assert_valid(repaired, task.spec.max_steps);
        frame = repaired.clone();
        frame.provider_requests += 1;
        frame.remaining = None;
        frame.result = Some("只有草稿".into());
    }
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let restored = saved(&tasks, &task.id);
    assert_eq!(restored.status, TaskStatus::Blocked);
    assert_eq!(restored.execution.unwrap().recoveries, MAX_AUTO_RECOVERIES);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_preserves_the_third_scheduled_repair_until_its_actual_result() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "third-repair-restart", text_contract(), 8, u64::MAX);
    let mut frame = frame("只有草稿");
    for expected in 1..=MAX_AUTO_RECOVERIES {
        save(&tasks, &task, &frame, u64::MAX);
        let repairing = finish(&tasks, &task, &frame, 40 + u64::from(expected));
        assert_eq!(repairing.status, TaskStatus::Running);
        let execution = repairing.execution.unwrap();
        assert_eq!(execution.recoveries, expected);
        assert!(execution.resume_at_ms.is_some());
        frame = execution.checkpoint.unwrap();
        if expected < MAX_AUTO_RECOVERIES {
            frame.provider_requests += 1;
            frame.result = Some("只有草稿".into());
        }
    }
    let scheduled = saved(&tasks, &task.id);
    assert!(frame.result.is_none());
    assert_valid(&frame, task.spec.max_steps);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let restored = saved(&tasks, &task.id);
    assert!(restored == scheduled);
    assert_eq!(restored.status, TaskStatus::Running);
    let execution = restored.execution.unwrap();
    assert_eq!(execution.recoveries, MAX_AUTO_RECOVERIES);
    assert!(execution.resume_at_ms.is_some());
    assert_eq!(execution.checkpoint, Some(frame.clone()));
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    // 重启只恢复已派发的第三次修复；取得第四个实际不符结果才结束自动修复。
    frame.provider_requests += 1;
    frame.result = Some("只有草稿".into());
    save(&tasks, &task, &frame, u64::MAX);
    let blocked = finish(&tasks, &task, &frame, 50);
    assert_eq!(blocked.status, TaskStatus::Blocked);
    assert!(blocked.notice_pending());
    assert_eq!(blocked.usage, Some(usage(&frame)));
    let execution = blocked.execution.unwrap();
    assert_eq!(execution.recoveries, MAX_AUTO_RECOVERIES);
    assert!(execution.resume_at_ms.is_none());
    assert_eq!(execution.checkpoint, Some(frame));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_classifies_a_saved_mismatch_then_accepts_a_corrected_answer() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "restart-repair", text_contract(), 8, u64::MAX);
    let mut frame = frame("只有草稿");
    frame.closing = true;
    save(&tasks, &task, &frame, u64::MAX);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let restored = saved(&tasks, &task.id);
    assert_eq!(restored.status, TaskStatus::Running);
    let execution = restored.execution.unwrap();
    assert_eq!(execution.recoveries, 1);
    assert!(execution.resume_at_ms.is_some());
    let mut repaired = execution.checkpoint.unwrap();
    assert!(repaired.result.is_none());
    assert!(!repaired.closing);
    assert_eq!(usage(&repaired), usage(&frame));
    assert_valid(&repaired, task.spec.max_steps);
    repaired.provider_requests += 1;
    repaired.remaining = None;
    repaired.result = Some("交付物已经验收通过".into());
    save(&tasks, &task, &repaired, u64::MAX);
    let completed = finish(&tasks, &task, &repaired, 40);
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(completed.usage, Some(usage(&repaired)));
    assert_eq!(completed.execution.unwrap().recoveries, 1);
    kernel.stop_all().await.unwrap();
}

#[test]
fn final_in_progress_receipt_blocks_even_when_the_caller_omits_remaining() {
    let mut frame = frame("模型声称已完成");
    let progress = json!({"status": "in_progress", "remaining": "还有交付物需要完成"});
    batch(
        &mut frame,
        "progress",
        "task_progress",
        progress.clone(),
        ToolOutput::Success(progress),
    );
    verify(&mut frame, "final-check", true);
    assert_valid(&frame, 8);
    assert!(frame.remaining.is_none());
    let assessment = evaluate_completion_contract(
        &receipt_contract(),
        &frame.messages,
        frame.result.as_deref().unwrap(),
        None,
    );
    assert!(matches!(
        assessment,
        TaskCompletionAssessment::Blocked { .. }
    ));
    assert!(assessment.reason().contains("还有交付物需要完成"));
}

#[tokio::test]
async fn malformed_missing_or_unequal_json_is_not_accepted_as_completion() {
    for result in [
        "```json\n{\"valid\":true}\n```",
        "{\"other\":true}",
        "{\"valid\":false}",
        "{\"valid\":true} trailing",
    ] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(
            &tasks,
            "json-mismatch",
            TaskCompletionContract::Text {
                required_fragments: vec![],
                json_equals: Some(json!({"valid": true})),
                valid_json: true,
                json_required_pointers: vec!["/valid".into()],
            },
            8,
            u64::MAX,
        );
        let frame = frame(result);
        save(&tasks, &task, &frame, u64::MAX);
        let settled = finish(&tasks, &task, &frame, 40);
        assert_eq!(settled.status, TaskStatus::Running);
        assert_eq!(settled.execution.unwrap().recoveries, 1);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn text_claims_cannot_validate_tool_side_effects() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "text-with-tools", text_contract(), 8, u64::MAX);
    let mut frame = frame("交付物已经验收通过");
    verify(&mut frame, "check", true);
    save(&tasks, &task, &frame, u64::MAX);
    assert_blocked(&finish(&tasks, &task, &frame, 40));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn uncheckpointed_model_usage_blocks_a_matching_text_result_without_repair() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "uncheckpointed-usage", text_contract(), 8, u64::MAX);
    let frame = frame("交付物已经验收通过");
    save(&tasks, &task, &frame, u64::MAX);
    let actual_usage = TaskUsage {
        tool_calls: 0,
        model_requests: 2,
    };
    let blocked = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: frame.result.clone().unwrap(),
            },
            actual_usage,
            40,
        )
        .unwrap();
    assert_blocked(&blocked);
    assert_eq!(blocked.usage, Some(actual_usage));
    assert_eq!(blocked.execution.unwrap().checkpoint, Some(frame.clone()));
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let restored = saved(&tasks, &task.id);
    assert_blocked(&restored);
    assert_eq!(restored.usage, Some(actual_usage));
    assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicit_blocked_progress_never_spawns_an_automatic_repair() {
    for restart in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(&tasks, "explicit-blocked", receipt_contract(), 8, u64::MAX);
        let mut frame = frame("需要确认部署环境");
        let progress = json!({"status": "blocked", "remaining": "需要确认部署环境"});
        batch(
            &mut frame,
            "blocked",
            "task_progress",
            progress.clone(),
            ToolOutput::Success(progress),
        );
        save(&tasks, &task, &frame, u64::MAX);
        if !restart {
            assert_blocked(&finish(&tasks, &task, &frame, 40));
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_blocked(&restored);
        assert!(
            restored
                .result
                .as_deref()
                .unwrap()
                .contains("需要确认部署环境")
        );
        assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn unfinished_work_at_the_step_limit_and_expired_authorization_are_blocked() {
    for restart in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(&tasks, "steps-exhausted", receipt_contract(), 1, u64::MAX);
        let mut frame = frame("仍有剩余工作，暂时汇报");
        verify(&mut frame, "check", true);
        frame.remaining = Some("继续验证其他交付物".into());
        frame.closing = true;
        save(&tasks, &task, &frame, u64::MAX);
        if !restart {
            assert_blocked(&finish(&tasks, &task, &frame, 40));
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        assert_blocked(&saved(&tasks, &task.id));
        kernel.stop_all().await.unwrap();
    }
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "expired-repair", text_contract(), 8, 50);
    let frame = frame("只有草稿");
    save(&tasks, &task, &frame, 50);
    assert_blocked(&finish(&tasks, &task, &frame, 60));
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_blocked(&saved(&tasks, &task.id));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn safely_missing_final_verification_repairs_instead_of_claiming_completion() {
    for restart in [false, true] {
        for case in 0..6 {
            let world = World::new();
            let (kernel, tasks) = world.open().await.unwrap();
            let task = create(&tasks, "receipt-match", receipt_contract(), 8, u64::MAX);
            let mut frame = frame("模型声称已完成");
            match case {
                0 => verify(&mut frame, "final", true),
                1 => {
                    verify(&mut frame, "old", true);
                    batch(
                        &mut frame,
                        "edit",
                        "edit_file",
                        json!({"path": "fixture"}),
                        ToolOutput::Success(json!({"saved": true})),
                    );
                }
                2 => batch(
                    &mut frame,
                    "different-target",
                    "verify",
                    json!({"target": "different"}),
                    ToolOutput::Success(json!({"valid": true})),
                ),
                3 => batch(
                    &mut frame,
                    "missing-field",
                    "verify",
                    json!({"target": "fixture"}),
                    ToolOutput::Success(json!({"claimed": "valid"})),
                ),
                4 => batch(
                    &mut frame,
                    "unavailable",
                    "verify",
                    json!({"target": "fixture"}),
                    ToolOutput::Failure {
                        code: ToolFailureCode::Unavailable,
                        message: "验证器暂不可用".into(),
                    },
                ),
                _ => {}
            }
            save(&tasks, &task, &frame, u64::MAX);
            if !restart {
                let settled = finish(&tasks, &task, &frame, 40);
                if case == 0 {
                    assert_eq!(settled.status, TaskStatus::Completed);
                } else {
                    assert_eq!(settled.status, TaskStatus::Running);
                    assert_eq!(settled.execution.as_ref().unwrap().recoveries, 1);
                }
            }
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let restored = saved(&tasks, &task.id);
            if case == 0 {
                assert_eq!(restored.status, TaskStatus::Completed);
                assert_eq!(restored.usage, Some(usage(&frame)));
                assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
            } else {
                assert_eq!(restored.status, TaskStatus::Running);
                assert!(restored.usage.is_none());
                assert!(restored.result.is_none());
                assert!(restored.notice.is_none());
                let execution = restored.execution.unwrap();
                assert_eq!(execution.recoveries, 1);
                assert!(execution.resume_at_ms.is_some());
                let repaired = execution.checkpoint.unwrap();
                assert_eq!(usage(&repaired), usage(&frame));
                assert!(repaired.result.is_none());
                assert!(repaired.messages.starts_with(&frame.messages));
                assert!(repaired.messages.len() > frame.messages.len());
                assert_valid(&repaired, task.spec.max_steps);
            }
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn safely_missing_verification_blocks_when_no_tool_budget_remains() {
    for restart in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(
            &tasks,
            "missing-check-no-budget",
            receipt_contract(),
            1,
            u64::MAX,
        );
        let mut frame = frame("已写入，尚缺规定验证");
        batch(
            &mut frame,
            "write",
            "edit_file",
            json!({"path": "fixture"}),
            ToolOutput::Success(json!({"saved": true})),
        );
        save(&tasks, &task, &frame, u64::MAX);
        if !restart {
            assert_blocked(&finish(&tasks, &task, &frame, 40));
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_blocked(&restored);
        assert_eq!(restored.usage, Some(usage(&frame)));
        assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn failed_receipt_repairs_keep_tool_counts_and_reverify_before_completion() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(&tasks, "receipt-repair", receipt_contract(), 8, u64::MAX);
    let mut frame = frame("已完成");
    verify(&mut frame, "before", false);
    save(&tasks, &task, &frame, u64::MAX);
    let repairing = finish(&tasks, &task, &frame, 40);
    assert_eq!(repairing.status, TaskStatus::Running);
    let execution = repairing.execution.unwrap();
    assert_eq!(execution.recoveries, 1);
    let mut repaired = execution.checkpoint.unwrap();
    assert_eq!(usage(&repaired), usage(&frame));
    assert!(repaired.result.is_none());
    assert!(repaired.messages.starts_with(&frame.messages));
    assert_valid(&repaired, task.spec.max_steps);
    verify(&mut repaired, "after", true);
    repaired.provider_requests += 1;
    repaired.remaining = None;
    repaired.result = Some("修复后验收通过".into());
    save(&tasks, &task, &repaired, u64::MAX);
    let completed = finish(&tasks, &task, &repaired, 50);
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(completed.usage, Some(usage(&repaired)));
    assert_eq!(completed.execution.unwrap().recoveries, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unknown_pending_tool_effects_are_blocked_without_replay_live_or_after_restart() {
    for restart in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let task = create(&tasks, "unknown-effect", receipt_contract(), 8, u64::MAX);
        let mut frame = frame("此最终结果必须移除");
        frame.result = None;
        frame.pending_calls = vec![ToolCall {
            id: "unknown-write".into(),
            name: "edit_file".into(),
            arguments: json!({"path": "fixture"}),
        }];
        frame.used_tool_calls = 1;
        frame.tool_batches = 1;
        save(&tasks, &task, &frame, u64::MAX);
        if !restart {
            assert_blocked(&finish(&tasks, &task, &frame, 40));
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = saved(&tasks, &task.id);
        assert_blocked(&restored);
        assert_eq!(restored.usage, Some(usage(&frame)));
        assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
        assert!(tasks.snapshot().unwrap().queued().is_empty());
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn failed_unknown_tool_effect_preserves_the_reason_and_pending_call_as_blocked() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = create(
        &tasks,
        "failed-unknown-effect",
        receipt_contract(),
        8,
        u64::MAX,
    );
    let mut frame = frame("尚无最终结果");
    frame.result = None;
    frame.pending_calls = vec![ToolCall {
        id: "unknown-write".into(),
        name: "edit_file".into(),
        arguments: json!({"path": "fixture"}),
    }];
    frame.used_tool_calls = 1;
    frame.tool_batches = 1;
    save(&tasks, &task, &frame, u64::MAX);
    let reason = "工具超时，结果未知";
    let blocked = tasks
        .finish(
            &task.id,
            TaskEnd::Failed {
                reason: reason.into(),
            },
            usage(&frame),
            40,
        )
        .unwrap();
    assert_blocked(&blocked);
    assert!(blocked.result.as_deref().unwrap().contains(reason));
    assert_eq!(blocked.usage, Some(usage(&frame)));
    assert_eq!(blocked.execution.unwrap().checkpoint, Some(frame.clone()));
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let restored = saved(&tasks, &task.id);
    assert_blocked(&restored);
    assert!(restored.result.as_deref().unwrap().contains(reason));
    assert_eq!(restored.usage, Some(usage(&frame)));
    assert_eq!(restored.execution.unwrap().checkpoint, Some(frame));
    assert!(tasks.snapshot().unwrap().queued().is_empty());
    kernel.stop_all().await.unwrap();
}
