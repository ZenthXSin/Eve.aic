//! Stable contracts exposed to Eve.aic plugins.
//!
//! The kernel owns the runtime implementation. Plugins only receive a
//! [`PluginContext`] and use its handles to interact with the runtime.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

mod capabilities;
mod diagnostics;
mod lifecycle;
mod logging;
mod tasks;
pub use capabilities::{
    EventBus, PermissionChecker, ServiceEntry, ServiceRegistry, ServiceValue, StateStore,
};
pub use diagnostics::{PluginState, PluginStatus, PluginStopError, RuntimeInspector, StopStage};
pub use lifecycle::{
    LifecycleOperation, LifecycleOperationId, LifecycleOperationState, LifecycleRequest,
    RuntimeLifecycle,
};
pub use logging::{LogEntry, LogLevel, LogRecord, Logger};
pub use tasks::{
    Task, TaskAction, TaskFuture, TaskInfo, TaskManager, TaskMode, TaskRunReport, TaskSchedule,
    TaskScheduleFactory, TaskScheduleInfo, TaskScheduler, TaskShutdownReport, TaskSignal, TaskSpec,
    TaskState, TaskTypeId,
};

/// The result type used by the plugin boundary.
pub type PluginResult<T> = Result<T, PluginError>;

/// An executor-independent, Send future for the object-safe plugin boundary.
pub type PluginFuture<'a, T> = Pin<Box<dyn Future<Output = PluginResult<T>> + Send + 'a>>;

/// A one-shot asynchronous cleanup action owned by a plugin scope.
/// The kernel awaits actions sequentially in reverse registration order.
pub type Cleanup = Box<dyn FnOnce() -> PluginFuture<'static, ()> + Send + 'static>;

/// Box an asynchronous cleanup action without exposing its future type.
///
/// ```
/// use eve_plugin_api::cleanup;
/// let action = cleanup(|| async { Ok(()) });
/// # drop(action);
/// ```
pub fn cleanup<F, Fut>(action: F) -> Cleanup
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = PluginResult<()>> + Send + 'static,
{
    Box::new(move || Box::pin(action()))
}

/// A stable identifier for a plugin.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(value: impl Into<String>) -> PluginResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "plugin id cannot be empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A validated semantic version used by plugin manifests and dependencies.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Version(String);

