//! 在派发时冻结的任务验收条件；执行模型不能在看到结果后改写成功标准。
use crate::{MAX_RESULT_BYTES, TaskError, TaskEvaluationContract, TaskResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_COMPLETION_CONTRACT_BYTES: usize = 32 * 1024;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCompletionContract {
    /// 新模型任务没有可机械验收的标准时保留原目标，不把模型最终答复当作完成。
    Unspecified,
    /// 实际工具回执必须满足固定检查；自述进度和 echo 不能充当验收。
    Receipts { contract: TaskEvaluationContract },
    /// 完整冻结纯 JS 输入在固定运行时的测试；不证明活工作区或外部交付。
    FrozenNodeTest {
        input_sha256: String,
        runtime_sha256: String,
        command_sha256: String,
    },
    /// 固定工作区与测试选择器；执行时冻结输入，收尾观察必须仍与测试输入一致。
    /// 仅证明该目录在收尾观察时与通过测试的包一致，不证明外部交付或以后不再改变。
    WorkspaceNodeTest {
        workspace_sha256: String,
        directory: String,
        entry: String,
        args: Vec<String>,
        runtime_sha256: String,
        command_sha256: String,
    },
    /// 仅用于没有调用任何工具的纯文本交付物，不能证明外部动作已发生。
    Text {
        #[serde(default)]
        required_fragments: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        json_equals: Option<Value>,
        #[serde(default)]
        valid_json: bool,
        #[serde(default)]
        json_required_pointers: Vec<String>,
    },
}

impl TaskCompletionContract {
    pub fn validate(&self) -> TaskResult<()> {
        if serde_json::to_vec(self)
            .map_err(|_| TaskError::InvalidInput)?
            .len()
            > MAX_COMPLETION_CONTRACT_BYTES
        {
            return Err(TaskError::InvalidInput);
        }
        match self {
            Self::Unspecified => Ok(()),
            Self::Receipts { contract } => contract.validate(),
            Self::FrozenNodeTest {
                input_sha256,
                runtime_sha256,
                command_sha256,
            } => {
                if [input_sha256, runtime_sha256, command_sha256]
                    .into_iter()
                    .all(|value| crate::valid_strategy_digest(value))
                {
                    Ok(())
                } else {
                    Err(TaskError::InvalidInput)
                }
            }
            Self::WorkspaceNodeTest {
                workspace_sha256,
                directory,
                entry,
                args,
                runtime_sha256,
                command_sha256,
            } => {
                crate::validate_workspace_node_selector(directory, entry, args)?;
                if [workspace_sha256, runtime_sha256, command_sha256]
                    .into_iter()
                    .all(|value| crate::valid_strategy_digest(value))
                {
                    Ok(())
                } else {
                    Err(TaskError::InvalidInput)
                }
            }
            Self::Text {
                required_fragments,
                json_equals,
                valid_json,
                json_required_pointers,
            } => {
                if required_fragments.len() > 32
                    || json_required_pointers.len() > 32
                    || (required_fragments.is_empty()
                        && json_equals.is_none()
                        && !valid_json
                        && json_required_pointers.is_empty())
                    || required_fragments.iter().any(|fragment| {
                        fragment.trim().is_empty() || fragment.len() > MAX_RESULT_BYTES
                    })
                    || json_required_pointers.iter().any(|pointer| {
                        pointer.len() > 256
                            || (!pointer.is_empty() && !pointer.starts_with('/'))
                            || pointer.chars().any(char::is_control)
                    })
                {
                    return Err(TaskError::InvalidInput);
                }
                Ok(())
            }
        }
    }
}

/// 无法验证与观察到不匹配分开：只有后者可以使用原预算自动修复。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCompletionAssessment {
    Verified,
    Mismatch { reason: String },
    Unverifiable { reason: String },
    Blocked { reason: String },
}
impl TaskCompletionAssessment {
    pub fn reason(&self) -> &str {
        match self {
            Self::Verified => "contract_satisfied",
            Self::Mismatch { reason }
            | Self::Unverifiable { reason }
            | Self::Blocked { reason } => reason,
        }
    }
    pub fn verified(&self) -> bool {
        matches!(self, Self::Verified)
    }
}
