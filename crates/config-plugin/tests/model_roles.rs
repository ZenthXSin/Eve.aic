use eve_config_api::*;
use eve_config_plugin::{ConfigBootstrap, ConfigController, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, ServiceId};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc};

async fn start(directory: &Path) -> (Kernel, ConfigController, Arc<ConfigServiceHandle>) {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = ConfigPlugin::new(
        ConfigBootstrap::new(directory, vec![model_roles_schema()])
            .with_environment(BTreeMap::from([
                ("EVE_MODELS_PRIMARY_ENABLED".into(), "true".into()),
                ("EVE_MODELS_PRIMARY_PROVIDER".into(), "mock".into()),
                ("EVE_MODELS_PRIMARY_MODEL".into(), "environment-model".into()),
            ])),
    )
    .unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&PluginId::new(CONFIG_PLUGIN_ID).unwrap()).await.unwrap();
    let handle = registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ConfigServiceHandle>()
        .unwrap();
    (kernel, admin, handle)
}

fn overrides(model: &str) -> ConfigOverrides {
    BTreeMap::from([(
        MODELS_NAMESPACE.into(),
        NamespaceValues {
            schema_version: MODELS_SCHEMA_VERSION,
            values: BTreeMap::from([
                ("primary_model".into(), json!(model)),
                ("auxiliary_small_enabled".into(), json!(true)),
                ("auxiliary_small_provider".into(), json!("mock")),
                ("auxiliary_small_model".into(), json!("small")),
                ("jev_enabled".into(), json!(true)),
                ("jev_provider".into(), json!("decision")),
                ("jev_model".into(), json!("judge")),
                ("semantic_enabled".into(), json!(true)),
                ("semantic_provider".into(), json!("vector")),
                ("semantic_model".into(), json!("embed")),
                ("semantic_dimensions".into(), json!(768)),
            ]),
        },
    )])
}

#[tokio::test]
async fn captured_roles_stay_fixed_across_updates_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, admin, service) = start(directory.path()).await;
    let first = ModelRolesConfig::capture(service.0.as_ref()).unwrap();
    assert_eq!(first.require(ModelRole::Primary).unwrap().model, "environment-model");
    assert!(first.profile(ModelRole::Jev).is_none());
    let request = service.0.begin_request(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION).unwrap();
    admin.replace(0, overrides("new-model"), ApplyMode::NewRequests).unwrap();
    assert_eq!(
        ModelRolesConfig::try_from(&service.0.read_request(&request).unwrap()).unwrap(),
        first
    );
    let second = ModelRolesConfig::capture(service.0.as_ref()).unwrap();
    assert_eq!(second.revision(), 1);
    for role in ModelRole::ALL {
        assert!(second.profile(role).is_some());
    }
    admin.replace(1, overrides("immediate-model"), ApplyMode::Immediate).unwrap();
    assert_eq!(
        ModelRolesConfig::try_from(&service.0.read_request(&request).unwrap())
            .unwrap()
            .require(ModelRole::Primary)
            .unwrap()
            .model,
        "immediate-model"
    );
    assert_eq!(first.require(ModelRole::Primary).unwrap().model, "environment-model");
    assert_eq!(second.require(ModelRole::Primary).unwrap().model, "new-model");
    kernel.stop_all().await.unwrap();
    assert_eq!(
        ModelRolesConfig::capture(service.0.as_ref()),
        Err(ConfigError::Unavailable)
    );
    let (kernel, admin, restored) = start(directory.path()).await;
    let current = ModelRolesConfig::capture(restored.0.as_ref()).unwrap();
    assert_eq!(current.revision(), 2);
    assert_eq!(current.require(ModelRole::Primary).unwrap().model, "immediate-model");
    assert_eq!(restored.0.read_request(&request), Err(ConfigError::StaleRequest));
    admin.rollback(2, 1, ApplyMode::NewRequests).unwrap();
    assert_eq!(
        ModelRolesConfig::capture(restored.0.as_ref())
            .unwrap()
            .require(ModelRole::Primary)
            .unwrap()
            .model,
        "new-model"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn malformed_updates_do_not_change_revision_or_backups() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, admin, service) = start(directory.path()).await;
    let before = ModelRolesConfig::capture(service.0.as_ref()).unwrap();
    for (field, value) in [
        ("primary_timeout_ms", json!(0)),
        ("semantic_dimensions", json!(-1)),
        ("primary_api_key", json!("secret")),
    ] {
        let mut values = overrides("ignored");
        values.get_mut(MODELS_NAMESPACE).unwrap().values.insert(field.into(), value);
        assert!(admin.replace(0, values, ApplyMode::NewRequests).is_err());
        assert_eq!(admin.current().unwrap().revision, 0);
        assert!(admin.backups().unwrap().is_empty());
        assert_eq!(ModelRolesConfig::capture(service.0.as_ref()).unwrap(), before);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn semantic_validation_is_a_consumer_boundary_and_never_clears_storage() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, admin, service) = start(directory.path()).await;
    let mut values = overrides("main");
    values.get_mut(MODELS_NAMESPACE).unwrap().values.insert("semantic_dimensions".into(), json!(0));
    // 通用存储只校验标量 Schema；跨字段语义由装配消费者拒绝。
    admin.replace(0, values, ApplyMode::NewRequests).unwrap();
    let bytes = std::fs::read(directory.path().join("config.json")).unwrap();
    assert!(ModelRolesConfig::capture(service.0.as_ref()).is_err());
    kernel.stop_all().await.unwrap();
    let (kernel, admin, restored) = start(directory.path()).await;
    assert!(ModelRolesConfig::capture(restored.0.as_ref()).is_err());
    assert_eq!(admin.current().unwrap().revision, 1);
    assert_eq!(std::fs::read(directory.path().join("config.json")).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
}
