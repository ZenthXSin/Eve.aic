use eve_config_api::*;
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, ServiceId, ServiceRegistry};
use eve_web_panel_api::*;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

fn pages(registry: &dyn ServiceRegistry) -> Arc<PluginPagesHandle> {
    registry
        .get(&ServiceId::new(page_service_id(CONFIG_PLUGIN_ID)).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast()
        .unwrap()
}
fn request(
    page: &PluginPage,
    values: BTreeMap<String, Option<serde_json::Value>>,
) -> PageSaveRequest {
    PageSaveRequest {
        plugin_id: CONFIG_PLUGIN_ID.into(),
        page_id: page.descriptor.id.clone(),
        instance: page.instance.clone(),
        expected_revision: page.revision,
        values,
    }
}

#[tokio::test]
async fn granted_pages_commit_patches_keep_snapshots_and_recover_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let permit = PageWritePermit::default();
    let mut schema = runtime_llm_schema();
    schema
        .fields
        .get_mut(RESPONSE_MODE)
        .unwrap()
        .restart_required = true;
    let plugin = ConfigPlugin::new(
        ConfigBootstrap::new(dir.path(), vec![schema, model_roles_schema()]).with_environment(
            BTreeMap::from([("EVE_LLM_MAX_PARALLEL_TOOL_CALLS".into(), "6".into())]),
        ),
    )
    .unwrap()
    .with_web_pages(permit.clone());
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    let id = PluginId::new(CONFIG_PLUGIN_ID).unwrap();
    kernel.start(&id).await.unwrap();
    let provider = pages(registry.as_ref());
    assert_eq!(provider.0.pages().unwrap().len(), 2);
    let first = provider.0.read(LLM_NAMESPACE).unwrap();
    let values = BTreeMap::from([
        (MAX_PARALLEL_TOOL_CALLS.into(), Some(json!(4))),
        (RESPONSE_MODE.into(), Some(json!("stream"))),
    ]);
    let change = request(&first, values);
    assert_eq!(
        provider.0.save(&change, &PageWritePermit::default()),
        Err(PanelError::Forbidden)
    );
    assert_eq!(admin.current().unwrap().revision, 0);
    let settings = registry
        .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ConfigServiceHandle>()
        .unwrap();
    let captured = settings.0.begin_request(LLM_NAMESPACE, 1).unwrap();
    let saved = provider.0.save(&change, &permit).unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(
        saved.restart_required,
        [format!("{LLM_NAMESPACE}.{RESPONSE_MODE}")]
    );
    assert_eq!(provider.0.save(&change, &permit), Err(PanelError::Stale));
    assert_eq!(
        settings
            .0
            .read_request(&captured)
            .unwrap()
            .get::<usize>(MAX_PARALLEL_TOOL_CALLS)
            .unwrap(),
        6
    );
    let current = provider.0.read(LLM_NAMESPACE).unwrap();
    let response = current
        .fields
        .iter()
        .find(|f| f.id == RESPONSE_MODE)
        .unwrap();
    assert_eq!(response.value, Some(json!("complete")));
    assert_eq!(response.override_value, Some(json!("stream")));
    for invalid in [json!(0), json!("4")] {
        assert_eq!(
            provider.0.save(
                &request(
                    &current,
                    BTreeMap::from([(MAX_PARALLEL_TOOL_CALLS.into(), Some(invalid))])
                ),
                &permit
            ),
            Err(PanelError::InvalidInput)
        );
    }
    assert_eq!(admin.current().unwrap().revision, 1);
    let role = provider.0.read(MODELS_NAMESPACE).unwrap();
    provider
        .0
        .save(
            &request(
                &role,
                BTreeMap::from([("primary_model".into(), Some(json!("next-model")))]),
            ),
            &permit,
        )
        .unwrap();
    let current = provider.0.read(LLM_NAMESPACE).unwrap();
    provider
        .0
        .save(
            &request(
                &current,
                BTreeMap::from([(MAX_PARALLEL_TOOL_CALLS.into(), None)]),
            ),
            &permit,
        )
        .unwrap();
    assert_eq!(
        settings
            .0
            .snapshot(LLM_NAMESPACE, 1)
            .unwrap()
            .get::<usize>(MAX_PARALLEL_TOOL_CALLS)
            .unwrap(),
        6
    );
    assert_eq!(
        admin.current().unwrap().namespaces[MODELS_NAMESPACE].values["primary_model"],
        json!("next-model")
    );
    assert_eq!(admin.backups().unwrap().len(), 3);
    kernel.stop_all().await.unwrap();
    assert!(
        registry
            .get(&ServiceId::new(page_service_id(CONFIG_PLUGIN_ID)).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(provider.0.read(LLM_NAMESPACE), Err(PanelError::Unavailable));
    kernel.start(&id).await.unwrap();
    let fresh = pages(registry.as_ref());
    let page = fresh.0.read(LLM_NAMESPACE).unwrap();
    assert_eq!(page.revision, 3);
    assert_ne!(page.instance, first.instance);
    assert_eq!(
        page.fields
            .iter()
            .find(|f| f.id == RESPONSE_MODE)
            .unwrap()
            .value,
        Some(json!("stream"))
    );
    let mut stale_instance = request(
        &page,
        BTreeMap::from([(MAX_PARALLEL_TOOL_CALLS.into(), Some(json!(3)))]),
    );
    stale_instance.instance = first.instance;
    assert_eq!(
        fresh.0.save(&stale_instance, &permit),
        Err(PanelError::Stale)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn ordinary_config_start_does_not_publish_a_write_page_service() {
    let dir = tempfile::tempdir().unwrap();
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(
            ConfigPlugin::new(
                ConfigBootstrap::new(dir.path(), vec![runtime_llm_schema()])
                    .with_environment(BTreeMap::new()),
            )
            .unwrap(),
        ))
        .unwrap();
    kernel
        .start(&PluginId::new(CONFIG_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    assert!(
        registry
            .get(&ServiceId::new(page_service_id(CONFIG_PLUGIN_ID)).unwrap())
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

#[test]
fn duplicate_json_fields_are_rejected_even_when_the_first_value_is_null() {
    let input = r#"{"plugin_id":"eve.config","page_id":"runtime.llm","instance":"instance","expected_revision":0,"values":{"response_mode":null,"response_mode":"stream"}}"#;
    assert!(serde_json::from_str::<PageSaveRequest>(input).is_err());
}
