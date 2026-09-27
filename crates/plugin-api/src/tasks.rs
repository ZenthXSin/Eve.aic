//! 任务模式、调度与执行契约；定义层不依赖 Tokio。

use crate::{PluginError, PluginFuture, PluginId, PluginResult, TaskId};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// 单调、可重复等待的停止信号；取消前后调用 cancelled 都不得丢失通知。
pub trait TaskSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

pub type TaskFuture = PluginFuture<'static, ()>;
/// 重复任务每次构造一个新的 Future；同一任务内的动作串行执行。
pub type TaskAction = Arc<dyn Fn(Arc<dyn TaskSignal>) -> TaskFuture + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskMode {
    /// 调用方等待完成；不是 UI 线程或更高的调度优先级。
    Foreground,
    Background,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskSchedule {
    Immediate,
    After(Duration),
    /// 注册时换算成延迟；已过期的时间立即执行一次，不追赶补发。
    At(SystemTime),
    /// 首次立即执行，此后每次完成再等待 interval，不重叠执行。
    Every {
        interval: Duration,
        runs: Option<u32>,
    },
}

#[derive(Clone)]
pub struct TaskSpec {
    pub name: String,
    pub mode: TaskMode,
    pub schedule: TaskSchedule,
    pub action: TaskAction,
}

impl TaskSpec {
    pub fn new(
        name: impl Into<String>,
        mode: TaskMode,
        schedule: TaskSchedule,
        action: TaskAction,
    ) -> PluginResult<Self> {
        let spec = Self {
            name: name.into(),
            mode,
            schedule,
            action,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// 后端也必须校验，防止直接构造或修改公开字段绕过检查。
    pub fn validate(&self) -> PluginResult<()> {
        if self.name.trim().is_empty() {
            return Err(PluginError::Task("任务名称不能为空".into()));
        }
        if let TaskSchedule::Every { interval, runs } = self.schedule {
            if interval.is_zero() {
                return Err(PluginError::Task("重复间隔必须大于零".into()));
            }
            if runs == Some(0) {
                return Err(PluginError::Task("执行次数必须大于零".into()));
            }
            if self.mode == TaskMode::Foreground && runs.is_none() {
                return Err(PluginError::Task("无限重复任务不能以前台模式运行".into()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskInfo {
    pub id: TaskId,
    pub owner: PluginId,
    pub name: String,
    pub mode: TaskMode,
    pub schedule: TaskSchedule,
    pub state: TaskState,
    /// 已成功完成的次数，长期运行到上限后饱和计数。
    pub runs: u32,
    /// true 表示执行 Future 已退出，而不只是已经发出取消请求。
    pub exited: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Scheduled,
    Running,
    Finished,
    Failed,
    Stopping,
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRunReport {
    pub id: TaskId,
    pub runs: u32,
    pub state: TaskState,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskShutdownReport {
    /// 已确认退出的任务，包括返回错误或已取消的任务。
    pub stopped: Vec<TaskId>,
    /// 超过协作停止期限，已经请求强制取消的任务。
    pub timed_out: Vec<TaskId>,
    /// 取消确认期限后仍未确认退出；保留可查询记录。
    pub unfinished: Vec<TaskId>,
    pub errors: Vec<String>,
}

impl TaskShutdownReport {
    pub fn is_clean(&self) -> bool {
        self.timed_out.is_empty() && self.unfinished.is_empty() && self.errors.is_empty()
    }
}

/// 前台/后台共用的任务管理后端，交给一个 Runtime 独占使用。
/// 注册方法同步完成登记，不得同步调用插件动作；失败不能残留任务。
/// 宿主须先封闭插件的新任务入口，再调用 shutdown。
pub trait TaskManager: Send + Sync {
    fn spawn(&self, owner: PluginId, spec: TaskSpec) -> PluginResult<TaskId>;
    /// 返回前已登记任务，Future 只负责等待。丢弃等待须请求取消，不能脱管。
    fn run_foreground(
        self: Arc<Self>,
        owner: PluginId,
        spec: TaskSpec,
    ) -> PluginResult<PluginFuture<'static, TaskRunReport>>;
    fn list(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>>;
    /// 所有任务共享各阶段的截止时间；不得在 abort 后无限等待。
    fn shutdown(
        self: Arc<Self>,
        owner: &PluginId,
        timeout: Duration,
        abort_timeout: Duration,
    ) -> PluginFuture<'static, TaskShutdownReport>;
}
