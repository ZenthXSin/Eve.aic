mod support;
use eve_interest_api::*;
use eve_interest_plugin::{InterestSettingsController, InterestSettingsPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_web_panel_api::*;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use support::RecordingStore;

async fn open(
    store: Arc<dyn StateStore>,
    defaults: InterestLearningSettings,
    permit: Option<PageWritePermit>,
) -> (
    Kernel,
    InterestSettingsController,
    Option<Arc<dyn PluginPages>>,
) {
    let services = KernelServices {
        state: store,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let plugin = InterestSettingsPlugin::new(defaults).unwrap();
    let controller = plugin.controller();
    let plugin = match permit {
        Some(permit) => plugin.with_web_pages(permit),
        None => plugin,
    };
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let pages = registry
        .get(&ServiceId::new(page_service_id(INTEREST_SETTINGS_PLUGIN_ID)).unwrap())
        .unwrap()
        .map(|entry| {
            entry
                .value
                .downcast::<PluginPagesHandle>()
                .unwrap()
                .0
                .clone()
        });
    (kernel, controller, pages)
}
fn request(pages: &dyn PluginPages, values: Value) -> PageSaveRequest {
    let page = pages.read("learning").unwrap();
    PageSaveRequest {
        plugin_id: INTEREST_SETTINGS_PLUGIN_ID.into(),
        page_id: "learning".into(),
        instance: page.instance,
        expected_revision: page.revision,
        values: serde_json::from_value(values).unwrap(),
    }
}
fn owner() -> PluginId {
    PluginId::new(INTEREST_SETTINGS_PLUGIN_ID).unwrap()
}

#[tokio::test]
async fn hot_save_notifies_after_durable_commit_and_reopen_preserves_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let permit = PageWritePermit::default();
    let (kernel, controller, pages) = open(
        store.clone(),
        InterestLearningSettings::default(),
        Some(permit.clone()),
    )
    .await;
    let pages = pages.unwrap();
    assert!(!controller.snapshot().unwrap().settings.enabled);
    assert_eq!(store.writes(), 0, "只读页面不得创建文件");
    let save = request(pages.as_ref(), json!({"enabled":true,"cooldown_ms":0}));
    let waiter = controller.changed(0);
    let result = pages.save(&save, &permit).unwrap();
    assert!(result.restart_required.is_empty());
    let snapshot = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.settings.enabled);
    assert_eq!(snapshot.settings.observation.cooldown_ms, 0);
    assert_eq!(snapshot.revision, 1);
    let disk: Value = serde_json::from_slice(
        &store
            .get(&owner(), INTEREST_SETTINGS_STATE_KEY)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(disk["revision"], snapshot.revision);
    assert_eq!(pages.save(&save, &permit), Err(PanelError::Stale));
    let old_page = pages.read("learning").unwrap();
    kernel.stop_all().await.unwrap();
    assert_eq!(controller.snapshot(), Err(InterestError::Unavailable));
    assert_eq!(pages.read("learning"), Err(PanelError::Unavailable));
    assert_eq!(controller.changed(1).await, Err(InterestError::Unavailable));
    let (kernel, reopened, next_pages) = open(
        store.clone(),
        InterestLearningSettings::default(),
        Some(permit.clone()),
    )
    .await;
    assert_eq!(reopened.snapshot().unwrap(), snapshot);
    let next_pages = next_pages.unwrap();
    assert_ne!(
        old_page.instance,
        next_pages.read("learning").unwrap().instance
    );
    let mut old = request(next_pages.as_ref(), json!({"enabled":false}));
    old.instance = old_page.instance;
    assert_eq!(next_pages.save(&old, &permit), Err(PanelError::Stale));
    let restore = request(
        next_pages.as_ref(),
        json!({"enabled":null,"cooldown_ms":null}),
    );
    next_pages.save(&restore, &permit).unwrap();
    assert_eq!(
        reopened.snapshot().unwrap().settings,
        InterestLearningSettings::default()
    );
    assert_eq!(reopened.snapshot().unwrap().paused_at_revision, 2);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn pause_epoch_survives_coalesced_fast_resume_and_close_wakes_subscribers() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let permit = PageWritePermit::default();
    let defaults = InterestLearningSettings {
        enabled: true,
        ..InterestLearningSettings::default()
    };
    let (kernel, controller, pages) = open(store, defaults, Some(permit.clone())).await;
    let pages = pages.unwrap();
    for enabled in [false, true] {
        pages
            .save(
                &request(pages.as_ref(), json!({"enabled":enabled})),
                &permit,
            )
            .unwrap();
    }
    let coalesced = controller.changed(0).await.unwrap();
    assert!(coalesced.settings.enabled);
    assert_eq!(coalesced.paused_at_revision, 1);
    let waiting = {
        let reader = controller.clone();
        tokio::spawn(async move { reader.changed(2).await })
    };
    kernel.stop_all().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap(),
        Err(InterestError::Unavailable)
    );
}

