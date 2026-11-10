//! 宿主持有的冻结测试见证。持久化后只核原轨迹，不查询当前源码或运行环境。
use crate::{
    TaskError, TaskOwner, TaskResult, task_strategy_digest, valid_strategy_digest, validate_owner,
};
use eve_llm_api::{ModelRequest, ModelSession, ToolOutput, TurnCheckpoint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const FROZEN_NODE_TEST_TOOL: &str = "run_frozen_node_test";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskFrozenNodeOutcome {
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Cancelled,
    Unknown,
}

/// 只能由受信组合层从可信宿主持有的真实运行回执采集。可序列化不表示模型拥有写入权限。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFrozenNodeEvidence {
    pub owner: TaskOwner,
    pub model_session: ModelSession,
    pub call_id: String,
    pub arguments_sha256: String,
    pub tool_result_sha256: String,
    pub input_sha256: String,
    pub runtime_sha256: String,
    pub command_sha256: String,
    pub outcome: TaskFrozenNodeOutcome,
}

/// 仅注入可信宿主；不得发布为工具或从 ToolOutput JSON 构造凭证。
/// capture 只读可信宿主持有的真实运行回执，不执行测试、不查活源码、不重置预算。
/// 可以只返回新增见证；持久层负责与旧见证合并并禁止替换。
pub trait TaskCompletionEvidenceSource: Send + Sync {
    fn capture(
        &self,
        owner: &TaskOwner,
        frame: &TurnCheckpoint,
    ) -> TaskResult<Vec<TaskFrozenNodeEvidence>>;
}

/// 检查来源、唯一性及完整调用/回执摘要。不会赋予任意工具 JSON 可信来源。
pub fn validate_frozen_node_evidence(
    owner: &TaskOwner,
    frame: &TurnCheckpoint,
    evidence: &[TaskFrozenNodeEvidence],
) -> TaskResult<()> {
    validate_owner(owner)?;
    if evidence.is_empty() {
        return Ok(());
    }
    if evidence.len() > frame.used_tool_calls
        || frame.scope.user_id != owner.user_id
        || frame.scope.session_id != owner.session_id
        || ModelRequest::validate_parts(&frame.messages, &[]).is_err()
    {
        return Err(TaskError::CorruptState);
    }
    let session = frame
        .model_session
        .as_ref()
        .ok_or(TaskError::CorruptState)?;
    session
        .validate_scope(Some(&frame.scope))
        .map_err(|_| TaskError::CorruptState)?;
    if session.turn_id().is_none() {
        return Err(TaskError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    for item in evidence {
        if &item.owner != owner
            || &item.model_session != session
            || !ids.insert(&item.call_id)
            || [
                &item.arguments_sha256,
                &item.tool_result_sha256,
                &item.input_sha256,
                &item.runtime_sha256,
                &item.command_sha256,
            ]
            .into_iter()
            .any(|value| !valid_strategy_digest(value))
            || matches!(&item.outcome, TaskFrozenNodeOutcome::Exited { exit_code, signal }
                if exit_code.is_some() == signal.is_some() || signal.is_some_and(|value| value <= 0))
        {
            return Err(TaskError::CorruptState);
        }
        let calls: Vec<_> = frame
            .messages
            .iter()
            .enumerate()
            .flat_map(|(index, message)| message.tool_calls.iter().map(move |call| (index, call)))
            .filter(|(_, call)| call.id == item.call_id)
            .collect();
        if calls.len() != 1 {
            return Err(TaskError::CorruptState);
        }
        let (index, call) = calls[0];
        let receipt = frame
            .messages
            .get(index + 1)
            .and_then(|message| {
                message
                    .tool_results
                    .iter()
                    .find(|receipt| receipt.call_id == item.call_id)
            })
            .ok_or(TaskError::CorruptState)?;
        if call.name != FROZEN_NODE_TEST_TOOL
            || task_strategy_digest(&call.arguments) != item.arguments_sha256
            || task_strategy_digest(receipt) != item.tool_result_sha256
            || (matches!(item.outcome, TaskFrozenNodeOutcome::Exited { .. })
                && !matches!(receipt.output, ToolOutput::Success(_)))
        {
            return Err(TaskError::CorruptState);
        }
    }
    Ok(())
}

/// 新见证不能替换旧调用；旧见证也必须仍绑定新检查点中的同一原回执。
pub fn merge_frozen_node_evidence(
    owner: &TaskOwner,
    frame: &TurnCheckpoint,
    saved: &[TaskFrozenNodeEvidence],
    captured: Vec<TaskFrozenNodeEvidence>,
) -> TaskResult<Vec<TaskFrozenNodeEvidence>> {
    validate_frozen_node_evidence(owner, frame, saved)?;
    validate_frozen_node_evidence(owner, frame, &captured)?;
    let mut merged = saved.to_vec();
    for item in captured {
        if let Some(old) = merged.iter().find(|old| old.call_id == item.call_id) {
            if old != &item {
                return Err(TaskError::CorruptState);
            }
        } else {
            merged.push(item);
        }
    }
    validate_frozen_node_evidence(owner, frame, &merged)?;
    Ok(merged)
}

/// 后台固定候选任务的执行身份；与宿主 Worker 的 task 分支一致，不能借用同 owner 的其他任务。
pub fn frozen_node_task_session(owner: &TaskOwner, task_id: &str) -> TaskResult<ModelSession> {
    validate_owner(owner)?;
    crate::validate_id(task_id)?;
    let scope = eve_llm_api::ContextScope {
        session_id: owner.session_id.clone(),
        user_id: owner.user_id.clone(),
    };
    ModelSession::anchored("task", Some(&scope), task_id)
        .and_then(|session| session.with_turn(task_id))
        .map_err(|_| TaskError::InvalidInput)
}

/// 新策略的结构化未知结果也属于未知副作用，不能只看可见 ToolOutput 是否 Success。
pub fn frozen_node_effects_unknown(execution: &crate::TaskExecution) -> bool {
    matches!(
        execution.completion_contract,
        Some(crate::TaskCompletionContract::FrozenNodeTest { .. })
    ) && execution.frozen_node_evidence.iter().any(|item| {
        matches!(
            item.outcome,
            TaskFrozenNodeOutcome::Unknown | TaskFrozenNodeOutcome::Cancelled
        )
    })
}