impl Version {
    pub fn new(value: impl Into<String>) -> PluginResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "plugin version cannot be empty".to_string(),
            ));
        }
        semver::Version::parse(&value).map_err(|error| {
            PluginError::InvalidManifest(format!("plugin version must be valid SemVer: {error}"))
        })?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 检查该版本是否满足一个 SemVer 版本范围。
    pub fn matches_requirement(&self, requirement: &str) -> PluginResult<bool> {
        let requirement = semver::VersionReq::parse(requirement).map_err(|error| {
            PluginError::InvalidManifest(format!(
                "dependency version requirement must be valid SemVer: {error}"
            ))
        })?;
        let version = semver::Version::parse(&self.0).map_err(|error| {
            PluginError::InvalidManifest(format!("plugin version must be valid SemVer: {error}"))
        })?;
        Ok(requirement.matches(&version))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A service identifier exposed through the service registry.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceId(String);

impl ServiceId {
    pub fn new(value: impl Into<String>) -> PluginResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "service id cannot be empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An event identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventId(String);

impl EventId {
    pub fn new(value: impl Into<String>) -> PluginResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "event id cannot be empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 由 Runtime 分配的任务标识。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(u64);

impl TaskId {
    #[doc(hidden)]
    pub fn new(value: u64) -> Self {
        Self(value)
    }
    pub fn get(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task-{}", self.0)
    }
}

/// A dependency declaration. `None` accepts any valid dependency version;
/// `Some` uses a standard SemVer version requirement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginDependency {
    pub id: PluginId,
    pub requirement: Option<String>,
}

/// A permission declared by a plugin.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Permission(String);

impl Permission {
    pub fn new(value: impl Into<String>) -> PluginResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "permission cannot be empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Metadata known before a plugin is started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginManifest {
    pub id: PluginId,
    pub version: Version,
    pub dependencies: Vec<PluginDependency>,
    pub permissions: Vec<Permission>,
}

impl PluginManifest {
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> PluginResult<Self> {
        Ok(Self {
            id: PluginId::new(id)?,
            version: Version::new(version)?,
            dependencies: Vec::new(),
            permissions: Vec::new(),
        })
    }

    /// 校验供注册、发现和未来外部插件协议共用的清单约束。
    pub fn validate(&self) -> PluginResult<()> {
        let mut dependencies = std::collections::HashSet::new();
        for dependency in &self.dependencies {
            if dependency.id == self.id {
                return Err(PluginError::InvalidManifest(format!(
                    "plugin {} cannot depend on itself",
                    self.id
                )));
            }
            if !dependencies.insert(dependency.id.clone()) {
                return Err(PluginError::InvalidManifest(format!(
                    "plugin {} declares dependency {} more than once",
                    self.id, dependency.id
                )));
            }
            if let Some(requirement) = &dependency.requirement {
                if requirement.trim().is_empty() {
                    return Err(PluginError::InvalidManifest(format!(
                        "dependency {} of plugin {} has an empty version requirement",
                        dependency.id, self.id
                    )));
                }
                semver::VersionReq::parse(requirement).map_err(|error| {
                    PluginError::InvalidManifest(format!(
                        "dependency {} of plugin {} has invalid version requirement {requirement:?}: {error}",
                        dependency.id, self.id
                    ))
                })?;
            }
        }
        Ok(())
    }
}

/// The immutable identity exposed to a running plugin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginInfo {
    pub id: PluginId,
    pub version: Version,
}

impl From<&PluginManifest> for PluginInfo {
    fn from(manifest: &PluginManifest) -> Self {
        Self {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
        }
    }
}

/// A payload dispatched through the event bus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub payload: Vec<u8>,
}

impl Event {
    pub fn new(id: impl Into<String>, payload: impl Into<Vec<u8>>) -> PluginResult<Self> {
        Ok(Self {
            id: EventId::new(id)?,
            payload: payload.into(),
        })
    }
}

/// An event listener owned by a plugin scope.
pub type EventHandler = Arc<dyn Fn(&Event) -> PluginResult<()> + Send + Sync + 'static>;

/// The runtime implementation behind a [`PluginContext`].
pub trait RuntimeHooks: Send + Sync {
    fn emit(&self, owner: &PluginId, event: Event) -> PluginResult<()>;
    fn subscribe(
        &self,
        owner: &PluginId,
        event: EventId,
        handler: EventHandler,
    ) -> PluginResult<()>;
    fn provide_service(
        &self,
        owner: &PluginId,
        id: ServiceId,
        service: Arc<dyn Any + Send + Sync>,
    ) -> PluginResult<()>;
    fn get_service(&self, id: &ServiceId) -> PluginResult<Option<ServiceValue>>;
    fn state_get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>>;
    fn state_set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()>;
    fn check_permission(&self, permission: &Permission) -> PluginResult<()>;
    fn log(&self, entry: LogEntry) -> PluginResult<()>;
    fn spawn_task(&self, owner: &PluginId, spec: TaskSpec) -> PluginResult<TaskId>;
    fn run_foreground(
        &self,
        owner: &PluginId,
        spec: TaskSpec,
    ) -> PluginFuture<'static, TaskRunReport>;
    fn list_tasks(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>>;
    fn register_cleanup(&self, owner: &PluginId, cleanup: Cleanup) -> PluginResult<()>;
}

/// The only runtime boundary exposed to a plugin.
#[derive(Clone)]
pub struct PluginContext {
    info: PluginInfo,
    hooks: Arc<dyn RuntimeHooks>,
}

impl PluginContext {
    #[doc(hidden)]
    pub fn new(info: PluginInfo, hooks: Arc<dyn RuntimeHooks>) -> Self {
        Self { info, hooks }
    }

    pub fn plugin(&self) -> &PluginInfo {
        &self.info
    }

    pub fn emit(&self, event: Event) -> PluginResult<()> {
        self.hooks.emit(&self.info.id, event)
    }

