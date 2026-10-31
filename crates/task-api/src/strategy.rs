//! 通用任务策略：宿主固定评价条件，实际回执结算，同题对照决定晋升。
use crate::{
    Task, TaskError, TaskOwner, TaskResult, TaskSnapshot, TaskUsage, validate_id, validate_owner,
};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const TASK_STRATEGY_PLUGIN_ID: &str = "eve.task-strategy";
pub const TASK_STRATEGY_STATE_KEY: &str = "strategies.v1";
pub const MAX_TASK_STRATEGY_RECORDS: usize = 256;
pub const MAX_TASK_STRATEGY_BYTES: usize = 4 * 1024 * 1024;
pub const RECEIPT_EVALUATOR_VERSION: &str = "task-receipts:v1";

pub fn task_strategy_digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("serializable strategy data");
    let mut context = Context::new(&SHA256);
    context.update(&bytes);
    context
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 不含任务 ID 或结果，预算、完整委派上下文也参加对照绑定。
pub fn task_strategy_input(task: &Task) -> String {
    task_strategy_digest(&(
        task.spec.instructions.trim(),
        task.spec.max_steps,
        &task.spec.context,
    ))
}

pub fn valid_strategy_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyScope {
    pub owner: TaskOwner,
    pub family: String,
    /// 宿主模型、提示、工具/角色清单、工作目录及环境版本的联合摘要。
    pub runtime_sha256: String,
    pub evaluator_version: String,
}
impl TaskStrategyScope {
    pub fn validate(&self) -> TaskResult<()> {
        validate_owner(&self.owner)?;
        validate_id(&self.family)?;
        if !valid_strategy_digest(&self.runtime_sha256)
            || self.evaluator_version != RECEIPT_EVALUATOR_VERSION
        {
            return Err(TaskError::InvalidInput);
        }
        Ok(())
    }
}

