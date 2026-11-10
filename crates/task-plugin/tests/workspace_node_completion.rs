//! 工作区测试只采用宿主原见证与独立收尾观察，不从模型 JSON 推导成功。
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, ModelSession, ToolCall, ToolOutput, ToolResult,
    TurnCheckpoint,
};
use eve_task_api::*;
use eve_task_plugin::{
    evaluate_checkpoint_completion, evaluate_completion_contract,
    evaluate_workspace_node_completion,
};
use serde_json::json;

fn digest(label: &str) -> String {
    task_strategy_digest(&label)
}
fn owner() -> TaskOwner {
    TaskOwner {
        session_id: "workspace-session".into(),
        user_id: "alice".into(),
    }
}
fn contract() -> TaskCompletionContract {
    TaskCompletionContract::WorkspaceNodeTest {
        workspace_sha256: digest("workspace"),
        directory: "project".into(),
        entry: "test.mjs".into(),
        args: vec!["--case=all".into()],
        runtime_sha256: digest("runtime"),
        command_sha256: digest("command"),
    }
}
fn frame() -> TurnCheckpoint {
    let scope = ContextScope {
        session_id: owner().session_id,
        user_id: owner().user_id,
    };
    TurnCheckpoint {
        model_session: Some(
            ModelSession::scoped("main", &scope)
                .unwrap()
                .with_turn("7")
                .unwrap()
                .fork("subagent", Some("7:code"))
                .unwrap(),
        ),
        scope,
        pending_model_request: None,
        pending_request_fingerprint: None,
        version: 1,
        messages: vec![ChatMessage::text(ChatRole::User, "修改项目并测试")],
        pending_calls: vec![],
        used_tool_calls: 0,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 0,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: Some("收尾观察时项目与通过测试的包一致。".into()),
    }
}
fn batch(frame: &mut TurnCheckpoint, call: ToolCall) -> ToolResult {
    let result = ToolResult {
        call_id: call.id.clone(),
        output: ToolOutput::Success(json!({"verified":true,"exit_code":0})),
    };
    frame
        .messages
        .push(ChatMessage::assistant_tool_calls(vec![call]).unwrap());
    frame
        .messages
        .push(ChatMessage::tool_results(vec![result.clone()]).unwrap());
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
    result
}
fn test(
    frame: &mut TurnCheckpoint,
    id: &str,
    outcome: TaskFrozenNodeOutcome,
) -> WorkspaceNodeEvidence {
    let arguments = json!({"directory":"project","entry":"test.mjs","args":["--case=all"]});
    let receipt = batch(
        frame,
        ToolCall {
            id: id.into(),
            name: WORKSPACE_NODE_TEST_TOOL.into(),
            arguments: arguments.clone(),
        },
    );
    WorkspaceNodeEvidence {
        owner: owner(),
        model_session: frame.model_session.clone().unwrap(),
        call_id: id.into(),
        arguments_sha256: task_strategy_digest(&arguments),
        tool_result_sha256: task_strategy_digest(&receipt),
        input_sha256: digest("input"),
        workspace_sha256: digest("workspace"),
        directory: "project".into(),
        entry: "test.mjs".into(),
        args: vec!["--case=all".into()],
        runtime_sha256: digest("runtime"),
        command_sha256: digest("command"),
        outcome,
    }
}
fn success() -> TaskFrozenNodeOutcome {
    TaskFrozenNodeOutcome::Exited {
        exit_code: Some(0),
        signal: None,
    }
}
fn observation(frame: &TurnCheckpoint) -> WorkspaceObservation {
    WorkspaceObservation {
        owner: owner(),
        model_session: frame.model_session.clone().unwrap(),
        checkpoint_sha256: workspace_node_checkpoint_sha256(frame),
        workspace_sha256: digest("workspace"),
        directory: "project".into(),
        entry: "test.mjs".into(),
        args: vec!["--case=all".into()],
        input_sha256: digest("input"),
        observed_at_ms: 1000,
    }
}
fn assess(
    frame: &TurnCheckpoint,
    evidence: &[WorkspaceNodeEvidence],
    current: Option<&WorkspaceObservation>,
) -> TaskCompletionAssessment {
    evaluate_workspace_node_completion(
        &contract(),
        &owner(),
        frame,
        12,
        frame.result.as_deref().unwrap_or("声称完成"),
        evidence,
        current,
    )
}
fn reason(value: TaskCompletionAssessment) -> String {
    value.reason().into()
}