    pub fn on(&self, event: EventId, handler: EventHandler) -> PluginResult<()> {
        self.hooks.subscribe(&self.info.id, event, handler)
    }

    pub fn provide_service<T>(&self, id: ServiceId, service: T) -> PluginResult<()>
    where
        T: Any + Send + Sync,
    {
        self.hooks
            .provide_service(&self.info.id, id, Arc::new(service))
    }

    pub fn service<T>(&self, id: &ServiceId) -> PluginResult<Option<Arc<T>>>
    where
        T: Any + Send + Sync,
    {
        self.hooks
            .get_service(id)?
            .map(|service| {
                service
                    .downcast::<T>()
                    .map_err(|_| PluginError::ServiceTypeMismatch(id.clone()))
            })
            .transpose()
    }

    pub fn state_get(&self, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.hooks.state_get(&self.info.id, key)
    }

    pub fn state_set(&self, key: impl Into<String>, value: impl Into<Vec<u8>>) -> PluginResult<()> {
        self.hooks
            .state_set(&self.info.id, key.into(), value.into())
    }

    pub fn cleanup(&self, cleanup: Cleanup) -> PluginResult<()> {
        self.hooks.register_cleanup(&self.info.id, cleanup)
    }

    /// 检查当前插件声明的权限；宿主可以注入更严格的检查器。
    pub fn check_permission(&self, permission: &Permission) -> PluginResult<()> {
        self.hooks.check_permission(permission)
    }

    /// 创建由当前插件 Scope 归属的后台任务。
    pub fn spawn_task(&self, spec: TaskSpec) -> PluginResult<TaskId> {
        self.hooks.spawn_task(&self.info.id, spec)
    }

    pub fn run_foreground(&self, spec: TaskSpec) -> PluginFuture<'static, TaskRunReport> {
        self.hooks.run_foreground(&self.info.id, spec)
    }

    pub fn tasks(&self) -> PluginResult<Vec<TaskInfo>> {
        self.hooks.list_tasks(&self.info.id)
    }

    /// 记录结构化日志，插件身份和时间由宿主补充；停止后的 Context 不能投递。
    pub fn log(&self, entry: LogEntry) -> PluginResult<()> {
        self.hooks.log(entry)
    }
}

/// A plugin implementation.
pub trait Plugin: Send {
    fn manifest(&self) -> &PluginManifest;

    /// Start the plugin. Resources created through the context are owned by
    /// the plugin scope and are cleaned up automatically when it stops.
    /// The optional return value is an additional explicit cleanup action.
    /// Return `Box::pin(async move { ... })`; the future may borrow `self`.
    /// A plugin becomes active only after this future completes successfully.
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>>;
}

/// 用于组合层发现和创建静态插件的工厂契约。
///
/// 工厂只负责提供清单和创建实例；创建不会自动启动插件，也不应在构造阶段
/// 注册 Runtime 资源。目录在登记时保存清单快照，并在创建后再次校验实例清单。
pub trait PluginFactory: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    fn create(&self) -> PluginResult<Box<dyn Plugin>>;
}

/// 静态插件目录契约。目录是组合层能力，不参与 Kernel 生命周期或依赖求解。
pub trait PluginCatalog: Send + Sync {
    fn register_factory(&self, factory: Arc<dyn PluginFactory>) -> PluginResult<()>;
    fn manifests(&self) -> PluginResult<Vec<PluginManifest>>;
    fn find(&self, id: &PluginId, version: &Version) -> PluginResult<Option<PluginManifest>>;
    fn create(&self, id: &PluginId, version: &Version) -> PluginResult<Box<dyn Plugin>>;
}

/// 宿主侧插件注册表契约。插件本身不能通过 `PluginContext` 操作注册表。
pub trait PluginRegistry: Send + Sync {
    fn register(&self, plugin: Box<dyn Plugin>) -> PluginResult<()>;

    /// 仅移除未运行且任务已确认退出的插件；插件状态字节不会被删除。
    fn unregister(&self, id: &PluginId) -> PluginResult<()>;
}