#[tokio::test]
async fn invalid_unauthorized_and_wrong_page_writes_preserve_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let permit = PageWritePermit::default();
    let (kernel, controller, pages) = open(
        store.clone(),
        InterestLearningSettings::default(),
        Some(permit.clone()),
    )
    .await;
    let pages = pages.unwrap();
    pages
        .save(
            &request(pages.as_ref(), json!({"cooldown_ms":500})),
            &permit,
        )
        .unwrap();
    let bytes = store
        .get(&owner(), INTEREST_SETTINGS_STATE_KEY)
        .unwrap()
        .unwrap();
    for values in [
        json!({"enabled":"true"}),
        json!({"cooldown_ms":-1}),
        json!({"cooldown_ms":86400001}),
        json!({"cooldown_ms":1.5}),
        json!({"unknown":true}),
    ] {
        assert_eq!(
            pages.save(&request(pages.as_ref(), values), &permit),
            Err(PanelError::InvalidInput)
        );
    }
    assert_eq!(
        pages.save(
            &request(pages.as_ref(), json!({"enabled":true})),
            &PageWritePermit::default()
        ),
        Err(PanelError::Forbidden)
    );
    let mut wrong = request(pages.as_ref(), json!({"enabled":true}));
    wrong.plugin_id = "eve.config".into();
    assert_eq!(pages.save(&wrong, &permit), Err(PanelError::NotFound));
    assert_eq!(controller.snapshot().unwrap().revision, 1);
    assert_eq!(
        store
            .get(&owner(), INTEREST_SETTINGS_STATE_KEY)
            .unwrap()
            .unwrap(),
        bytes
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn ambiguous_write_closes_runtime_then_reopen_reads_actual_commit() {
    for after in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        let permit = PageWritePermit::default();
        let (kernel, controller, pages) = open(
            store.clone(),
            InterestLearningSettings::default(),
            Some(permit.clone()),
        )
        .await;
        let pages = pages.unwrap();
        if after {
            store.fail_after(true);
        } else {
            store.fail_before(true);
        }
        assert_eq!(
            pages.save(&request(pages.as_ref(), json!({"enabled":true})), &permit),
            Err(PanelError::Unavailable)
        );
        assert_eq!(controller.snapshot(), Err(InterestError::Unavailable));
        assert_eq!(controller.changed(0).await, Err(InterestError::Unavailable));
        store.fail_after(false);
        store.fail_before(false);
        kernel.stop_all().await.unwrap();
        let (kernel, controller, _) = open(store, InterestLearningSettings::default(), None).await;
        assert_eq!(controller.snapshot().unwrap().settings.enabled, after);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_or_future_document_refuses_start_and_preserves_original_bytes() {
    for bytes in [b"broken".as_slice(), br#"{"format_version":2}"#, br#"{"format_version":1,"format_version":1}"#,
        br#"{"format_version":1,"revision":0,"paused_at_revision":1,"overrides":{"enabled":true,"cooldown_ms":0}}"#,
        br#"{"format_version":1,"revision":0,"paused_at_revision":0,"overrides":{"enabled":true,"cooldown_ms":86400001}}"#] {
        let directory = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(directory.path());
        store.set(&owner(), INTEREST_SETTINGS_STATE_KEY.into(), bytes.to_vec()).unwrap();
        let kernel = Kernel::with_services(KernelServices { state: store.clone(), ..KernelServices::default() });
        kernel.register(Box::new(InterestSettingsPlugin::new(InterestLearningSettings::default()).unwrap())).unwrap();
        assert!(kernel.start_all().await.is_err());
        assert_eq!(store.get(&owner(), INTEREST_SETTINGS_STATE_KEY).unwrap().unwrap(), bytes);
        assert_eq!(store.writes(), 1);
    }
}

#[tokio::test]
async fn headless_reader_keeps_cli_defaults_without_publishing_write_service() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let defaults = InterestLearningSettings {
        enabled: true,
        observation: ObservationOptions {
            cooldown_ms: 12345,
            ..ObservationOptions::default()
        },
    };
    let (kernel, controller, pages) = open(store.clone(), defaults.clone(), None).await;
    assert_eq!(controller.snapshot().unwrap().settings, defaults);
    assert!(pages.is_none());
    assert_eq!(store.writes(), 0);
    kernel.stop_all().await.unwrap();
}
