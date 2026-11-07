use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{ControlRunner, RunFuture};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::TurnEventSink;
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{
    PluginDependency, PluginError, PluginFuture, PluginId, PluginResult, ServiceId, StateStore,
};
use eve_qqbot_plugin::{
    QQBOT_PLUGIN_ID, QQBOT_STATUS_SERVICE_ID, QqArchivedReceipt, QqBeforeIngress, QqBotConfig,
    QqBotPlugin, QqBotStatusHandle, QqReceiptArchive,
};
use eve_session_api::{SESSION_PLUGIN_ID, SessionInput};
use eve_session_plugin::SessionPlugin;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

struct NeverRun;
impl ControlRunner for NeverRun {
    fn run<'a>(&'a self, _: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async { panic!("检查点验收不应进入模型") })
    }
}

struct Archive(Arc<AtomicBool>);
impl QqReceiptArchive for Archive {
    fn identity(&self) -> PluginResult<(String, u64)> {
        self.0.store(true, Ordering::SeqCst);
        Ok(("a".repeat(32), 1))
    }
    fn store(&self, _: &[QqArchivedReceipt]) -> PluginResult<u64> {
        panic!("合成恢复不应创建冷回执")
    }
    fn get(&self, _: &str, _: &str) -> PluginResult<Option<Vec<u8>>> {
        Ok(None)
    }
    fn latest_failed(&self, _: &str, _: &str) -> PluginResult<Option<Vec<u8>>> {
        Ok(None)
    }
    fn original_identity(&self) -> PluginResult<Option<(String, u64)>> {
        Ok(Some(("a".repeat(32), 0)))
    }
}

struct Barrier {
    attached: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    entered: watch::Sender<bool>,
    release: watch::Receiver<bool>,
    allow: bool,
}
impl QqBeforeIngress for Barrier {
    fn before_ingress(&self) -> PluginFuture<'_, ()> {
        Box::pin(async move {
            assert!(self.attached.load(Ordering::SeqCst), "归档须先于检查点绑定");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.send_replace(true);
            let mut release = self.release.clone();
            release.wait_for(|value| *value).await.unwrap();
            if self.allow {
                Ok(())
            } else {
                Err(PluginError::State("合成恢复证明拒绝".into()))
            }
        })
    }
}

async fn exercise(allow: bool) {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("bridge-started");
    let bridge = directory.path().join("bridge.mjs");
    std::fs::write(&bridge, format!(
        "import fs from 'node:fs';fs.writeFileSync({},'started');console.log(JSON.stringify({{version:1,type:'ready'}}));process.stdin.resume();\n",
        serde_json::to_string(&marker).unwrap(),
    )).unwrap();
    let state = Arc::new(MemoryStateStore::default());
    state.set(&PluginId::new(QQBOT_PLUGIN_ID).unwrap(), "receipts.v1".into(),
        serde_json::to_vec(&json!({"version":6,"entries":[],"archive":{"identity":"a".repeat(32),"records":1}})).unwrap()).unwrap();
    let services = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel
        .register(Box::new(
            ControlPlugin::new(
                Arc::new(NeverRun),
                vec![PluginDependency {
                    id: PluginId::new(SESSION_PLUGIN_ID).unwrap(),
                    requirement: Some("^0.1".into()),
                }],
            )
            .unwrap(),
        ))
        .unwrap();
    kernel
        .register(Box::new(
            ConfigPlugin::new(ConfigBootstrap::new(
                directory.path(),
                vec![eve_message_api::message_schema()],
            ))
            .unwrap(),
        ))
        .unwrap();
    kernel
        .register(Box::new(RelationPlugin::rules().unwrap()))
        .unwrap();
    kernel
        .register(Box::new(MessageRouterPlugin::builtin().unwrap()))
        .unwrap();
    let attached = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered, mut entered_rx) = watch::channel(false);
    let (release, release_rx) = watch::channel(false);
    let plugin = QqBotPlugin::new(QqBotConfig {
        node_program: "node".into(),
        bridge_script: bridge,
        bridge_args: vec![],
        app_id: "app".into(),
        app_secret: "checkpoint-fixture".into(),
        sandbox: true,
        markdown: false,
    })
    .unwrap()
    .with_receipt_archive(Arc::new(Archive(attached.clone())))
    .with_before_ingress(Arc::new(Barrier {
        attached,
        calls: calls.clone(),
        entered,
        release: release_rx,
        allow,
    }));
    kernel.register(Box::new(plugin)).unwrap();
    let running = kernel.clone();
    let start = tokio::spawn(async move {
        running
            .start(&PluginId::new(QQBOT_PLUGIN_ID).unwrap())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered_rx.wait_for(|value| *value))
        .await
        .unwrap()
        .unwrap();
    assert!(!marker.exists(), "确认前不能创建桥进程");
    assert!(
        registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send_replace(true);
    let result = tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .unwrap()
        .unwrap();
    if allow {
        result.unwrap();
        let handle = registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<QqBotStatusHandle>()
            .unwrap();
        let mut status = handle.status.clone();
        tokio::time::timeout(Duration::from_secs(5), status.wait_for(|value| value.ready))
            .await
            .unwrap()
            .unwrap();
        assert!(marker.is_file());
        handle.request_stop();
        tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|value| value.closed),
        )
        .await
        .unwrap()
        .unwrap();
    } else {
        assert!(result.is_err());
        assert!(!marker.exists());
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn archive_is_attached_but_bridge_is_not_spawned_until_checkpoint_succeeds() {
    exercise(true).await;
}

#[tokio::test]
async fn failed_checkpoint_never_starts_bridge_or_reports_ready() {
    exercise(false).await;
}
