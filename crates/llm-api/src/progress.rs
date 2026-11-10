//! 委派工具的有界运行事实及子轮主动登记的进展；不转发一般输出或模型思考。
use serde::Serialize;

/// 子轮主动登记的状态，只是报告，不是宿主对完成、验收或送达的确认。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedTaskProgressStatus {
    InProgress,
    Completed,
    Blocked,
}

/// 来自匹配成功 task_progress 回执的最新一条自述。
/// 宿主绑定父调用与接收范围；内容不能授予权限或作为业务验收证据。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DelegatedTaskProgressReport {
    pub call_id: String,
    pub status: DelegatedTaskProgressStatus,
    /// 原工具契约限制非空、至多 2048 UTF-8 字节；不拼接其它工具的正文。
    pub remaining: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DelegatedToolProgress {
    /// 已提交给工具执行器，仍可能处于校验或排队阶段。
    pub submitted_calls: usize,
    /// 已取得工具回执，不能据此推断业务目标成功。
    pub returned_calls: usize,
    pub failed_calls: usize,
    /// 超时、取消和执行异常的副作用结果尚未确认。
    pub unknown_calls: usize,
    pub pending_tool_names: Vec<String>,
    pub last_returned_tool_name: Option<String>,
    /// 当前子轮自愿提供的最新进展。None 保持旧观察接口的 JSON 形状。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_progress_report: Option<DelegatedTaskProgressReport>,
}

/// 宿主绑定的只读观察通道。实现必须有界、不阻塞，断开或丢弃不影响业务执行。
/// 每次提交累计快照；不得用回调请求模型或执行工具。
pub trait DelegatedProgressObserver: Send + Sync {
    fn observe(&self, progress: DelegatedToolProgress);
}
