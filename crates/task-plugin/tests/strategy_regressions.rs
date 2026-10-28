use eve_llm_api::{ChatMessage, ChatRole, ContextScope, ToolCall, ToolResult, TurnCheckpoint};
use eve_task_api::*;
use eve_task_plugin::{evaluate_task_effect, task_strategy_views};
use serde_json::json;

fn scope() -> TaskStrategyScope {
    TaskStrategyScope {
        owner: TaskOwner {
            session_id: "qq:alice".into(),
            user_id: "alice".into(),
        },
        family: "build".into(),
        runtime_sha256: task_strategy_digest(&"runtime:v1"),
        evaluator_version: RECEIPT_EVALUATOR_VERSION.into(),
    }
}
fn contract() -> TaskEvaluationContract {
    TaskEvaluationContract {
        checks: vec![TaskReceiptCheck {
            tool: "verify".into(),
            arguments: json!({"target":"artifact"}),
            output_pointer: "/valid".into(),
            expected: json!(true),
        }],
        require_final_batch: true,
    }
}
fn selection(id: &str, input: &str, arm: TaskStrategyArm) -> TaskStrategySelection {
    TaskStrategySelection {
        task_id: id.into(),
        input_sha256: task_strategy_digest(&input),
        assignment: TaskStrategyAssignment {
            scope: scope(),
            contract: contract(),
            arm,
        },
        selected_version: if matches!(arm, TaskStrategyArm::Control { .. }) {
            None
        } else {
            Some(1)
        },
        max_steps: DEFAULT_STEPS,
        observation: Some(TaskStrategyObservation {
            effect: TaskEffect::Verified,
            reason: "contract_satisfied".into(),
            evidence_sha256: task_strategy_digest(&id),
            usage: TaskUsage {
                tool_calls: 2,
                model_requests: 3,
            },
        }),
    }
}
fn ledger() -> TaskStrategySnapshot {
    TaskStrategySnapshot {
        versions: vec![TaskStrategyVersion {
            version: 1,
            parent: None,
            proposal: TaskStrategyProposal {
                scope: scope(),
                source_id: "source".into(),
                source_input_sha256: task_strategy_digest(&"training"),
                source_evidence_sha256: task_strategy_digest(&"receipt"),
                guidance: "先检查依赖，再验证产物".into(),
            },
        }],
        ..Default::default()
    }
}
#[test]
fn promotion_requires_two_distinct_pairs_and_application_never_counts() {
    let mut snapshot = ledger();
    snapshot.selections.push(selection(
        "c1",
        "one",
        TaskStrategyArm::Control { candidate: 1 },
    ));
    snapshot.selections.push(selection(
        "t1",
        "one",
        TaskStrategyArm::Trial { candidate: 1 },
    ));
    snapshot.selections.push(selection(
        "a1",
        "one",
        TaskStrategyArm::Apply { candidate: 1 },
    ));
    let views = task_strategy_views(&snapshot).unwrap();
    assert_eq!(views[0].paired_successes, 1);
    assert_eq!(views[0].status, TaskStrategyStatus::Candidate);
    snapshot.selections.push(selection(
        "c2",
        "two",
        TaskStrategyArm::Control { candidate: 1 },
    ));
    snapshot.selections.push(selection(
        "t2",
        "two",
        TaskStrategyArm::Trial { candidate: 1 },
    ));
    assert_eq!(
        task_strategy_views(&snapshot).unwrap()[0].status,
        TaskStrategyStatus::Active
    );
    snapshot
        .selections
        .last_mut()
        .unwrap()
        .observation
        .as_mut()
        .unwrap()
        .usage
        .tool_calls = 3;
    assert_eq!(
        task_strategy_views(&snapshot).unwrap()[0].status,
        TaskStrategyStatus::Rejected
    );
}
#[test]
fn source_reuse_duplicate_trials_and_cross_owner_records_are_rejected() {
    for mode in 0..3 {
        let mut snapshot = ledger();
        let mut trial = selection("t1", "one", TaskStrategyArm::Trial { candidate: 1 });
        match mode {
            0 => trial.input_sha256 = snapshot.versions[0].proposal.source_input_sha256.clone(),
            1 => {
                let mut duplicate = trial.clone();
                duplicate.task_id = "t2".into();
                snapshot.selections.push(duplicate);
            }
            _ => trial.assignment.scope.owner.user_id = "bob".into(),
        }
        snapshot.selections.push(trial);
        assert!(task_strategy_views(&snapshot).is_err());
    }
}
fn task() -> Task {
    let owner = scope().owner;
    let check = contract().checks.remove(0);
    let checkpoint = TurnCheckpoint {
        version: 2,
        scope: ContextScope {
            session_id: owner.session_id.clone(),
            user_id: owner.user_id.clone(),
        },
        messages: vec![
            ChatMessage::text(ChatRole::User, "验证产物"),
            ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "v1".into(),
                name: check.tool,
                arguments: check.arguments,
            }])
            .unwrap(),
            ChatMessage::tool_results(vec![
                ToolResult::success("v1", json!({"valid":true})).unwrap(),
            ])
            .unwrap(),
        ],
        pending_calls: vec![],
        used_tool_calls: 1,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 2,
        tool_batches: 1,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: Some("完成".into()),
    };
    Task {
        id: "task1".into(),
        owner,
        spec: TaskSpec {
            title: "验证".into(),
            instructions: "验证产物".into(),
            max_steps: DEFAULT_STEPS,
            context: None,
        },
        source: "source".into(),
        created_at_ms: 1,
        status: TaskStatus::Completed,
        cancel_requested: false,
        started_at_ms: Some(2),
        finished_at_ms: Some(3),
        usage: Some(TaskUsage {
            tool_calls: 1,
            model_requests: 2,
        }),
        result: Some("完成".into()),
        failure: None,
        notice: None,
        execution: Some(TaskExecution {
            checkpoint: Some(checkpoint),
            recoveries: 0,
            resume_at_ms: None,
            deadline_at_ms: Some(100),
            authorization_deadline_at_ms: None,
        }),
    }
}
#[test]
fn evaluator_uses_final_receipts_and_includes_delegated_usage() {
    let mut task = task();
    assert_eq!(
        evaluate_task_effect(&task, &contract()).unwrap().effect,
        TaskEffect::Verified
    );
    task.execution
        .as_mut()
        .unwrap()
        .checkpoint
        .as_mut()
        .unwrap()
        .delegated_tool_calls = 1;
    assert_eq!(
        evaluate_task_effect(&task, &contract()).unwrap().reason,
        "incomplete_usage"
    );
    task.usage.as_mut().unwrap().tool_calls = 2;
    assert_eq!(
        evaluate_task_effect(&task, &contract()).unwrap().effect,
        TaskEffect::Verified
    );
    let checkpoint = task
        .execution
        .as_mut()
        .unwrap()
        .checkpoint
        .as_mut()
        .unwrap();
    checkpoint.messages.push(
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "w1".into(),
            name: "write".into(),
            arguments: json!({}),
        }])
        .unwrap(),
    );
    checkpoint.messages.push(
        ChatMessage::tool_results(vec![ToolResult::success("w1", json!({})).unwrap()]).unwrap(),
    );
    checkpoint.used_tool_calls += 1;
    checkpoint.tool_batches += 1;
    task.usage.as_mut().unwrap().tool_calls = 3;
    assert_eq!(
        evaluate_task_effect(&task, &contract()).unwrap().reason,
        "unbound_final_batch"
    );
}
#[test]
fn self_report_missing_receipts_and_scope_mismatch_are_not_success() {
    let mut task = task();
    task.execution = None;
    assert_eq!(
        evaluate_task_effect(&task, &contract()).unwrap().effect,
        TaskEffect::Inconclusive
    );
    let mut task = self::task();
    task.owner.user_id = "bob".into();
    assert_eq!(
        evaluate_task_effect(&task, &contract()).unwrap().effect,
        TaskEffect::Inconclusive
    );
}
