//! 可替换的基础能力契约，不依赖 Tokio 或任何具体后端。

use crate::{
    Cleanup, Event, EventHandler, EventId, Permission, PluginError, PluginId, PluginManifest,
    PluginResult, ServiceId, TaskId,
};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 类型擦除的共享服务；插件通过 Context 按契约类型获取。
pub type ServiceValue = Arc<dyn Any + Send + Sync>;

#[derive(Clone)]
pub struct ServiceEntry {
    pub owner: PluginId,
    pub value: ServiceValue,
}

/// 事件投递后端。同步接口只保证当前实现的同步投递语义。
/// 队列型实现可以在 emit 时完成入队，消费确认需要另行定义。
pub trait EventBus: Send + Sync {
    fn emit(&self, event: Event) -> PluginResult<()>;

    /// 注册过程中不得执行 handler。失败时不得留下监听器。
    /// 返回的清理动作仅注销此次注册，不应强持有后端以形成资源环。
    fn subscribe(self: Arc<Self>, event: EventId, handler: EventHandler) -> PluginResult<Cleanup>;
}

/// 服务发现后端。ID 冲突必须拒绝，获取结果保留注册者身份供内核校验。
pub trait ServiceRegistry: Send + Sync {
    /// 失败不得留下服务；清理动作只撤销此次注册。
    fn provide(
        self: Arc<Self>,
        owner: PluginId,
        id: ServiceId,
        value: ServiceValue,
    ) -> PluginResult<Cleanup>;
    fn get(&self, id: &ServiceId) -> PluginResult<Option<ServiceEntry>>;
}

/// 按插件 ID 隔离的字节键值状态。停止和启动失败不会撤销已写入状态。
/// 后端错误必须返回 Err，不能伪装成键不存在；当前接口是同步的。
pub trait StateStore: Send + Sync {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>>;
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()>;
}

/// 可信插件的权限声明检查约定，不是操作系统访问控制或沙箱。
pub trait PermissionChecker: Send + Sync {
    fn check(&self, manifest: &PluginManifest, permission: &Permission) -> PluginResult<()>;
}

/// 插件任务收到的协作式停止信号。
pub trait TaskSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// 由插件提供给 TaskManager 的一次任务动作。重复调度会多次调用同一个动作。
pub type TaskFuture = Pin<Box<dyn Future<Output = PluginResult<()>> + Send + 'static>>;
pub type TaskAction = Arc<dyn Fn(Arc<dyn TaskSignal>) -> TaskFuture + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskMode {
    /// 由插件显式等待，适合必须完成后才能继续的短任务。
    Foreground,
    /// 脱离当前调用继续运行，归属插件 Scope 并在停止时收尾。
    Background,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskSchedule {
    Immediate,
    After(std::time::Duration),
    Every {
        interval: std::time::Duration,
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
        let name = name.into();
        if name.trim().is_empty() {
            return Err(PluginError::Task("任务名称不能为空".into()));
        }
        if let TaskSchedule::Every { runs: Some(0), .. } = schedule {
            return Err(PluginError::Task("重复任务的执行次数必须大于 0".into()));
        }
        if mode == TaskMode::Foreground
            && matches!(schedule, TaskSchedule::Every { runs: None, .. })
        {
            return Err(PluginError::Task("无限重复任务不能以前台模式运行".into()));
        }
        Ok(Self {
            name,
            mode,
            schedule,
            action,
        })
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Scheduled,
    Running,
    Finished,
    Failed,
    Stopping,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRunReport {
    pub id: TaskId,
    pub runs: u32,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskShutdownReport {
    pub stopped: Vec<TaskId>,
    pub timed_out: Vec<TaskId>,
    pub errors: Vec<String>,
}

impl TaskShutdownReport {
    pub fn is_clean(&self) -> bool {
        self.timed_out.is_empty() && self.errors.is_empty()
    }
}

/// 后台任务管理后端。停止必须先发出协作式取消，再等待或取消任务。
pub trait TaskManager: Send + Sync {
    fn spawn(&self, owner: PluginId, spec: TaskSpec) -> PluginResult<TaskId>;
    fn run_foreground(
        self: Arc<Self>,
        owner: PluginId,
        spec: TaskSpec,
    ) -> crate::PluginFuture<'static, TaskRunReport>;
    fn list(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>>;
    fn shutdown(
        self: Arc<Self>,
        owner: &PluginId,
        timeout: std::time::Duration,
    ) -> crate::PluginFuture<'static, TaskShutdownReport>;
}
