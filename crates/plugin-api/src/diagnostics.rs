//! 宿主诊断契约。只读查询不授予插件 Runtime 控制权限。

use crate::{PluginError, PluginId, PluginInfo, PluginResult, TaskInfo};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginState {
    Registered,
    WaitingDependencies,
    Starting,
    Active,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginStatus {
    pub info: PluginInfo,
    pub state: PluginState,
}

/// 宿主可通过 Trait 对象接入诊断界面，不必依赖 Kernel 私有结构。
pub trait RuntimeInspector: Send + Sync {
    /// 按插件 ID 排序；逐项取快照，不承诺所有状态来自同一时刻。
    fn plugins(&self) -> PluginResult<Vec<PluginStatus>>;
    /// 未注册插件必须返回错误；包含停止后尚未确认退出的任务。
    fn plugin_tasks(&self, id: &PluginId) -> PluginResult<Vec<TaskInfo>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopStage {
    Tasks,
    Cleanup,
    Inspection,
    Lifecycle,
}

impl fmt::Display for StopStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tasks => "任务停止",
            Self::Cleanup => "资源清理",
            Self::Inspection => "退出确认",
            Self::Lifecycle => "生命周期",
        })
    }
}

/// 同一插件不同阶段可有多个错误；保留底层错误供宿主判断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginStopError {
    pub plugin: PluginId,
    pub stage: StopStage,
    pub error: Box<PluginError>,
}