#[test]
fn success_survives_later_notes_and_serialization_without_changing_old_receipts_contract() {
    let mut frame = frame();
    let evidence = vec![test(&mut frame, "test", success())];
    let before = evidence.clone();
    batch(
        &mut frame,
        ToolCall {
            id: "note".into(),
            name: "save_exploration_note".into(),
            arguments: json!({"note":"调查结束"}),
        },
    );
    let current = observation(&frame);
    frame.validate(12).unwrap();
    assert!(assess(&frame, &evidence, Some(&current)).verified());
    let persisted = serde_json::to_vec(&(frame, evidence, current)).unwrap();
    let (frame, evidence, current): (
        TurnCheckpoint,
        Vec<WorkspaceNodeEvidence>,
        WorkspaceObservation,
    ) = serde_json::from_slice(&persisted).unwrap();
    assert_eq!(evidence, before);
    assert!(assess(&frame, &evidence, Some(&current)).verified());
    assert!(
        !evaluate_completion_contract(
            &contract(),
            &frame.messages,
            frame.result.as_deref().unwrap(),
            None
        )
        .verified()
    );
    assert!(
        !evaluate_checkpoint_completion(
            &contract(),
            &owner(),
            &frame,
            12,
            frame.result.as_deref().unwrap(),
            &[]
        )
        .verified()
    );
    let old_contract = TaskCompletionContract::Receipts {
        contract: TaskEvaluationContract {
            checks: vec![TaskReceiptCheck {
                tool: WORKSPACE_NODE_TEST_TOOL.into(),
                arguments: frame.messages[1].tool_calls[0].arguments.clone(),
                output_pointer: "/verified".into(),
                expected: json!(true),
            }],
            require_final_batch: true,
        },
    };
    assert!(
        !evaluate_completion_contract(
            &old_contract,
            &frame.messages,
            frame.result.as_deref().unwrap(),
            None
        )
        .verified()
    );
    old_contract.validate().unwrap();
}

#[test]
fn changed_workspace_missing_observation_and_old_checkpoint_observation_do_not_pass() {
    let mut frame = frame();
    let evidence = vec![test(&mut frame, "test", success())];
    let mut current = observation(&frame);
    assert_eq!(
        reason(assess(&frame, &evidence, None)),
        "missing_current_workspace_observation"
    );
    current.input_sha256 = digest("edited-after-test");
    assert_eq!(
        reason(assess(&frame, &evidence, Some(&current))),
        "workspace_changed_since_test"
    );
    current = observation(&frame);
    batch(
        &mut frame,
        ToolCall {
            id: "later".into(),
            name: "task_progress".into(),
            arguments: json!({"status":"completed"}),
        },
    );
    assert_eq!(
        reason(assess(&frame, &evidence, Some(&current))),
        "invalid_current_workspace_observation"
    );
    assert!(assess(&frame, &evidence, Some(&observation(&frame))).verified());
}

#[test]
fn latest_actual_failure_overrides_success_and_unknown_is_not_hidden_by_a_later_success() {
    let mut frame = frame();
    let mut evidence = vec![test(&mut frame, "first", success())];
    evidence.push(test(
        &mut frame,
        "failure",
        TaskFrozenNodeOutcome::Exited {
            exit_code: Some(2),
            signal: None,
        },
    ));
    assert_eq!(
        reason(assess(&frame, &evidence, Some(&observation(&frame)))),
        "workspace_node_test_failed"
    );
    evidence.push(test(&mut frame, "fixed", success()));
    assert!(assess(&frame, &evidence, Some(&observation(&frame))).verified());
    evidence[1].outcome = TaskFrozenNodeOutcome::Unknown;
    assert_eq!(
        reason(assess(&frame, &evidence, Some(&observation(&frame)))),
        "unknown_workspace_node_effect_not_replayed"
    );
    evidence[1].outcome = TaskFrozenNodeOutcome::Cancelled;
    assert_eq!(
        reason(assess(&frame, &evidence, Some(&observation(&frame)))),
        "unknown_workspace_node_effect_not_replayed"
    );
}

#[test]
fn evidence_cannot_be_fabricated_from_json_transferred_or_replaced() {
    let mut frame = frame();
    let witness = test(&mut frame, "real", success());
    let current = observation(&frame);
    assert_eq!(
        reason(assess(&frame, &[], Some(&current))),
        "missing_trusted_workspace_node_evidence"
    );
    assert_eq!(
        merge_workspace_node_evidence(
            &owner(),
            &frame,
            std::slice::from_ref(&witness),
            vec![witness.clone()]
        )
        .unwrap(),
        vec![witness.clone()]
    );
    let mut replaced = witness.clone();
    replaced.input_sha256 = digest("replacement");
    assert!(
        merge_workspace_node_evidence(
            &owner(),
            &frame,
            std::slice::from_ref(&witness),
            vec![replaced]
        )
        .is_err()
    );
    for mutate in 0..7 {
        let mut bad = witness.clone();
        match mutate {
            0 => bad.owner.user_id = "bob".into(),
            1 => bad.model_session = bad.model_session.fork("subagent", Some("other")).unwrap(),
            2 => bad.arguments_sha256 = digest("different args"),
            3 => bad.tool_result_sha256 = digest("different output"),
            4 => bad.entry = "other.mjs".into(),
            5 => bad.workspace_sha256 = digest("other workspace"),
            _ => bad.runtime_sha256 = digest("other runtime"),
        }
        assert!(!assess(&frame, &[bad], Some(&current)).verified());
    }
    for mutate in 0..7 {
        let mut bad = current.clone();
        match mutate {
            0 => bad.owner.user_id = "bob".into(),
            1 => bad.model_session = bad.model_session.fork("subagent", Some("other")).unwrap(),
            2 => bad.workspace_sha256 = digest("other workspace"),
            3 => bad.directory = "other".into(),
            4 => bad.args.push("extra".into()),
            5 => bad.checkpoint_sha256 = digest("other checkpoint"),
            _ => bad.observed_at_ms = 0,
        }
        assert!(!assess(&frame, std::slice::from_ref(&witness), Some(&bad)).verified());
    }
}

