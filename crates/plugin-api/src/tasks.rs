//! 任务模式、调度与执行契约；定义层不依赖 Tokio。

use crate::{PluginError, PluginFuture, PluginId, PluginResult, TaskId};
use std::fmt;
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

/// 开放的任务类型标识，建议使用插件命名空间，例如 `demo.write-state`。
/// 仅用于诊断与分类；执行行为由 Task 对象提供，不按字符串查找代码。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TaskTypeId(String);

impl TaskTypeId {
    pub fn new(value: impl Into<String>) -> PluginResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PluginError::Task("任务类型标识不能为空".into()));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 插件自定义的任务行为。字段可保存类型化参数和 Context，无需修改内核。
/// 同一任务串行调用 execute；多个任务共享此对象时，实现须自行保护共享状态。
pub trait Task: Send + Sync + 'static {
    /// 在构造 TaskSpec 时快照；此方法应快速返回稳定标识。
    fn task_type(&self) -> TaskTypeId;
    fn execute(&self, signal: Arc<dyn TaskSignal>) -> PluginFuture<'_, ()>;
}

/// 每个已登记任务独占的调度状态；不依赖特定执行器。
pub trait TaskScheduler: Send + 'static {
    /// 每次动作前调用；成功次数从零开始。可以异步等待事件或外部条件。
    /// Some 表示从本次返回起再等多久，None 表示自然结束。
    /// 停止时等待 Future 会被丢弃，因此实现必须允许取消；不得同步阻塞线程。
    fn next(&mut self, completed: u32) -> PluginFuture<'_, Option<Duration>>;
}

/// 执行器内部调用，为每个任务创建独立调度状态；工厂 panic 记入任务报告。
pub type TaskScheduleFactory = Arc<dyn Fn() -> Box<dyn TaskScheduler> + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskMode {
    /// 调用方等待完成；不是 UI 线程或更高的调度优先级。
    Foreground,
    Background,
}

#[derive(Clone)]
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
    /// 调度工厂由插件提供；max_runs 是由后端强制执行的次数上限。
    /// 前台必须有正数上限；后台可为 None。上限不保证 next 会及时返回。
    Custom {
        name: String,
        max_runs: Option<u32>,
        factory: TaskScheduleFactory,
    },
}

/// 可查询的调度描述，不持有插件对象或工厂闭包。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskScheduleInfo {
    Immediate,
    After(Duration),
    At(SystemTime),
    Every {
        interval: Duration,
        runs: Option<u32>,
    },
    Custom {
        name: String,
        max_runs: Option<u32>,
    },
}

impl TaskSchedule {
    pub fn info(&self) -> TaskScheduleInfo {
        match self {
            Self::Immediate => TaskScheduleInfo::Immediate,
            Self::After(delay) => TaskScheduleInfo::After(*delay),
            Self::At(time) => TaskScheduleInfo::At(*time),
            Self::Every { interval, runs } => TaskScheduleInfo::Every {
                interval: *interval,
                runs: *runs,
            },
            Self::Custom { name, max_runs, .. } => TaskScheduleInfo::Custom {
                name: name.clone(),
                max_runs: *max_runs,
            },
        }
    }
}

impl fmt::Debug for TaskSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.info().fmt(f)
    }
}

#[derive(Clone)]
pub struct TaskSpec {
    pub name: String,
    pub task_type: TaskTypeId,
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
            task_type: TaskTypeId("eve.action".into()),
            mode,
            schedule,
            action,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// 将自定义任务对象适配到统一执行路径，前台、后台与各种调度均可组合。
    pub fn from_task(
        name: impl Into<String>,
        mode: TaskMode,
        schedule: TaskSchedule,
        task: Arc<dyn Task>,
    ) -> PluginResult<Self> {
        let task_type = task.task_type();
        let action: TaskAction = Arc::new(move |signal| {
            let task = task.clone();
            Box::pin(async move { task.execute(signal).await })
        });
        let mut spec = Self::new(name, mode, schedule, action)?;
        spec.task_type = task_type;
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
        if let TaskSchedule::Custom { name, max_runs, .. } = &self.schedule {
            if name.trim().is_empty() {
                return Err(PluginError::Task("自定义调度名称不能为空".into()));
            }
            if *max_runs == Some(0) {
                return Err(PluginError::Task("执行次数上限必须大于零".into()));
            }
            if self.mode == TaskMode::Foreground && max_runs.is_none() {
                return Err(PluginError::Task(
                    "前台自定义调度必须设置执行次数上限".into(),
                ));
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
    pub task_type: TaskTypeId,
    pub mode: TaskMode,
    pub schedule: TaskScheduleInfo,
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
    pub task_type: TaskTypeId,
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
