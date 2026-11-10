//! 仅核对本次真实测试见证及最终检查点绑定的工作区观察；不查盘、不运行模型或测试。
use eve_task_api::*;

fn unknown(reason: &str) -> TaskCompletionAssessment {
    TaskCompletionAssessment::Unverifiable {
        reason: reason.into(),
    }
}
fn mismatch(reason: &str) -> TaskCompletionAssessment {
    TaskCompletionAssessment::Mismatch {
        reason: reason.into(),
    }
}

pub fn evaluate_workspace_node_completion(
    contract: &TaskCompletionContract,
    owner: &TaskOwner,
    checkpoint: &eve_llm_api::TurnCheckpoint,
    max_tool_calls: usize,
    result: &str,
    evidence: &[WorkspaceNodeEvidence],
    current: Option<&WorkspaceObservation>,
) -> TaskCompletionAssessment {
    if contract.validate().is_err() {
        return unknown("invalid_frozen_contract");
    }
    if checkpoint.validate(max_tool_calls).is_err()
        || checkpoint.scope.user_id != owner.user_id
        || checkpoint.scope.session_id != owner.session_id
    {
        return unknown("invalid_completion_checkpoint");
    }
    if !checkpoint.resumable() {
        return unknown("unknown_tool_effect_not_replayed");
    }
    if checkpoint.result.as_deref() != Some(result) {
        return unknown("missing_or_mismatched_saved_final_result");
    }
    let TaskCompletionContract::WorkspaceNodeTest {
        workspace_sha256,
        directory,
        entry,
        args,
        runtime_sha256,
        command_sha256,
    } = contract
    else {
        return unknown("not_workspace_node_contract");
    };
    if let Some(blocker) =
        crate::completion::execution_blocker(&checkpoint.messages, checkpoint.remaining.as_deref())
    {
        return blocker;
    }
    if result.trim().is_empty() || result.len() > MAX_RESULT_BYTES {
        return mismatch("invalid_final_result");
    }
    if validate_workspace_node_evidence(owner, checkpoint, evidence).is_err() {
        return unknown("invalid_trusted_workspace_node_evidence");
    }
    if checkpoint
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.name == WORKSPACE_NODE_TEST_TOOL)
        .any(|call| !evidence.iter().any(|item| item.call_id == call.id))
    {
        return unknown("missing_trusted_workspace_node_evidence");
    }
    // 不能借用更早成功掩盖后来未知/取消的测试，即使它没有匹配目标。
    if evidence.iter().any(|item| {
        matches!(
            item.outcome,
            TaskFrozenNodeOutcome::Cancelled | TaskFrozenNodeOutcome::Unknown
        )
    }) {
        return unknown("unknown_workspace_node_effect_not_replayed");
    }
    let matches_target = |item: &&WorkspaceNodeEvidence| {
        &item.workspace_sha256 == workspace_sha256
            && &item.directory == directory
            && &item.entry == entry
            && &item.args == args
            && &item.runtime_sha256 == runtime_sha256
            && &item.command_sha256 == command_sha256
    };
    let latest_batch = checkpoint.messages.iter().rev().find_map(|message| {
        let matched: Vec<_> = message
            .tool_calls
            .iter()
            .filter_map(|call| {
                evidence
                    .iter()
                    .find(|item| item.call_id == call.id)
                    .filter(matches_target)
            })
            .collect();
        (!matched.is_empty()).then_some(matched)
    });
    let latest = match latest_batch.as_deref() {
        Some([item]) => *item,
        Some(_) => return unknown("ambiguous_workspace_node_test_batch"),
        None => return unknown("missing_matching_workspace_node_evidence"),
    };
    if !matches!(
        latest.outcome,
        TaskFrozenNodeOutcome::Exited {
            exit_code: Some(0),
            signal: None,
        }
    ) {
        return mismatch("workspace_node_test_failed");
    }
    let Some(current) = current else {
        return unknown("missing_current_workspace_observation");
    };
    if validate_workspace_observation(owner, checkpoint, current).is_err() {
        return unknown("invalid_current_workspace_observation");
    }
    if &current.workspace_sha256 != workspace_sha256
        || &current.directory != directory
        || &current.entry != entry
        || &current.args != args
    {
        return unknown("mismatched_current_workspace_observation");
    }
    if current.input_sha256 != latest.input_sha256 {
        return mismatch("workspace_changed_since_test");
    }
    TaskCompletionAssessment::Verified
}
