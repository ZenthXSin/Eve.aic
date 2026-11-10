use eve_llm_api::{ChatMessage, ChatRole, ToolCall, ToolFailureCode, ToolResult};
use eve_task_api::{
    TaskCompletionAssessment, TaskCompletionContract, TaskEvaluationContract, TaskReceiptCheck,
    TaskReceiptEvidenceStatus, task_strategy_digest,
};
use eve_task_plugin::{evaluate_completion_contract, select_task_receipt_evidence};
use serde_json::{Value, json};

fn contract() -> TaskEvaluationContract {
    TaskEvaluationContract {
        checks: vec![TaskReceiptCheck {
            tool: "read_file".into(),
            arguments: json!({"path":"private-input.rs","offset":0}),
            output_pointer: "/sha256".into(),
            expected: json!("expected-digest"),
        }],
        require_final_batch: true,
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn verification_call(id: &str) -> ToolCall {
    let check = contract().checks.remove(0);
    call(id, &check.tool, check.arguments)
}

fn trace() -> Vec<ChatMessage> {
    vec![ChatMessage::text(ChatRole::User, "按原要求检查")]
}

fn append_batch(messages: &mut Vec<ChatMessage>, calls: Vec<ToolCall>, receipts: Vec<ToolResult>) {
    messages.push(ChatMessage::assistant_tool_calls(calls).unwrap());
    messages.push(ChatMessage::tool_results(receipts).unwrap());
}

fn append_verification(messages: &mut Vec<ChatMessage>, id: &str, value: Value) {
    append_batch(
        messages,
        vec![verification_call(id)],
        vec![ToolResult::success(id, value).unwrap()],
    );
}

#[test]
fn later_progress_does_not_hide_historical_receipt_or_relax_completion_contract() {
    let frozen = contract();
    let before = serde_json::to_vec(&frozen).unwrap();
    let mut messages = trace();
    let receipt = ToolResult::success("verified", json!({"sha256":"expected-digest"})).unwrap();
    append_batch(
        &mut messages,
        vec![verification_call("verified")],
        vec![receipt.clone()],
    );
    append_batch(
        &mut messages,
        vec![call(
            "progress",
            "task_progress",
            json!({"status":"completed"}),
        )],
        vec![ToolResult::success("progress", json!({"status":"completed"})).unwrap()],
    );
    messages.push(ChatMessage::text(ChatRole::Assistant, "已完成"));

    let report = select_task_receipt_evidence(&frozen, &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::HistoricalMatch);
    assert_eq!(report.contract_sha256, task_strategy_digest(&frozen));
    assert_eq!(report.checks[0].check_index, 0);
    assert_eq!(
        report.checks[0].check_sha256,
        task_strategy_digest(&frozen.checks[0])
    );
    let source = report.checks[0].source.as_ref().unwrap();
    assert_eq!(source.call_id, "verified");
    assert_eq!(source.batch_ordinal, 1);
    assert_eq!(source.tool, "read_file");
    assert_eq!(
        source.arguments_sha256,
        task_strategy_digest(&frozen.checks[0].arguments)
    );
    assert_eq!(source.receipt_sha256, task_strategy_digest(&receipt));
    assert_eq!(serde_json::to_vec(&frozen).unwrap(), before);
    assert!(matches!(
        evaluate_completion_contract(
            &TaskCompletionContract::Receipts { contract: frozen },
            &messages,
            "已完成",
            None,
        ),
        TaskCompletionAssessment::Mismatch { reason } if reason == "unbound_final_batch"
    ));
}

#[test]
fn latest_exact_call_does_not_select_older_success_after_mismatch() {
    let mut messages = trace();
    append_verification(&mut messages, "old", json!({"sha256":"expected-digest"}));
    append_batch(
        &mut messages,
        vec![call("other", "list_files", json!({"path":"."}))],
        vec![ToolResult::success("other", json!({"files":[]})).unwrap()],
    );
    append_verification(&mut messages, "new", json!({"sha256":"different"}));
    let report = select_task_receipt_evidence(&contract(), &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::HistoricalMismatch);
    assert_eq!(
        report.checks[0].status,
        TaskReceiptEvidenceStatus::HistoricalMismatch
    );
    let source = report.checks[0].source.as_ref().unwrap();
    assert_eq!(source.call_id, "new");
    assert_eq!(source.batch_ordinal, 3);
}

#[test]
fn latest_tool_failure_is_unknown_and_never_selects_old_success() {
    for code in [
        ToolFailureCode::UnknownTool,
        ToolFailureCode::Unavailable,
        ToolFailureCode::OwnerMismatch,
        ToolFailureCode::Inactive,
        ToolFailureCode::PermissionDenied,
        ToolFailureCode::InvalidArguments,
    ] {
        let receipt = ToolResult::failure("new", code, "private tool error").unwrap();
        let mut messages = trace();
        append_verification(&mut messages, "old", json!({"sha256":"expected-digest"}));
        append_batch(
            &mut messages,
            vec![call("other", "list_files", json!({"path":"."}))],
            vec![ToolResult::success("other", json!({"files":[]})).unwrap()],
        );
        append_batch(&mut messages, vec![verification_call("new")], vec![receipt]);
        let report = select_task_receipt_evidence(&contract(), &messages, &[]);
        assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
        assert_eq!(report.checks[0].status, TaskReceiptEvidenceStatus::Unknown);
        assert_eq!(report.checks[0].reason, "tool_failed");
        let source = report.checks[0].source.as_ref().unwrap();
        assert_eq!(source.call_id, "new");
        assert_eq!(source.batch_ordinal, 3);
    }
}

#[test]
fn missing_observation_in_latest_receipt_is_unknown_with_original_source() {
    let mut messages = trace();
    append_verification(&mut messages, "old", json!({"sha256":"expected-digest"}));
    append_verification(&mut messages, "new", json!({"content":"private content"}));
    let report = select_task_receipt_evidence(&contract(), &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
    assert_eq!(report.checks[0].reason, "missing_observation");
    assert_eq!(report.checks[0].source.as_ref().unwrap().call_id, "new");
}

#[test]
fn latest_matching_batch_with_duplicate_checks_is_ambiguous_even_after_old_success() {
    let mut messages = trace();
    append_verification(&mut messages, "old", json!({"sha256":"expected-digest"}));
    append_batch(
        &mut messages,
        vec![verification_call("new-a"), verification_call("new-b")],
        vec![
            ToolResult::success("new-a", json!({"sha256":"expected-digest"})).unwrap(),
            ToolResult::success("new-b", json!({"sha256":"expected-digest"})).unwrap(),
        ],
    );
    let report = select_task_receipt_evidence(&contract(), &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
    assert_eq!(report.checks[0].reason, "ambiguous_matching_calls_in_batch");
    assert!(report.checks[0].source.is_none());
}

#[test]
fn checks_select_independently_by_exact_arguments_and_full_frozen_check_digest() {
    let mut frozen = contract();
    let mut second = frozen.checks[0].clone();
    second.output_pointer = "/size".into();
    second.expected = json!(12);
    frozen.checks.push(second);
    let mut third = frozen.checks[0].clone();
    third.arguments = json!({"path":"other.rs","offset":0});
    frozen.checks.push(third);
    let mut messages = trace();
    append_verification(
        &mut messages,
        "original",
        json!({"sha256":"expected-digest","size":12}),
    );
    append_batch(
        &mut messages,
        vec![call(
            "different-args",
            "read_file",
            json!({"path":"other.rs","offset":0}),
        )],
        vec![ToolResult::success("different-args", json!({"sha256":"different"})).unwrap()],
    );
    let report = select_task_receipt_evidence(&frozen, &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::HistoricalMismatch);
    assert_eq!(report.checks.len(), 3);
    for (index, check) in report.checks.iter().enumerate() {
        assert_eq!(check.check_index, index);
        assert_eq!(
            check.check_sha256,
            task_strategy_digest(&frozen.checks[index])
        );
    }
    assert_ne!(report.checks[0].check_sha256, report.checks[1].check_sha256);
    assert_eq!(
        report.checks[0].status,
        TaskReceiptEvidenceStatus::HistoricalMatch
    );
    assert_eq!(
        report.checks[1].status,
        TaskReceiptEvidenceStatus::HistoricalMatch
    );
    assert_eq!(
        report.checks[2].status,
        TaskReceiptEvidenceStatus::HistoricalMismatch
    );
    assert_eq!(report.checks[0].source, report.checks[1].source);
    assert_eq!(
        report.checks[2].source.as_ref().unwrap().call_id,
        "different-args"
    );
}

#[test]
fn plain_text_or_only_different_arguments_never_create_a_receipt_source() {
    let mut messages = trace();
    messages.push(ChatMessage::text(
        ChatRole::Assistant,
        "read_file成功，sha256=expected-digest",
    ));
    let text_report = select_task_receipt_evidence(&contract(), &messages, &[]);
    assert_eq!(text_report.status, TaskReceiptEvidenceStatus::Missing);
    assert!(text_report.checks[0].source.is_none());
    append_batch(
        &mut messages,
        vec![call(
            "wrong",
            "read_file",
            json!({"path":"private-input.rs","offset":1}),
        )],
        vec![ToolResult::success("wrong", json!({"sha256":"expected-digest"})).unwrap()],
    );
    let report = select_task_receipt_evidence(&contract(), &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::Missing);
    assert_eq!(report.checks[0].reason, "no_matching_call");
    assert!(report.checks[0].source.is_none());
}

#[test]
fn repeated_call_id_across_batches_or_pending_is_unknown_without_source() {
    for pending_duplicate in [false, true] {
        let mut messages = trace();
        append_verification(
            &mut messages,
            "reused-id",
            json!({"sha256":"expected-digest"}),
        );
        let pending = if pending_duplicate {
            vec![verification_call("reused-id")]
        } else {
            append_verification(
                &mut messages,
                "reused-id",
                json!({"sha256":"expected-digest"}),
            );
            vec![]
        };
        let report = select_task_receipt_evidence(&contract(), &messages, &pending);
        assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
        assert_eq!(report.reason, "ambiguous_call_ids");
        assert!(report.checks[0].source.is_none());
    }
}

#[test]
fn unpaired_or_ambiguous_trace_never_selects_a_historical_success() {
    let mut complete = trace();
    append_verification(&mut complete, "real", json!({"sha256":"expected-digest"}));
    let mut absent = complete.clone();
    absent.pop();
    let mut wrong_id = complete.clone();
    wrong_id.last_mut().unwrap().tool_results[0].call_id = "invented".into();
    let mut orphan = trace();
    orphan.push(complete[2].clone());
    let mut duplicate = complete.clone();
    duplicate[1].tool_calls.push(verification_call("real"));
    let repeated_receipt = duplicate[2].tool_results[0].clone();
    duplicate[2].tool_results.push(repeated_receipt);
    let mut later_missing = complete.clone();
    later_missing
        .push(ChatMessage::assistant_tool_calls(vec![verification_call("later")]).unwrap());
    for messages in [absent, wrong_id, orphan, duplicate, later_missing] {
        let report = select_task_receipt_evidence(&contract(), &messages, &[]);
        assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
        assert_eq!(report.reason, "invalid_tool_trace");
        assert!(report.checks[0].source.is_none());
    }
}

#[test]
fn unknown_effect_anywhere_blocks_match_even_when_a_later_check_succeeded() {
    for code in [
        ToolFailureCode::ExecutionFailed,
        ToolFailureCode::TimedOut,
        ToolFailureCode::Cancelled,
    ] {
        for unknown_first in [false, true] {
            let mut messages = trace();
            let unknown = ToolResult::failure("unknown", code, "private failure body").unwrap();
            if !unknown_first {
                append_verification(&mut messages, "real", json!({"sha256":"expected-digest"}));
            }
            append_batch(
                &mut messages,
                vec![call(
                    "unknown",
                    "run_command",
                    json!({"command":"other work"}),
                )],
                vec![unknown],
            );
            if unknown_first {
                append_verification(&mut messages, "real", json!({"sha256":"expected-digest"}));
            }
            let report = select_task_receipt_evidence(&contract(), &messages, &[]);
            assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
            assert_eq!(report.reason, "unknown_tool_effect");
            assert_eq!(report.checks[0].status, TaskReceiptEvidenceStatus::Unknown);
            assert_eq!(report.checks[0].source.as_ref().unwrap().call_id, "real");
        }
    }
}

#[test]
fn pending_effect_keeps_existing_source_for_inspection_but_never_match() {
    let mut messages = trace();
    append_verification(&mut messages, "real", json!({"sha256":"expected-digest"}));
    let pending = [verification_call("not-returned")];
    let report = select_task_receipt_evidence(&contract(), &messages, &pending);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
    assert_eq!(report.reason, "pending_tool_effect");
    assert_eq!(report.checks[0].status, TaskReceiptEvidenceStatus::Unknown);
    assert_eq!(report.checks[0].source.as_ref().unwrap().call_id, "real");
}

#[test]
fn latest_matching_unknown_receipt_keeps_its_source_instead_of_old_success() {
    for code in [
        ToolFailureCode::ExecutionFailed,
        ToolFailureCode::TimedOut,
        ToolFailureCode::Cancelled,
    ] {
        let mut messages = trace();
        append_verification(&mut messages, "old", json!({"sha256":"expected-digest"}));
        let receipt = ToolResult::failure("new", code, "private unknown failure").unwrap();
        append_batch(
            &mut messages,
            vec![verification_call("new")],
            vec![receipt.clone()],
        );
        let report = select_task_receipt_evidence(&contract(), &messages, &[]);
        assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
        assert_eq!(report.reason, "unknown_tool_effect");
        let source = report.checks[0].source.as_ref().unwrap();
        assert_eq!(source.call_id, "new");
        assert_eq!(source.receipt_sha256, task_strategy_digest(&receipt));
    }
}

#[test]
fn historical_match_after_later_mutation_is_not_a_current_artifact_verification() {
    let frozen = contract();
    let mut messages = trace();
    append_verification(
        &mut messages,
        "old-check",
        json!({"sha256":"expected-digest"}),
    );
    append_batch(
        &mut messages,
        vec![call(
            "mutation",
            "write_file",
            json!({"path":"private-input.rs","content":"changed"}),
        )],
        vec![ToolResult::success("mutation", json!({"sha256":"different"})).unwrap()],
    );
    let report = select_task_receipt_evidence(&frozen, &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::HistoricalMatch);
    assert_eq!(
        report.checks[0].source.as_ref().unwrap().call_id,
        "old-check"
    );
    assert!(matches!(
        evaluate_completion_contract(
            &TaskCompletionContract::Receipts { contract: frozen },
            &messages,
            "产物已修改",
            None,
        ),
        TaskCompletionAssessment::Mismatch { reason } if reason == "unbound_final_batch"
    ));
}

#[test]
fn invalid_frozen_contract_is_not_weakened_for_historical_query() {
    let mut frozen = contract();
    frozen.require_final_batch = false;
    let before = serde_json::to_vec(&frozen).unwrap();
    let mut messages = trace();
    append_verification(&mut messages, "real", json!({"sha256":"expected-digest"}));
    let report = select_task_receipt_evidence(&frozen, &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::Unknown);
    assert_eq!(report.reason, "invalid_frozen_contract");
    assert!(report.checks[0].source.is_none());
    assert_eq!(serde_json::to_vec(&frozen).unwrap(), before);
}

#[test]
fn report_exposes_only_ids_digests_and_fixed_reasons_not_observation_or_body() {
    let mut frozen = contract();
    frozen.checks[0].expected = json!("private expected digest");
    let receipt = ToolResult::success(
        "real",
        json!({
            "sha256":"private expected digest",
            "content":"unredacted full file body",
            "stdout":"private command output",
        }),
    )
    .unwrap();
    let mut messages = trace();
    append_batch(
        &mut messages,
        vec![verification_call("real")],
        vec![receipt.clone()],
    );
    let report = select_task_receipt_evidence(&frozen, &messages, &[]);
    assert_eq!(report.status, TaskReceiptEvidenceStatus::HistoricalMatch);
    assert_eq!(
        report.checks[0].source.as_ref().unwrap().receipt_sha256,
        task_strategy_digest(&receipt)
    );
    let encoded = serde_json::to_string(&report).unwrap();
    for private in [
        "private-input.rs",
        "private expected digest",
        "unredacted full file body",
        "private command output",
    ] {
        assert!(!encoded.contains(private));
    }
    let repeated = select_task_receipt_evidence(&frozen, &messages, &[]);
    assert_eq!(report, repeated);
}
