//! 委派工具的有界运行事实；不包含参数、输出、模型正文或思考。
use serde::Serialize;

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
}

/// 宿主绑定的只读观察通道。实现必须有界、不阻塞，断开或丢弃不影响业务执行。
/// 每次提交累计快照；不得用回调请求模型或执行工具。
pub trait DelegatedProgressObserver: Send + Sync {
    fn observe(&self, progress: DelegatedToolProgress);
}
