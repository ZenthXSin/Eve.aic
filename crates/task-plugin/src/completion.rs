//! 只读取固定合同与真实执行证据，不请求模型，也不执行验收工具。
use eve_llm_api::{ChatMessage, ModelRequest, ToolFailureCode, ToolOutput};
use eve_task_api::*;

fn unknown(reason: impl Into<String>) -> TaskCompletionAssessment {
    TaskCompletionAssessment::Unverifiable {
        reason: reason.into(),
    }
}
fn mismatch(reason: impl Into<String>) -> TaskCompletionAssessment {
    TaskCompletionAssessment::Mismatch {
        reason: reason.into(),
    }
}
fn blocked(reason: impl Into<String>) -> TaskCompletionAssessment {
    TaskCompletionAssessment::Blocked {
        reason: reason.into(),
    }
}

/// 子代理与持久任务复用同一验收器；消息必须来自宿主收集的本次执行轨迹。
pub fn evaluate_completion_contract(
    contract: &TaskCompletionContract,
    messages: &[ChatMessage],
    result: &str,
    remaining: Option<&str>,
) -> TaskCompletionAssessment {
    if contract.validate().is_err() {
        return unknown("invalid_frozen_contract");
    }
    if let Some(reason) = execution_blocker(messages, remaining) {
        return reason;
    }
    if result.trim().is_empty() {
        return mismatch("empty_final_result");
    }
    if result.len() > MAX_RESULT_BYTES {
        return mismatch("final_result_exceeds_retained_limit");
    }
    match contract {
        TaskCompletionContract::Unspecified => unknown("missing_frozen_completion_contract"),
        TaskCompletionContract::Text {
            required_fragments,
            json_equals,
            valid_json,
            json_required_pointers,
        } => {
            if messages
                .iter()
                .any(|message| !message.tool_calls.is_empty())
            {
                return unknown("text_contract_cannot_verify_tool_effects");
            }
            if required_fragments
                .iter()
                .any(|fragment| !result.contains(fragment))
            {
                return mismatch("required_text_fragment_missing");
            }
            if *valid_json || json_equals.is_some() || !json_required_pointers.is_empty() {
                let Ok(actual) = crate::strict_json::from_slice(result.as_bytes()) else {
                    return mismatch("final_text_is_not_valid_json");
                };
                if json_equals
                    .as_ref()
                    .is_some_and(|expected| expected != &actual)
                {
                    return mismatch("final_json_does_not_equal_contract");
                }
                if json_required_pointers
                    .iter()
                    .any(|pointer| actual.pointer(pointer).is_none())
                {
                    return mismatch("required_json_pointer_missing");
                }
            }
            TaskCompletionAssessment::Verified
        }
        TaskCompletionContract::Receipts { contract } => evaluate_receipts(contract, messages),
    }
}

