use eve_config_api::*;
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_example_plugins::{CONFIG_CONSUMER, CONFIG_REPORT, ConfigConsumerPlugin};
use eve_kernel::{Kernel, KernelServices, PluginState};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginId,
    PluginManifest, ServiceId,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[tokio::test]
async fn consumer_auto_starts_config_and_restart_rebuilds_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = ConfigPlugin::new(
        ConfigBootstrap::new(dir.path(), vec![runtime_llm_schema()])
            .with_environment(BTreeMap::new()),
    )
    .unwrap();
    let admin = plugin.controller();
    assert_eq!(admin.current(), Err(ConfigError::Unavailable));
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .register(Box::new(ConfigConsumerPlugin::new().unwrap()))
        .unwrap();
    kernel
        .start(&PluginId::new(CONFIG_CONSUMER).unwrap())
        .await
        .unwrap();
    assert_eq!(
        kernel.state(&PluginId::new(CONFIG_PLUGIN_ID).unwrap()),
        Some(PluginState::Active)
    );
    let report = registry
        .get(&ServiceId::new(CONFIG_REPORT).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<LlmRuntimeConfig>()
        .unwrap();
    assert_eq!(report.max_parallel_tool_calls, 10);
    let handle = registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ConfigServiceHandle>()
        .unwrap();
    let request = handle.0.begin_request(LLM_NAMESPACE, 1).unwrap();
    let overrides = BTreeMap::from([(
        LLM_NAMESPACE.into(),
        NamespaceValues {
            schema_version: 1,
            values: BTreeMap::from([(MAX_PARALLEL_TOOL_CALLS.into(), serde_json::json!(7))]),
        },
    )]);
    admin.replace(0, overrides, ApplyMode::Immediate).unwrap();
    kernel.stop_all().await.unwrap();
    assert!(
        registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        handle.0.snapshot(LLM_NAMESPACE, 1),
        Err(ConfigError::Unavailable)
    );
    assert_eq!(admin.current(), Err(ConfigError::Unavailable));
    // retained handle 不能锁住旧目录；同一 Kernel 中重启服务须成功。
    kernel
        .start(&PluginId::new(CONFIG_CONSUMER).unwrap())
        .await
        .unwrap();
    let report = registry
        .get(&ServiceId::new(CONFIG_REPORT).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<LlmRuntimeConfig>()
        .unwrap();
    assert_eq!(report.max_parallel_tool_calls, 7);
    let fresh = registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ConfigServiceHandle>()
        .unwrap();
    assert_eq!(
        fresh.0.read_request(&request),
        Err(ConfigError::StaleRequest)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn missing_config_dependency_is_a_startup_error() {
    let kernel = Kernel::new();
    kernel
        .register(Box::new(ConfigConsumerPlugin::new().unwrap()))
        .unwrap();
    assert!(matches!(
        kernel.start(&PluginId::new(CONFIG_CONSUMER).unwrap()).await,
        Err(PluginError::MissingDependency { .. })
    ));
    assert_eq!(
        kernel.state(&PluginId::new(CONFIG_CONSUMER).unwrap()),
        Some(PluginState::Registered)
    );
}

struct FailingConsumer {
    manifest: PluginManifest,
    captured: Arc<Mutex<Option<ConfigServiceHandle>>>,
}

impl Plugin for FailingConsumer {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            *self.captured.lock().unwrap() = Some(
                ctx.service::<ConfigServiceHandle>(&ServiceId::new(CONFIG_SERVICE_ID)?)
                    .unwrap()
                    .unwrap()
                    .as_ref()
                    .clone(),
            );
            Err(PluginError::State("故意失败以验证配置依赖回滚".into()))
        })
    }
}

#[tokio::test]
async fn dependency_rollback_closes_retained_service_and_releases_directory() {
    let dir = tempfile::tempdir().unwrap();
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = ConfigPlugin::new(
        ConfigBootstrap::new(dir.path(), vec![runtime_llm_schema()])
            .with_environment(BTreeMap::new()),
    )
    .unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    let mut manifest = PluginManifest::new("demo.config-failure", "0.1.0").unwrap();
    manifest.dependencies.push(PluginDependency {
        id: PluginId::new(CONFIG_PLUGIN_ID).unwrap(),
        requirement: Some("^0.1".into()),
    });
    let captured = Arc::new(Mutex::new(None));
    kernel
        .register(Box::new(FailingConsumer {
            manifest,
            captured: captured.clone(),
        }))
        .unwrap();
    assert!(
        kernel
            .start(&PluginId::new("demo.config-failure").unwrap())
            .await
            .is_err()
    );
    assert_eq!(admin.current(), Err(ConfigError::Unavailable));
    assert_eq!(
        captured
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .0
            .snapshot(LLM_NAMESPACE, 1),
        Err(ConfigError::Unavailable)
    );
    assert!(
        registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
            .unwrap()
            .is_none()
    );
    kernel
        .start(&PluginId::new(CONFIG_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    assert_eq!(admin.current().unwrap().revision, 0);
    kernel.stop_all().await.unwrap();
}