#[test]
fn simultaneous_matching_tests_and_pending_execution_are_unverifiable() {
    let mut frame = frame();
    let evidence = vec![
        test(&mut frame, "one", success()),
        test(&mut frame, "two", success()),
    ];
    let mut calls = frame.messages[1].tool_calls.clone();
    calls.extend(frame.messages[3].tool_calls.clone());
    let mut results = frame.messages[2].tool_results.clone();
    results.extend(frame.messages[4].tool_results.clone());
    frame.messages.truncate(1);
    frame
        .messages
        .push(ChatMessage::assistant_tool_calls(calls).unwrap());
    frame
        .messages
        .push(ChatMessage::tool_results(results).unwrap());
    frame.tool_batches = 1;
    assert_eq!(
        reason(assess(&frame, &evidence, Some(&observation(&frame)))),
        "ambiguous_workspace_node_test_batch"
    );
    frame.result = None;
    frame.pending_calls.push(ToolCall {
        id: "pending".into(),
        name: WORKSPACE_NODE_TEST_TOOL.into(),
        arguments: json!({"entry":"test.mjs"}),
    });
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    assert!(!assess(&frame, &evidence, Some(&observation(&frame))).verified());
}

#[test]
fn selector_rejects_ambiguous_or_escaping_paths_and_contract_serde_has_no_input_digest() {
    for directory in [
        "../elsewhere",
        "/tmp",
        "./project",
        "project/",
        "project//sub",
        ".git",
        "C:\\project",
    ] {
        assert!(validate_workspace_node_selector(directory, "test.mjs", &[]).is_err());
    }
    for entry in [
        "../test.js",
        "./test.js",
        "/test.js",
        "test.node",
        "test.js\0",
        ".eve-developer/test.js",
    ] {
        assert!(validate_workspace_node_selector(".", entry, &[]).is_err());
    }
    assert!(validate_workspace_node_selector(".", "test.mjs", &[]).is_ok());
    assert!(validate_workspace_node_selector(".", &format!("{}.js", "a".repeat(509)), &[]).is_ok());
    assert!(
        validate_workspace_node_selector(".", &format!("{}.js", "a".repeat(510)), &[]).is_err()
    );
    assert!(validate_workspace_node_selector(".", ".js", &[]).is_err());
    assert!(
        validate_workspace_node_selector(".", "test.mjs", &["a".repeat(2049), "b".repeat(2048)])
            .is_err()
    );
    assert!(
        validate_workspace_node_selector("project", "test.mjs", &vec!["x".into(); 17]).is_err()
    );
    let value = serde_json::to_value(contract()).unwrap();
    assert!(value.get("input_sha256").is_none());
    let mut tampered = value;
    tampered["input_sha256"] = json!(digest("posthoc"));
    assert!(serde_json::from_value::<TaskCompletionContract>(tampered).is_err());
}

#[test]
fn final_result_original_budget_and_explicit_remaining_work_are_still_required() {
    let mut frame = frame();
    let evidence = vec![test(&mut frame, "test", success())];
    let current = observation(&frame);
    assert!(
        !evaluate_workspace_node_completion(
            &contract(),
            &owner(),
            &frame,
            0,
            frame.result.as_deref().unwrap(),
            &evidence,
            Some(&current)
        )
        .verified()
    );
    assert_eq!(
        reason(evaluate_workspace_node_completion(
            &contract(),
            &owner(),
            &frame,
            12,
            "另一个最终结果",
            &evidence,
            Some(&current)
        )),
        "missing_or_mismatched_saved_final_result"
    );
    frame.closing = true;
    frame.remaining = Some("还有未完成的需求".into());
    assert!(matches!(
        assess(&frame, &evidence, Some(&observation(&frame))),
        TaskCompletionAssessment::Blocked { .. }
    ));
}

#[tokio::test]
async fn background_admission_remains_unavailable_without_workspace_observation_wiring() {
    let kernel = eve_kernel::Kernel::new();
    let plugin = eve_task_plugin::TaskPlugin::new().unwrap();
    let tasks = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let result = tasks.create_authorized(
        &owner(),
        TaskSpec {
            title: "工作区测试".into(),
            instructions: "修改项目并测试".into(),
            max_steps: 12,
            context: None,
        },
        "workspace",
        vec!["请修改项目并测试".into()],
        Some(contract()),
        Some(u64::MAX),
        10,
    );
    assert!(matches!(result, Err(TaskError::Unavailable)));
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    kernel.stop_all().await.unwrap();
}