/// task_progress 是阻断/剩余工作的证据，永远不是成功的替代证明。
pub(super) fn execution_blocker(
    messages: &[ChatMessage],
    remaining: Option<&str>,
) -> Option<TaskCompletionAssessment> {
    if ModelRequest::validate_parts(messages, &[]).is_err() {
        return Some(unknown("missing_or_invalid_tool_receipts"));
    }
    let mut ids = std::collections::BTreeSet::new();
    if messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .any(|call| !ids.insert(&call.id))
    {
        return Some(unknown("ambiguous_tool_receipts"));
    }
    if let Some(value) = messages
        .iter()
        .rev()
        .flat_map(|message| message.tool_calls.iter().rev())
        .filter(|call| call.name == "task_progress")
        .find_map(|call| {
            messages
                .iter()
                .flat_map(|message| &message.tool_results)
                .find(|receipt| receipt.call_id == call.id)
                .and_then(|receipt| match &receipt.output {
                    ToolOutput::Success(value) => Some(value),
                    ToolOutput::Failure { .. } => None,
                })
        })
        && matches!(
            value.get("status").and_then(serde_json::Value::as_str),
            Some("blocked" | "in_progress")
        )
    {
        let reason = value
            .get("remaining")
            .and_then(serde_json::Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("task_progress_explicitly_blocked");
        return Some(blocked(reason));
    }
    if messages
        .iter()
        .flat_map(|message| &message.tool_results)
        .any(|receipt| {
            matches!(
                receipt.output,
                ToolOutput::Failure {
                    code: ToolFailureCode::ExecutionFailed
                        | ToolFailureCode::TimedOut
                        | ToolFailureCode::Cancelled,
                    ..
                }
            )
        })
    {
        return Some(unknown("unknown_tool_effect_not_replayed"));
    }
    if let Some(reason) = remaining.filter(|reason| !reason.trim().is_empty()) {
        return Some(blocked(format!("remaining_work: {reason}")));
    }
    None
}

fn evaluate_receipts(
    contract: &TaskEvaluationContract,
    messages: &[ChatMessage],
) -> TaskCompletionAssessment {
    let Some(batch) = messages
        .iter()
        .rev()
        .find(|message| !message.tool_calls.is_empty())
    else {
        return mismatch("no_verification_receipt");
    };
    // 不接受同批次的额外动作：工具可并发完成，额外动作可能在检查之后修改产物。
    if batch.tool_calls.iter().any(|call| {
        !contract
            .checks
            .iter()
            .any(|check| check.tool == call.name && check.arguments == call.arguments)
    }) {
        return mismatch("unbound_final_batch");
    }
    for check in &contract.checks {
        let calls: Vec<_> = batch
            .tool_calls
            .iter()
            .filter(|call| call.name == check.tool && call.arguments == check.arguments)
            .collect();
        if calls.len() != 1 {
            return mismatch("missing_or_ambiguous_verification_check");
        }
        let Some(receipt) = messages
            .iter()
            .flat_map(|message| &message.tool_results)
            .find(|receipt| receipt.call_id == calls[0].id)
        else {
            return unknown("missing_verification_receipt");
        };
        match &receipt.output {
            ToolOutput::Success(value) => match value.pointer(&check.output_pointer) {
                Some(actual) if actual != &check.expected => {
                    return mismatch("receipt_effect_mismatch");
                }
                Some(_) => {}
                None => return mismatch("missing_verification_observation_field"),
            },
            ToolOutput::Failure { .. } => return mismatch("verification_tool_failed"),
        }
    }
    TaskCompletionAssessment::Verified
}

/// 任务结算和重启恢复均调用此函数；旧的手工宿主任务仍须没有已知剩余工作或未知效果。
pub fn evaluate_task_completion(task: &Task, result: &str) -> TaskCompletionAssessment {
    if task.cancel_requested {
        return blocked("task_cancel_requested");
    }
    let execution = task.execution.as_ref();
    let contract = execution.and_then(|execution| execution.completion_contract.as_ref());
    let checkpoint = execution.and_then(|execution| execution.checkpoint.as_ref());
    let Some(checkpoint) = checkpoint else {
        return if contract.is_some() {
            unknown("missing_checkpoint")
        } else {
            TaskCompletionAssessment::Verified
        };
    };
    if checkpoint.validate(task.spec.max_steps as usize).is_err()
        || checkpoint.scope.user_id != task.owner.user_id
        || checkpoint.scope.session_id != task.owner.session_id
    {
        return unknown("invalid_completion_checkpoint");
    }
    if !checkpoint.resumable() {
        return unknown("unknown_tool_effect_not_replayed");
    }
    if let Some(reason) = execution_blocker(&checkpoint.messages, checkpoint.remaining.as_deref()) {
        return reason;
    }
    let Some(contract) = contract else {
        return TaskCompletionAssessment::Verified;
    };
    if checkpoint.result.as_deref() != Some(result) {
        return unknown("missing_or_mismatched_saved_final_result");
    }
    if task.usage.is_some_and(|usage| {
        usage.tool_calls as usize != checkpoint.used_tool_calls + checkpoint.delegated_tool_calls
            || usage.model_requests as usize
                != checkpoint.provider_requests + checkpoint.delegated_model_requests
    }) {
        return unknown("uncheckpointed_execution_usage");
    }
    if matches!(contract, TaskCompletionContract::Text { .. })
        && (checkpoint.used_tool_calls != 0 || checkpoint.delegated_tool_calls != 0)
    {
        return unknown("text_contract_cannot_verify_tool_effects");
    }
    evaluate_completion_contract(
        contract,
        &checkpoint.messages,
        result,
        checkpoint.remaining.as_deref(),
    )
}
