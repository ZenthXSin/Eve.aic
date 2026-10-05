use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFailure, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{
    Kernel, KernelServices,
    backends::{MemoryLogger, MemoryStateStore},
};
use eve_llm_api::{ChatMessage, ChatRole, TurnEventSink};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{
    Plugin, PluginDependency, PluginError, PluginId, PluginResult, ServiceId, StateStore,
};
use eve_qqbot_plugin::{
    QQBOT_PLUGIN_ID, QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin, QqBotStatus,
    QqBotStatusHandle, QqInteraction, QqInteractionObserver,
};
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionFailure, SessionFailureCode, SessionInput,
    SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use eve_training_plugin::TrainingPlugin;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct ReceiptStore {
    memory: MemoryStateStore,
    fail_sent: AtomicBool,
}
impl StateStore for ReceiptStore {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(owner, key)
    }
    fn set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if owner.as_str() == QQBOT_PLUGIN_ID && self.fail_sent.load(Ordering::SeqCst) {
            let ledger: Value = serde_json::from_slice(&value).unwrap();
            if ledger["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["state"] == "Sent")
            {
                return Err(PluginError::State("receipt write failed".into()));
            }
        }
        self.memory.set(owner, key, value)
    }
}
impl ReceiptStore {
    fn receipts(&self) -> Value {
        self.get(&PluginId::new(QQBOT_PLUGIN_ID).unwrap(), "receipts.v1")
            .unwrap()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_else(|| json!({"version": 1, "entries": []}))
    }
}

#[derive(Default)]
struct Observer {
    store: Arc<ReceiptStore>,
    seen: Mutex<Vec<Value>>,
    broken: u8,
}
impl QqInteractionObserver for Observer {
    fn observe(&self, interaction: &QqInteraction<'_>) -> PluginResult<()> {
        // 此断言在真实回调内读取持久层，不能由内存状态或 Completed 冒充 Sent。
        let ledger = self.store.receipts();
        let saved = ledger["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| {
                r["app_id"] == interaction.app_id()
                    && r["message"]["id"] == interaction.message_id()
            })
            .unwrap();
        assert_eq!(saved["state"], "Sent");
        assert_eq!(saved["reply"], interaction.assistant_text());
        self.seen.lock().unwrap().push(json!({
            "app": interaction.app_id(),
            "source": format!("{:?}", interaction.source()),
            "session": interaction.session(),
            "message": interaction.message_id(),
            "turn": interaction.turn_id(),
            "user": interaction.user_text(),
            "assistant": interaction.assistant_text(),
        }));
        match self.broken {
            1 => Err(PluginError::State("observer private detail".into())),
            2 => panic!("observer panic"),
            _ => Ok(()),
        }
    }
}

struct Runner {
    sessions: Arc<dyn SessionService>,
    calls: Arc<AtomicUsize>,
    failed: bool,
    tampered: bool,
}
impl ControlRunner for Runner {
    fn run<'a>(&'a self, input: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let started = self.sessions.begin(input.clone()).unwrap();
            let text = format!("回复：{}", input.text);
            let transcript = vec![
                ChatMessage::text(ChatRole::User, &input.text),
                ChatMessage::text(ChatRole::Assistant, &text),
            ];
            if self.failed {
                self.sessions
                    .fail(
                        &started.lease,
                        SessionFailure {
                            code: SessionFailureCode::Provider,
                            started_tools: Some(0),
                        },
                    )
                    .unwrap();
            } else {
                self.sessions
                    .complete(&started.lease, transcript.clone())
                    .unwrap();
            }
            RunReport {
                turn_id: Some(started.lease.turn_id),
                commit: if self.failed {
                    CommitState::Failed
                } else {
                    CommitState::Completed
                },
                text: Some(if self.tampered {
                    "未保存的文字".into()
                } else {
                    text
                }),
                transcript: Some(transcript),
                started_tools: Some(0),
                tool_results: vec![],
                failure: self.failed.then_some(RunFailure::RunnerPanicked),
            }
        })
    }
}

