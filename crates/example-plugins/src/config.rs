//! 配置消费者只依赖定义层，不能调用配置插件的管理接口。
use eve_config_api::{
    CONFIG_PLUGIN_ID, CONFIG_SERVICE_ID, ConfigServiceHandle, LLM_NAMESPACE, LlmRuntimeConfig,
};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginId,
    PluginManifest, PluginResult, ServiceId,
};

pub const CONFIG_CONSUMER: &str = "demo.config-consumer";
pub const CONFIG_REPORT: &str = "demo.config-report";

pub struct ConfigConsumerPlugin {
    manifest: PluginManifest,
}

impl ConfigConsumerPlugin {
    pub fn new() -> PluginResult<Self> {
        let mut manifest = PluginManifest::new(CONFIG_CONSUMER, "0.1.0")?;
        manifest.dependencies.push(PluginDependency {
            id: PluginId::new(CONFIG_PLUGIN_ID)?,
            requirement: Some("^0.1".into()),
        });
        Ok(Self { manifest })
    }
}

impl Plugin for ConfigConsumerPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let id = ServiceId::new(CONFIG_SERVICE_ID)?;
            let config = ctx
                .service::<ConfigServiceHandle>(&id)?
                .ok_or(PluginError::ServiceNotFound(id))?;
            let snapshot = config
                .0
                .snapshot(LLM_NAMESPACE, 1)
                .map_err(|e| PluginError::State(e.to_string()))?;
            let settings = LlmRuntimeConfig::try_from(&snapshot)
                .map_err(|e| PluginError::State(e.to_string()))?;
            ctx.provide_service(ServiceId::new(CONFIG_REPORT)?, settings)?;
            Ok(None)
        })
    }
}