/// 由可信宿主在执行前提供；模型不能事后选择“哪个工具成功了”作为验收条件。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReceiptCheck {
    pub tool: String,
    pub arguments: Value,
    pub output_pointer: String,
    pub expected: Value,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEvaluationContract {
    pub checks: Vec<TaskReceiptCheck>,
    /// 评价点必须为最后一个工具批次，防止先验证、再改坏产物仍计为成功。
    pub require_final_batch: bool,
}
impl TaskEvaluationContract {
    pub fn validate(&self) -> TaskResult<()> {
        if self.checks.is_empty() || self.checks.len() > 16 || !self.require_final_batch {
            return Err(TaskError::InvalidInput);
        }
        let mut unique = std::collections::BTreeSet::new();
        for check in &self.checks {
            validate_id(&check.tool)?;
            if matches!(
                check.tool.as_str(),
                "echo" | "task_progress" | "run_subagent" | "dispatch_task"
            ) || !check.arguments.is_object()
                || serde_json::to_vec(&check.arguments)
                    .map_err(|_| TaskError::InvalidInput)?
                    .len()
                    > 8192
                || check.output_pointer.len() > 256
                || (!check.output_pointer.is_empty() && !check.output_pointer.starts_with('/'))
                || serde_json::to_vec(&check.expected)
                    .map_err(|_| TaskError::InvalidInput)?
                    .len()
                    > 4096
                || !unique.insert(task_strategy_digest(check))
            {
                return Err(TaskError::InvalidInput);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyProposal {
    pub scope: TaskStrategyScope,
    /// 每次提议的不可变来源 ID；同一 ID 不允许换内容。
    pub source_id: String,
    pub source_input_sha256: String,
    pub source_evidence_sha256: String,
    pub guidance: String,
}
impl TaskStrategyProposal {
    pub fn validate(&self) -> TaskResult<()> {
        self.scope.validate()?;
        validate_id(&self.source_id)?;
        if !valid_strategy_digest(&self.source_input_sha256)
            || !valid_strategy_digest(&self.source_evidence_sha256)
            || self.guidance.trim().is_empty()
            || self.guidance.len() > 4096
            || self.guidance.chars().any(|c| c.is_control() && c != '\n')
        {
            return Err(TaskError::InvalidInput);
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyVersion {
    pub version: u64,
    pub parent: Option<u64>,
    pub proposal: TaskStrategyProposal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskStrategyStatus {
    Candidate,
    Active,
    Retired,
    Rejected,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyView {
    pub strategy: TaskStrategyVersion,
    pub status: TaskStrategyStatus,
    pub paired_successes: usize,
}
/// 两个臂只能在宿主授权的独立任务中运行；账本本身绝不自动重跑实际任务。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskStrategyArm {
    Control {
        candidate: u64,
    },
    Trial {
        candidate: u64,
    },
    /// 已晋升版本的日常复用，不增加独立验证次数。
    Apply {
        candidate: u64,
    },
}
impl TaskStrategyArm {
    pub fn candidate(self) -> u64 {
        match self {
            Self::Control { candidate } | Self::Trial { candidate } | Self::Apply { candidate } => {
                candidate
            }
        }
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyAssignment {
    pub scope: TaskStrategyScope,
    pub contract: TaskEvaluationContract,
    pub arm: TaskStrategyArm,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategySelection {
    pub task_id: String,
    pub input_sha256: String,
    pub assignment: TaskStrategyAssignment,
    pub selected_version: Option<u64>,
    pub max_steps: u32,
    /// 一次选择终结后不重新抽样；安全恢复继续使用同一选择。
    pub observation: Option<TaskStrategyObservation>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskEffect {
    Verified,
    Failed,
    Inconclusive,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyObservation {
    pub effect: TaskEffect,
    pub reason: String,
    pub evidence_sha256: String,
    pub usage: TaskUsage,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategySnapshot {
    /// 执行前固定的经验来源，和留出评价记录分离；旧账本按空集合升级。
    #[serde(default)]
    pub sources: Vec<TaskStrategySource>,
    pub versions: Vec<TaskStrategyVersion>,
    pub selections: Vec<TaskStrategySelection>,
}
impl TaskStrategySnapshot {
    /// 来源身份由可信策略账本固定；保留已结算来源以继续核对历史证据。
    pub fn evidence_keys(&self) -> TaskResult<Vec<crate::TaskEvidenceKey>> {
        if self
            .sources
            .len()
            .checked_add(self.selections.len())
            .is_none_or(|count| count > MAX_TASK_STRATEGY_RECORDS)
        {
            return Err(TaskError::LimitReached);
        }
        let keys: Vec<_> = self
            .selections
            .iter()
            .map(|selected| crate::TaskEvidenceKey {
                id: selected.task_id.clone(),
                owner: selected.assignment.scope.owner.clone(),
            })
            .chain(self.sources.iter().map(|source| crate::TaskEvidenceKey {
                id: source.task_id.clone(),
                owner: source.scope.owner.clone(),
            }))
            .collect();
        Ok(crate::TaskEvidenceSnapshot::keys(&keys)?
            .into_iter()
            .map(|(id, owner)| crate::TaskEvidenceKey { id, owner })
            .collect())
    }
}
/// 只有宿主在首次执行前冻结评价条件的任务，才能成为自动经验来源。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategySource {
    pub task_id: String,
    pub input_sha256: String,
    pub scope: TaskStrategyScope,
    pub contract: TaskEvaluationContract,
    pub max_steps: u32,
    pub observation: Option<TaskStrategyObservation>,
    /// 已消费来源不因候选被否决或重启而再次生成。
    pub extracted: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyGuidance {
    pub version: Option<u64>,
    pub instructions: Option<String>,
    pub evaluation: TaskEvaluationContract,
}
impl fmt::Debug for TaskStrategyGuidance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskStrategyGuidance")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// 宿主接口不暴露为对话工具；策略建议不能自己固定验收标准。
pub trait TaskStrategyPolicy: Send + Sync + fmt::Debug {
    fn proposals(&self) -> TaskResult<Vec<TaskStrategyProposal>> {
        Ok(Vec::new())
    }
    /// 首次执行前提供经验来源验收条件。默认不自动采集，禁止事后挑选成功标准。
    fn source_contract(&self, _task: &Task) -> TaskResult<Option<TaskEvaluationContract>> {
        Ok(None)
    }
    /// 每次准入和结算都重新读取，配置/权限变化后结果记为不确定。
    fn scope(&self, task: &Task) -> TaskResult<Option<TaskStrategyScope>>;
    fn assign(
        &self,
        task: &Task,
        snapshot: &TaskStrategySnapshot,
    ) -> TaskResult<Option<TaskStrategyAssignment>>;
}
/// 组合层每次读取实际模型、提示、工具/角色、环境和授权版本，不能返回启动时的旧缓存。
pub trait TaskStrategyEnvironment: Send + Sync + fmt::Debug {
    fn scope(&self, task: &Task) -> TaskResult<Option<TaskStrategyScope>>;
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStrategyCase {
    pub scope: TaskStrategyScope,
    pub input_sha256: String,
    pub contract: TaskEvaluationContract,
}
pub trait TaskStrategyLearning: Send + Sync {
    fn snapshot(&self) -> TaskResult<TaskStrategySnapshot>;
    fn propose(&self, proposal: TaskStrategyProposal) -> TaskResult<TaskStrategyVersion>;
    fn prepare(&self, task: &Task) -> TaskResult<Option<TaskStrategyGuidance>>;
    fn reconcile(&self, tasks: &TaskSnapshot) -> TaskResult<()>;
    /// 原生实现按冻结来源读取证据；旧策略仍接收完整快照，不能收到伪装的局部快照。
    fn reconcile_from(&self, tasks: &dyn crate::TaskAdmin) -> TaskResult<()> {
        self.reconcile(&tasks.snapshot()?)
    }
}
