//! 可替换的基础能力契约，不依赖 Tokio 或任何具体后端。

use crate::{
    Cleanup, Event, EventHandler, EventId, Permission, PluginId, PluginManifest, PluginResult,
    ServiceId,
};
use std::any::Any;
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
