use eve_cognition_api::*;
use eve_cognition_plugin::{COGNITION_STATE_KEY, CognitionController, CognitionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginId, ServiceRegistry, StateStore};
use std::sync::Arc;

fn namespace() -> PluginId {
    PluginId::new(COGNITION_PLUGIN_ID).unwrap()
}

async fn open(
    store: Arc<dyn StateStore>,
) -> (Kernel, CognitionController, Arc<dyn ServiceRegistry>) {
    let services = KernelServices {
        state: store,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let plugin = CognitionPlugin::new("eve").unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, controller, registry)
}

fn drive(status: AutonomousDriveStatus) -> AutonomousDrive {
    AutonomousDrive {
        id: "self-drive-1".into(),
        source: Source {
            kind: SourceKind::Internal,
            channel: "cognition.self".into(),
            reference: "reflection-1".into(),
        },
        visibility: Visibility::Internal,
        description: "推进一个可验证的维护步骤".into(),
        evidence: vec![],
        priority: 90,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 1,
            max_attempts: 2,
            timeout_ms: 1_000,
        },
        stop_condition: "single-attempt".into(),
        status,
        attempts: 0,
        cooldown_until_ms: None,
        last_progress_at_ms: None,
        valid_until_ms: Some(10_000),
    }
}

#[tokio::test]
async fn autonomous_drive_review_persists_and_reader_scope_holds_after_restart() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, admin, _) = open(store.clone()).await;
    let initial = admin.snapshot().unwrap();
    let submitted = admin
        .submit_autonomous_drive(initial.revision, drive(AutonomousDriveStatus::Candidate), 1_000)
        .unwrap();
    assert_eq!(
        submitted.state.autonomous_drives["self-drive-1"].status,
        AutonomousDriveStatus::Candidate
    );

    let public = admin.reader(ReadAccess::Public).unwrap().snapshot().unwrap();
    assert!(public.state.autonomous_drives.is_empty());
    let internal = admin
        .reader(ReadAccess::Internal)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(internal.state.autonomous_drives.len(), 1);

    let reviewed = admin
        .review_autonomous_drives(submitted.revision, 1_100)
        .unwrap();
    assert_eq!(
        reviewed.state.autonomous_drives["self-drive-1"].status,
        AutonomousDriveStatus::Ready
    );
    assert_eq!(
        reviewed
            .state
            .events
            .last()
            .unwrap()
            .source
            .channel,
        "cognition.autonomous.review"
    );
    kernel.stop_all().await.unwrap();

    let (kernel, recovered, _) = open(store).await;
    let snapshot = recovered.snapshot().unwrap();
    assert_eq!(
        snapshot.state.autonomous_drives["self-drive-1"].status,
        AutonomousDriveStatus::Ready
    );
    assert!(recovered
        .reader(ReadAccess::Public)
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .autonomous_drives
        .is_empty());
    assert!(recovered
        .reader(ReadAccess::Internal)
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .autonomous_drives
        .contains_key("self-drive-1"));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn autonomous_drive_review_rejects_stale_revision_without_duplicate_audit() {
    let (kernel, admin, _) = open(Arc::new(MemoryStateStore::default())).await;
    let initial = admin.snapshot().unwrap();
    let submitted = admin
        .submit_autonomous_drive(initial.revision, drive(AutonomousDriveStatus::Candidate), 1_000)
        .unwrap();
    let before = admin.snapshot().unwrap();
    assert_eq!(
        admin.review_autonomous_drives(initial.revision, 1_100),
        Err(CognitionError::StaleRevision)
    );
    assert_eq!(admin.snapshot().unwrap(), before);
    let reviewed = admin
        .review_autonomous_drives(submitted.revision, 1_100)
        .unwrap();
    assert_eq!(
        reviewed.state.autonomous_drives["self-drive-1"].status,
        AutonomousDriveStatus::Ready
    );
    let after = admin.snapshot().unwrap();
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.source.channel == "cognition.autonomous.review")
            .count(),
        1
    );
    kernel.stop_all().await.unwrap();
}

#[allow(dead_code)]
fn state_key() -> (&'static str, PluginId) {
    (COGNITION_STATE_KEY, namespace())
}