struct Harness {
    kernel: Kernel,
    status: QqBotStatusHandle,
    calls: Arc<AtomicUsize>,
    logger: Arc<MemoryLogger>,
    _directory: tempfile::TempDir,
}
impl Harness {
    async fn open(
        store: Arc<ReceiptStore>,
        observer: Option<Arc<Observer>>,
        scenario: Value,
        failed: bool,
        tampered: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let scenario_path = directory.path().join("scenario.json");
        std::fs::write(&scenario_path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let logger = Arc::new(MemoryLogger::default());
        let backends = KernelServices {
            state: store,
            logger: logger.clone(),
            ..KernelServices::default()
        };
        let registry = backends.registry.clone();
        let kernel = Kernel::with_services(backends);
        kernel
            .register(Box::new(SessionPlugin::new().unwrap()))
            .unwrap();
        kernel
            .start(&PluginId::new(SESSION_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let sessions = registry
            .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<SessionServiceHandle>()
            .unwrap()
            .0
            .clone();
        let calls = Arc::new(AtomicUsize::new(0));
        kernel
            .register(Box::new(
                ControlPlugin::new(
                    Arc::new(Runner {
                        sessions,
                        calls: calls.clone(),
                        failed,
                        tampered,
                    }),
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
        kernel
            .register(Box::new(TrainingPlugin::new(false).unwrap()))
            .unwrap();
        let mut plugin = QqBotPlugin::new(QqBotConfig {
            node_program: "node".into(),
            bridge_script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../connectors/qqbot/test/fake-bridge.mjs"),
            bridge_args: vec![scenario_path.into()],
            app_id: "app".into(),
            app_secret: "test-app-secret".into(),
            sandbox: true,
        })
        .unwrap()
        .with_training()
        .unwrap();
        if let Some(observer) = observer {
            plugin = plugin.with_interaction_observer(observer).unwrap();
        }
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .start(&PluginId::new(QQBOT_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let status = registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<QqBotStatusHandle>()
            .unwrap()
            .as_ref()
            .clone();
        Self {
            kernel,
            status,
            calls,
            logger,
            _directory: directory,
        }
    }
    async fn closed(&mut self) -> QqBotStatus {
        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = *self.status.status.borrow_and_update();
                if status.closed {
                    return status;
                }
                self.status.status.changed().await.unwrap();
            }
        })
        .await;
        if closed.is_err() {
            self.status.request_stop();
        }
        // 只停止本 Harness 注册的内核与其持有的 Child，不读取或发送任意 PID 信号。
        let stopped = self.kernel.stop_all().await;
        let status = closed.expect("QQ bridge did not close");
        assert_eq!(stopped.is_err(), status.terminal_error);
        status
    }
}

fn message(id: &str, text: &str) -> Value {
    json!({"id": id, "scope": "c2c", "target_id": "alice", "user_id": "alice", "text": text, "expected": format!("回复：{text}")})
}
fn observer(store: &Arc<ReceiptStore>) -> Arc<Observer> {
    Arc::new(Observer {
        store: store.clone(),
        ..Observer::default()
    })
}

#[test]
fn session_dependency_is_explicit_and_added_once() {
    let plugin = || {
        QqBotPlugin::new(QqBotConfig {
            node_program: "node".into(),
            bridge_script: "unused".into(),
            bridge_args: vec![],
            app_id: "app".into(),
            app_secret: "test-app-secret".into(),
            sandbox: true,
        })
        .unwrap()
    };
    assert!(
        !plugin()
            .manifest()
            .dependencies
            .iter()
            .any(|d| d.id.as_str() == SESSION_PLUGIN_ID)
    );
    let observer = observer(&Arc::new(ReceiptStore::default()));
    let plugin = plugin()
        .with_interaction_observer(observer.clone())
        .unwrap()
        .with_interaction_observer(observer)
        .unwrap();
    assert_eq!(
        plugin
            .manifest()
            .dependencies
            .iter()
            .filter(|d| d.id.as_str() == SESSION_PLUGIN_ID)
            .count(),
        1
    );
    plugin.manifest().validate().unwrap();
}

#[tokio::test]
async fn delivered_ordinary_turn_is_observed_once_and_restart_never_backfills() {
    let store = Arc::new(ReceiptStore::default());
    let observer = observer(&store);
    let mut h = Harness::open(
        store.clone(),
        Some(observer.clone()),
        json!({"messages": [message("m1", "原始输入")]}),
        false,
        false,
    )
    .await;
    let status = h.closed().await;
    assert!(!status.terminal_error);
    assert_eq!((status.completed, status.sent, status.failed), (1, 1, 0));
    let seen = observer.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["message"], "m1");
    assert_eq!(seen[0]["user"], "原始输入");
    assert_eq!(seen[0]["assistant"], "回复：原始输入");
    assert_eq!(seen[0]["source"], "DirectMessage");
    assert_eq!(seen[0]["turn"], 1);
    let before = store.receipts();
    let mut h = Harness::open(
        store.clone(),
        Some(observer.clone()),
        json!({"messages": [message("m1", "原始输入")]}),
        false,
        false,
    )
    .await;
    let status = h.closed().await;
    assert!(!status.terminal_error);
    assert_eq!((status.completed, status.sent), (0, 0));
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    assert_eq!(observer.seen.lock().unwrap().len(), 1);
    assert_eq!(store.receipts(), before);
}

#[tokio::test]
async fn delivery_and_sent_commit_are_both_required() {
    for sent_write_fails in [false, true] {
        let store = Arc::new(ReceiptStore::default());
        store.fail_sent.store(sent_write_fails, Ordering::SeqCst);
        let observer = observer(&store);
        let mut h = Harness::open(
            store.clone(),
            Some(observer.clone()),
            json!({"messages": [message("m1", "问题")], "send_fail": !sent_write_fails}),
            false,
            false,
        )
        .await;
        let status = h.closed().await;
        assert_eq!(status.completed, 1);
        assert_eq!(status.sent, 0);
        assert_eq!(status.terminal_error, sent_write_fails);
        assert_eq!(
            store.receipts()["entries"][0]["state"],
            if sent_write_fails {
                "ReplyPending"
            } else {
                "Failed"
            }
        );
        assert!(observer.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn completed_session_waits_for_actual_delivery_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let pending = directory.path().join("reply-pending");
    let release = directory.path().join("release-delivery");
    let store = Arc::new(ReceiptStore::default());
    let observer = observer(&store);
    let mut input = message("m1", "等待发送确认");
    input["hold_delivery"] = json!(true);
    let mut h = Harness::open(
        store.clone(),
        Some(observer.clone()),
        json!({"script": [
            {"send": input},
            {"wait_command": {"type": "reply", "id": "m1"}},
            {"touch": pending},
            {"wait_file": release},
            {"delivery": "m1"},
        ]}),
        false,
        false,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pending.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.status.status.borrow().completed, 1);
    assert_eq!(h.status.status.borrow().sent, 0);
    assert_eq!(store.receipts()["entries"][0]["state"], "ReplyPending");
    assert!(observer.seen.lock().unwrap().is_empty());
    std::fs::write(release, "ready").unwrap();
    assert_eq!(h.closed().await.sent, 1);
    assert_eq!(observer.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn observer_failure_preserves_sent_closes_and_never_retries() {
    for broken in [1, 2] {
        let store = Arc::new(ReceiptStore::default());
        let observer = Arc::new(Observer {
            store: store.clone(),
            broken,
            ..Observer::default()
        });
        let mut h = Harness::open(
            store.clone(),
            Some(observer.clone()),
            json!({"messages": [message("m1", "问题")]}),
            false,
            false,
        )
        .await;
        let status = h.closed().await;
        assert!(status.terminal_error);
        assert_eq!(status.sent, 1);
        assert_eq!(store.receipts()["entries"][0]["state"], "Sent");
        assert_eq!(observer.seen.lock().unwrap().len(), 1);
        let logs = h.logger.snapshot().unwrap();
        assert!(
            logs.records
                .iter()
                .any(|r| r.entry.message == "interaction_observer_failed_no_retry")
        );
        assert!(!format!("{logs:?}").contains("observer private detail"));
        let mut h = Harness::open(
            store,
            Some(observer.clone()),
            json!({"messages": [message("m1", "问题")]}),
            false,
            false,
        )
        .await;
        assert!(!h.closed().await.terminal_error);
        assert_eq!(observer.seen.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn completed_report_cannot_replace_session_evidence() {
    for failed in [true, false] {
        let store = Arc::new(ReceiptStore::default());
        let observer = observer(&store);
        let mut h = Harness::open(
            store,
            Some(observer.clone()),
            json!({"messages": [message("m1", "问题")]}),
            failed,
            !failed,
        )
        .await;
        let status = h.closed().await;
        assert_eq!(status.sent, 0);
        assert_eq!(status.terminal_error, !failed);
        assert!(observer.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn training_commands_and_control_replacement_are_excluded() {
    let store = Arc::new(ReceiptStore::default());
    let observer = observer(&store);
    let mut replacement = message("m2", "/new 替代任务");
    replacement["expected"] = json!("回复：替代任务");
    let mut status_command = message("m4", "/train status");
    status_command["expected"] = json!("当前会话：主动提问训练已开启。");
    let mut h = Harness::open(store, Some(observer.clone()), json!({"messages": [message("m1", "原始任务"), replacement, message("m3", "/train start"), status_command]}), false, false).await;
    let status = h.closed().await;
    assert!(!status.terminal_error);
    assert_eq!(status.sent, 4);
    assert_eq!(h.calls.load(Ordering::SeqCst), 3);
    let seen = observer.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["message"], "m1");
}

#[tokio::test]
async fn without_observer_existing_completed_delivery_does_not_read_evidence() {
    let store = Arc::new(ReceiptStore::default());
    let mut input = message("m1", "问题");
    input["expected"] = json!("未保存的文字");
    let mut h = Harness::open(store, None, json!({"messages": [input]}), false, true).await;
    let status = h.closed().await;
    assert!(!status.terminal_error);
    assert_eq!(status.sent, 1);
}
