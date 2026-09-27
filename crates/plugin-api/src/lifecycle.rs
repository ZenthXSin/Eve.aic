//! 宿主生命周期操作契约；取消等待与停止插件是两个不同动作。

use crate::{PluginError, PluginFuture, PluginId, PluginResult};
use std::fmt;

/// 单个 Runtime 内唯一的操作 ID，不能跨 Runtime 使用。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LifecycleOperationId(u64);

impl LifecycleOperationId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LifecycleOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleRequest {
    Start(PluginId),
    StartAll,
    Stop(PluginId),
    StopAll,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleOperationState {
    Running,
    /// 操作及其错误收尾已返回；Err 仍可能包含未退出任务或清理失败。
    Completed(PluginResult<()>),
    /// 执行器关闭等原因使操作中断，不代表已完成清理。
    Interrupted(PluginError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleOperation {
    pub id: LifecycleOperationId,
    pub request: LifecycleRequest,
    pub state: LifecycleOperationState,
}

/// 面向宿主的可替换接口；不通过 PluginContext 授予插件 Runtime 控制权。
pub trait RuntimeLifecycle: Send + Sync {
    /// 等待串行准入；未准入前取消无副作用，准入后独立执行至返回。
    /// 返回 ID 前必须完成登记，不能留下不可查询的操作。
    fn submit(&self, request: LifecycleRequest) -> PluginFuture<'_, LifecycleOperationId>;
    /// 返回按操作 ID 排序的独立快照，不因读取而清除错误。
    fn operations(&self) -> PluginResult<Vec<LifecycleOperation>>;
    /// 可反复等待；丢弃等待不取消操作，也不删除结果。
    fn wait(&self, id: LifecycleOperationId) -> PluginFuture<'_, LifecycleOperation>;
    /// 仅移除已终止操作；运行中返回错误，已不存在时返回 false。
    fn acknowledge(&self, id: LifecycleOperationId) -> PluginResult<bool>;
}
