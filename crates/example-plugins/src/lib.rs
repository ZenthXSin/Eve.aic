//! 仅依赖定义层的静态插件示例；不访问 Kernel 或具体后端。

use eve_plugin_api::{
    Cleanup, Event, EventId, LogEntry, LogLevel, Permission, Plugin, PluginContext,
    PluginDependency, PluginError, PluginFuture, PluginId, PluginManifest, PluginResult, ServiceId,
};
use std::sync::Arc;

mod tasks;
pub use tasks::{TASK_DEMO, TaskDemoPlugin};

pub const PROVIDER: &str = "demo.provider";
pub const CONSUMER: &str = "demo.consumer";
pub const FAILING: &str = "demo.failing";
pub const FORMATTER: &str = "demo.formatter";
pub const READY: &str = "demo.consumer.ready";
pub const MESSAGE: &str = "demo.message";
pub const TRANSIENT: &str = "demo.transient";
pub const TRANSIENT_EVENT: &str = "demo.transient.event";

/// 消费者依赖的服务契约；不暴露提供者实现类型。
pub trait MessageFormatter: Send + Sync {
    fn format(&self, payload: &[u8]) -> Vec<u8>;
}

pub type FormatterService = Arc<dyn MessageFormatter>;

struct TextFormatter;

impl MessageFormatter for TextFormatter {
    fn format(&self, payload: &[u8]) -> Vec<u8> {
        format!("已接收：{}", String::from_utf8_lossy(payload)).into_bytes()
    }
}

pub struct ServiceProviderPlugin {
    manifest: PluginManifest,
}

impl ServiceProviderPlugin {
    pub fn new() -> PluginResult<Self> {
        let mut manifest = PluginManifest::new(PROVIDER, "0.1.0")?;
        manifest.permissions.push(Permission::new("demo.publish")?);
        Ok(Self { manifest })
    }
}

impl Plugin for ServiceProviderPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.check_permission(&Permission::new("demo.publish")?)?;
            ctx.provide_service(
                ServiceId::new(FORMATTER)?,
                Arc::new(TextFormatter) as FormatterService,
            )?;
            let publisher = ctx.clone();
            // 等消费者订阅完毕再发布，避免同步事件在消费者启动前丢失。
            ctx.on(
                EventId::new(READY)?,
                Arc::new(move |_| publisher.emit(Event::new(MESSAGE, "你好，Eve.aic".as_bytes())?)),
            )?;
            Ok(None)
        })
    }
}

pub struct EventConsumerPlugin {
    manifest: PluginManifest,
}

impl EventConsumerPlugin {
    pub fn new() -> PluginResult<Self> {
        let mut manifest = dependent_manifest(CONSUMER)?;
        manifest.permissions.push(Permission::new("demo.receive")?);
        Ok(Self { manifest })
    }
}

impl Plugin for EventConsumerPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.check_permission(&Permission::new("demo.receive")?)?;
            let id = ServiceId::new(FORMATTER)?;
            let formatter = ctx
                .service::<FormatterService>(&id)?
                .ok_or(PluginError::ServiceNotFound(id))?;
            let state = ctx.clone();
            ctx.on(
                EventId::new(MESSAGE)?,
                Arc::new(move |event| {
                    state.state_set("last_message", formatter.format(&event.payload))?;
                    state.log(
                        LogEntry::new(LogLevel::Info, "demo.message", "已处理消息")?
                            .with_field("event", event.id.as_str())
                            .with_field("bytes", event.payload.len().to_string()),
                    )
                }),
            )?;
            ctx.emit(Event::new(READY, Vec::new())?)?;
            Ok(None)
        })
    }
}

/// 在取得 Service 和监听器之后故意失败，用于验证资源回滚。
pub struct FailingPlugin {
    manifest: PluginManifest,
}

impl FailingPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: dependent_manifest(FAILING)?,
        })
    }
}

impl Plugin for FailingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.provide_service(ServiceId::new(TRANSIENT)?, 1_u32)?;
            let state = ctx.clone();
            ctx.on(
                EventId::new(TRANSIENT_EVENT)?,
                Arc::new(move |_| state.state_set("unexpected", b"leaked".to_vec())),
            )?;
            ctx.log(LogEntry::new(
                LogLevel::Warn,
                "demo.rollback",
                "准备触发预期启动失败，验证资源回滚",
            )?)?;
            Err(PluginError::PluginFailed {
                plugin: ctx.plugin().id.clone(),
                message: "验收用预期启动失败".into(),
            })
        })
    }
}

fn dependent_manifest(id: &str) -> PluginResult<PluginManifest> {
    let mut manifest = PluginManifest::new(id, "0.1.0")?;
    manifest.dependencies.push(PluginDependency {
        id: PluginId::new(PROVIDER)?,
        requirement: None,
    });
    Ok(manifest)
}
