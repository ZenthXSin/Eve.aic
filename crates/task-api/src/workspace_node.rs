//! 工作区测试的可信运行见证与独立收尾观察。纯读校验不访问当前源码或运行器。
use crate::{
    TaskError, TaskFrozenNodeOutcome, TaskOwner, TaskResult, task_strategy_digest,
    valid_strategy_digest, validate_owner,
};
use eve_llm_api::{ModelRequest, ModelSession, ToolOutput, TurnCheckpoint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const WORKSPACE_NODE_TEST_TOOL: &str = "run_workspace_node_test";

/// 只接受工作区内的规范相对路径；目录的唯一根写法是 `.`。
/// 不隐式排除文件，也不把绝对宿主目录转成普通工作区。
pub fn validate_workspace_node_selector(
    directory: &str,
    entry: &str,
    args: &[String],
) -> TaskResult<()> {
    fn relative(path: &str) -> bool {
        !path.is_empty()
            && path.len() <= 4096
            && !path.contains(['\\', ':'])
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".." | ".git" | ".eve-developer"))
    }
    if (directory != "." && !relative(directory))
        || !relative(entry)
        || entry.len() > 512
        || !matches!(
            std::path::Path::new(entry)
                .extension()
                .and_then(|value| value.to_str()),
            Some("js" | "cjs" | "mjs")
        )
        || args.len() > 16
        || args
            .iter()
            .fold(0usize, |total, arg| total.saturating_add(arg.len()))
            > 4096
        || args.iter().any(|arg| arg.contains('\0'))
    {
        return Err(TaskError::InvalidInput);
    }
    Ok(())
}

/// 由受信宿主从实际 FrozenNodeRunner 回执产生。可反序列化只为读取持久记录；
/// 模型工具 JSON、模型填写的摘要与成功文字不能作为该类型的可信来源。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceNodeEvidence {
    pub owner: TaskOwner,
    pub model_session: ModelSession,
    pub call_id: String,
    pub arguments_sha256: String,
    pub tool_result_sha256: String,
    pub input_sha256: String,
    pub workspace_sha256: String,
    pub directory: String,
    pub entry: String,
    pub args: Vec<String>,
    pub runtime_sha256: String,
    pub command_sha256: String,
    pub outcome: TaskFrozenNodeOutcome,
}

/// 收尾时独立采集，不覆盖测试当时的输入。绑定完整最终检查点，不能借用其他执行的观察。
/// observed_at_ms 是宿主观察时间；只证明当时一致，不表示工作区此后一直未变。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceObservation {
    pub owner: TaskOwner,
    pub model_session: ModelSession,
    pub checkpoint_sha256: String,
    pub workspace_sha256: String,
    pub directory: String,
    pub entry: String,
    pub args: Vec<String>,
    pub input_sha256: String,
    pub observed_at_ms: u64,
}

/// 仅注入可信宿主，同步读取运行器持有的原见证，不运行测试或读取当前工作区。
pub trait WorkspaceNodeEvidenceSource: Send + Sync {
    fn capture(
        &self,
        owner: &TaskOwner,
        frame: &TurnCheckpoint,
    ) -> TaskResult<Vec<WorkspaceNodeEvidence>>;
}

pub fn workspace_node_checkpoint_sha256(frame: &TurnCheckpoint) -> String {
    task_strategy_digest(&("eve.workspace_node.checkpoint.v1", frame))
}

fn validate_frame_identity<'a>(
    owner: &TaskOwner,
    frame: &'a TurnCheckpoint,
) -> TaskResult<&'a ModelSession> {
    validate_owner(owner)?;
    if frame.scope.user_id != owner.user_id
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
    Ok(session)
}

/// 校验精确原调用/结果及工作区选择器；不从可见结果里的 exit_code/verified 推导成功。
pub fn validate_workspace_node_evidence(
    owner: &TaskOwner,
    frame: &TurnCheckpoint,
    evidence: &[WorkspaceNodeEvidence],
) -> TaskResult<()> {
    validate_owner(owner)?;
    if evidence.is_empty() {
        return Ok(());
    }
    if evidence.len() > frame.used_tool_calls {
        return Err(TaskError::CorruptState);
    }
    let session = validate_frame_identity(owner, frame)?;
    let mut ids = BTreeSet::new();
    for item in evidence {
        validate_workspace_node_selector(&item.directory, &item.entry, &item.args)
            .map_err(|_| TaskError::CorruptState)?;
        if &item.owner != owner
            || &item.model_session != session
            || !ids.insert(&item.call_id)
            || [
                &item.arguments_sha256,
                &item.tool_result_sha256,
                &item.input_sha256,
                &item.workspace_sha256,
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
        let arguments = call.arguments.as_object().ok_or(TaskError::CorruptState)?;
        let directory = match arguments.get("directory") {
            None => ".",
            Some(value) => value.as_str().ok_or(TaskError::CorruptState)?,
        };
        let args: Vec<String> = match arguments.get("args") {
            None => vec![],
            Some(value) => {
                serde_json::from_value(value.clone()).map_err(|_| TaskError::CorruptState)?
            }
        };
        if call.name != WORKSPACE_NODE_TEST_TOOL
            || arguments
                .keys()
                .any(|key| !matches!(key.as_str(), "directory" | "entry" | "args"))
            || directory != item.directory
            || arguments.get("entry").and_then(serde_json::Value::as_str)
                != Some(item.entry.as_str())
            || args != item.args
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

pub fn merge_workspace_node_evidence(
    owner: &TaskOwner,
    frame: &TurnCheckpoint,
    saved: &[WorkspaceNodeEvidence],
    captured: Vec<WorkspaceNodeEvidence>,
) -> TaskResult<Vec<WorkspaceNodeEvidence>> {
    validate_workspace_node_evidence(owner, frame, saved)?;
    validate_workspace_node_evidence(owner, frame, &captured)?;
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
    validate_workspace_node_evidence(owner, frame, &merged)?;
    Ok(merged)
}

pub fn validate_workspace_observation(
    owner: &TaskOwner,
    frame: &TurnCheckpoint,
    current: &WorkspaceObservation,
) -> TaskResult<()> {
    let session = validate_frame_identity(owner, frame)?;
    validate_workspace_node_selector(&current.directory, &current.entry, &current.args)
        .map_err(|_| TaskError::CorruptState)?;
    if &current.owner != owner
        || &current.model_session != session
        || !frame.resumable()
        || frame
            .result
            .as_deref()
            .is_none_or(|result| result.trim().is_empty())
        || current.checkpoint_sha256 != workspace_node_checkpoint_sha256(frame)
        || [&current.workspace_sha256, &current.input_sha256]
            .into_iter()
            .any(|value| !valid_strategy_digest(value))
        || current.observed_at_ms == 0
    {
        return Err(TaskError::CorruptState);
    }
    Ok(())
}
