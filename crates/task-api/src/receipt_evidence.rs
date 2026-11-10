//! 历史回执查询：只说明冻结检查在原执行轨迹中的观察，不证明当前产物有效。
use serde::{Deserialize, Serialize};

/// 历史观察类别；不能转换为任务完成或当前产物验证结论。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskReceiptEvidenceStatus {
    HistoricalMatch,
    HistoricalMismatch,
    /// 没有精确匹配冻结工具与参数的调用，不等于允许重新执行。
    Missing,
    /// 来源不唯一、缺观察字段、断配或存在未知效果；不能自动重放。
    Unknown,
}

/// 原始真实回执的定位信息；身份与执行 ID 由可信组合层另行绑定。
/// 不包含参数、期望观察、实际观察或回执正文，也没有当前产物快照凭证。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReceiptEvidenceReference {
    pub call_id: String,
    /// 以 1 起算，包含原轨迹的全部已配对工具批次。
    pub batch_ordinal: usize,
    pub tool: String,
    pub arguments_sha256: String,
    /// 完整 ToolResult 的序列化摘要，包含原 call_id 和真实 output。
    pub receipt_sha256: String,
}

/// 按执行前冻结合同顺序逐项给出最近一次精确调用的历史观察。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReceiptEvidenceCheck {
    /// 以 0 起算，对应冻结合同的 checks 下标。
    pub check_index: usize,
    /// 完整冻结 check（tool/arguments/pointer/expected）的摘要。
    pub check_sha256: String,
    pub status: TaskReceiptEvidenceStatus,
    /// 仅宿主固定原因码，不携带模型文字或工具错误正文。
    pub reason: String,
    /// 全局未知效果仍可保留已配对历史来源，但不再返回 HistoricalMatch。
    pub source: Option<TaskReceiptEvidenceReference>,
}

/// 只读历史查询结果；不是 TaskCompletionAssessment，也不更新冻结合同或任务。
/// 调用者必须先从同一受信执行读取合同和完整轨迹，核对身份、执行 ID 与存储。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReceiptEvidenceReport {
    pub status: TaskReceiptEvidenceStatus,
    /// 仅宿主固定原因码。未知优先于不符，不符优先于缺失。
    pub reason: String,
    pub contract_sha256: String,
    pub checks: Vec<TaskReceiptEvidenceCheck>,
}
