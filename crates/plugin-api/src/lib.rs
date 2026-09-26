//! Stable contracts exposed to Eve.aic plugins.
//!
//! The kernel owns the runtime implementation. Plugins only receive a
//! [`PluginContext`] and use its handles to interact with the runtime.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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

/// A version string. Full semver validation is intentionally deferred to the
/// dependency protocol; the first kernel only preserves and reports it.
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
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
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

/// A dependency declaration. `requirement` is preserved for the future
/// semver resolver; the first resolver checks the dependency id.
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
    fn get_service(&self, id: &ServiceId) -> Option<Arc<dyn Any + Send + Sync>>;
    fn state_get(&self, owner: &PluginId, key: &str) -> Option<Vec<u8>>;
    fn state_set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()>;
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

    pub fn service<T>(&self, id: &ServiceId) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        self.hooks
            .get_service(id)
            .and_then(|service| service.downcast::<T>().ok())
    }

    pub fn state_get(&self, key: &str) -> Option<Vec<u8>> {
        self.hooks.state_get(&self.info.id, key)
    }

    pub fn state_set(&self, key: impl Into<String>, value: impl Into<Vec<u8>>) -> PluginResult<()> {
        self.hooks
            .state_set(&self.info.id, key.into(), value.into())
    }

    pub fn cleanup(&self, cleanup: Cleanup) -> PluginResult<()> {
        self.hooks.register_cleanup(&self.info.id, cleanup)
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

/// Errors crossing the plugin boundary.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PluginError {
    InvalidManifest(String),
    DuplicatePlugin(PluginId),
    MissingDependency {
        plugin: PluginId,
        dependency: PluginId,
    },
    DependencyCycle(PluginId),
    InvalidLifecycle {
        plugin: PluginId,
        state: String,
    },
    PluginFailed {
        plugin: PluginId,
        message: String,
    },
    ServiceConflict(ServiceId),
    ServiceNotFound(ServiceId),
    State(String),
    Event(String),
    Cleanup(String),
    Lifecycle(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest(message) => write!(f, "invalid plugin manifest: {message}"),
            Self::DuplicatePlugin(id) => write!(f, "duplicate plugin: {id}"),
            Self::MissingDependency { plugin, dependency } => {
                write!(f, "plugin {plugin} is missing dependency {dependency}")
            }
            Self::DependencyCycle(id) => write!(f, "dependency cycle detected at {id}"),
            Self::InvalidLifecycle { plugin, state } => {
                write!(f, "invalid lifecycle transition for {plugin}: {state}")
            }
            Self::PluginFailed { plugin, message } => {
                write!(f, "plugin {plugin} failed: {message}")
            }
            Self::ServiceConflict(id) => write!(f, "service already provided: {id}"),
            Self::ServiceNotFound(id) => write!(f, "service not found: {id}"),
            Self::State(message) => write!(f, "state error: {message}"),
            Self::Event(message) => write!(f, "event error: {message}"),
            Self::Cleanup(message) => write!(f, "cleanup error: {message}"),
            Self::Lifecycle(message) => write!(f, "lifecycle error: {message}"),
        }
    }
}

impl std::error::Error for PluginError {}