/// Errors crossing the plugin boundary.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PluginError {
    InvalidManifest(String),
    DuplicatePlugin(PluginId),
    MissingDependency {
        plugin: PluginId,
        dependency: PluginId,
    },
    DependencyVersionMismatch {
        plugin: PluginId,
        dependency: PluginId,
        requirement: String,
        found: Version,
    },
    DependencyCycle(PluginId),
    InvalidLifecycle {
        plugin: PluginId,
        state: String,
    },
    PluginNotFound(PluginId),
    PluginFailed {
        plugin: PluginId,
        message: String,
    },
    /// 保留启动原始错误及回滚期间的全部停止错误。
    Rollback {
        cause: Box<PluginError>,
        errors: Vec<PluginError>,
    },
    /// 停止尝试中的所有失败，按实际处理顺序保留原插件和阶段。
    Shutdown(Vec<PluginStopError>),
    ServiceConflict(ServiceId),
    ServiceNotFound(ServiceId),
    ServiceTypeMismatch(ServiceId),
    PermissionDenied {
        plugin: PluginId,
        permission: Permission,
    },
    Task(String),
    State(String),
    Event(String),
    Log(String),
    Cleanup(String),
    Lifecycle(String),
    FactoryCreate(String),
    FactoryNotFound {
        id: PluginId,
        version: Version,
    },
    FactoryManifestMismatch {
        expected: Box<PluginManifest>,
        found: Box<PluginManifest>,
    },
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest(message) => write!(f, "invalid plugin manifest: {message}"),
            Self::DuplicatePlugin(id) => write!(f, "duplicate plugin: {id}"),
            Self::MissingDependency { plugin, dependency } => {
                write!(f, "plugin {plugin} is missing dependency {dependency}")
            }
            Self::DependencyVersionMismatch {
                plugin,
                dependency,
                requirement,
                found,
            } => write!(
                f,
                "plugin {plugin} requires dependency {dependency} at {requirement}, found {found}"
            ),
            Self::DependencyCycle(id) => write!(f, "dependency cycle detected at {id}"),
            Self::InvalidLifecycle { plugin, state } => {
                write!(f, "invalid lifecycle transition for {plugin}: {state}")
            }
            Self::PluginNotFound(id) => write!(f, "plugin not found: {id}"),
            Self::PluginFailed { plugin, message } => {
                write!(f, "plugin {plugin} failed: {message}")
            }
            Self::Rollback { cause, errors } => {
                write!(f, "{cause}；回滚错误")?;
                for error in errors {
                    write!(f, "；{error}")?;
                }
                Ok(())
            }
            Self::Shutdown(errors) => {
                write!(f, "插件停止失败")?;
                for failure in errors {
                    write!(
                        f,
                        "；{} [{}]：{}",
                        failure.plugin, failure.stage, failure.error
                    )?;
                }
                Ok(())
            }
            Self::ServiceConflict(id) => write!(f, "service already provided: {id}"),
            Self::ServiceNotFound(id) => write!(f, "service not found: {id}"),
            Self::ServiceTypeMismatch(id) => write!(f, "服务类型不匹配：{id}"),
            Self::PermissionDenied { plugin, permission } => {
                write!(f, "插件 {plugin} 未获权限：{}", permission.as_str())
            }
            Self::Task(message) => write!(f, "task error: {message}"),
            Self::State(message) => write!(f, "state error: {message}"),
            Self::Event(message) => write!(f, "event error: {message}"),
            Self::Log(message) => write!(f, "日志错误：{message}"),
            Self::Cleanup(message) => write!(f, "cleanup error: {message}"),
            Self::Lifecycle(message) => write!(f, "lifecycle error: {message}"),
            Self::FactoryCreate(message) => write!(f, "插件工厂创建失败：{message}"),
            Self::FactoryNotFound { id, version } => {
                write!(f, "插件目录中不存在精确版本：{id}@{version}")
            }
            Self::FactoryManifestMismatch { expected, found } => write!(
                f,
                "插件工厂清单不一致：登记为 {}@{}，实例为 {}@{}",
                expected.id, expected.version, found.id, found.version
            ),
        }
    }
}

impl std::error::Error for PluginError {}
