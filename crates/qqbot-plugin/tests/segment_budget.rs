//! 通过真实插件、Session 和受控 Node 子进程验证分段预算，不连接模型或 QQ。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole, TurnEventSink};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginDependency, PluginId, PluginResult, ServiceId, StateStore};
use eve_qqbot_plugin::{
    QQBOT_PLUGIN_ID, QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin, QqBotStatus,
    QqBotStatusHandle, QqInteraction, QqInteractionObserver,
};
use eve_segment_api::{
    Segment, SegmentError, SegmentLimits, SegmentPlan, SegmentPlanner, SegmentRequest,
    SegmentResult,
};
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionInput, SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn bytes(store: &MemoryStateStore, owner: &str, key: &str) -> Vec<u8> {
    store
        .get(&PluginId::new(owner).unwrap(), key)
        .unwrap()
        .unwrap()
}
fn receipts(store: &MemoryStateStore) -> Value {
    serde_json::from_slice(&bytes(store, QQBOT_PLUGIN_ID, "receipts.v1")).unwrap()
}

struct Observer {
    store: Arc<MemoryStateStore>,
    seen: Mutex<Vec<String>>,
}
impl QqInteractionObserver for Observer {
    fn observe(&self, interaction: &QqInteraction<'_>) -> PluginResult<()> {
        let ledger = receipts(&self.store);
        let saved = ledger["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["message"]["id"] == interaction.message_id())
            .unwrap();
        assert_eq!(saved["state"], "Sent");
        assert_eq!(saved["reply"], interaction.assistant_text());
        if let Some(parts) = saved["segments"]["parts"].as_array() {
            assert!(parts.iter().all(|part| part["state"] == "Sent"));
        }
        self.seen
            .lock()
            .unwrap()
            .push(interaction.assistant_text().into());
        Ok(())
    }
}

struct Runner {
    sessions: Arc<dyn SessionService>,
    calls: Arc<AtomicUsize>,
}
impl ControlRunner for Runner {
    fn run<'a>(&'a self, input: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let started = self.sessions.begin(input.clone()).unwrap();
            let transcript = vec![
                ChatMessage::text(ChatRole::User, &input.text),
                ChatMessage::text(ChatRole::Assistant, &input.text),
            ];
            self.sessions
                .complete(&started.lease, transcript.clone())
                .unwrap();
            RunReport {
                turn_id: Some(started.lease.turn_id),
                commit: CommitState::Completed,
                text: Some(input.text),
                transcript: Some(transcript),
                started_tools: Some(0),
                tool_results: vec![],
                failure: None,
            }
        })
    }
}

#[derive(Clone, Copy)]
enum Strategy {
    Error,
    Invalid,
    Paragraphs,
    Single,
}
struct Planner {
    strategy: Strategy,
    calls: AtomicUsize,
}
impl SegmentPlanner for Planner {
    fn plan(&self, request: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.strategy {
            Strategy::Error => Err(SegmentError::Planner("fixture failure".into())),
            Strategy::Invalid => Ok(SegmentPlan {
                planner: "invalid-fixture".into(),
                segments: vec![],
            }),
            Strategy::Single => SegmentPlan::single("single-fixture", request.text),
            Strategy::Paragraphs => Ok(SegmentPlan {
                planner: "paragraph-fixture".into(),
                segments: request
                    .text
                    .split("\n\n")
                    .scan(0, |cursor, part| {
                        let start = *cursor;
                        *cursor += part.len() + 2;
                        Some(Segment {
                            start,
                            end: start + part.len(),
                            pause_before_ms: 0,
                        })
                    })
                    .collect(),
            }),
        }
    }
}

struct Harness {
    kernel: Kernel,
    status: QqBotStatusHandle,
    calls: Arc<AtomicUsize>,
    directory: tempfile::TempDir,
}
impl Harness {
    async fn open(
        store: Arc<MemoryStateStore>,
        observer: Arc<Observer>,
        planner: Arc<Planner>,
        script: Value,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let scenario = directory.path().join("scenario.json");
        std::fs::write(
            &scenario,
            serde_json::to_vec(&json!({
                "script": script,
                "events_file": directory.path().join("events.jsonl"),
                "error_file": directory.path().join("bridge-error"),
            }))
            .unwrap(),
        )
        .unwrap();
        let backends = KernelServices {
            state: store,
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
        let plugin = QqBotPlugin::new(QqBotConfig {
            node_program: "node".into(),
            bridge_script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../connectors/qqbot/test/fake-bridge.mjs"),
            bridge_args: vec![scenario.into()],
            app_id: "app".into(),
            app_secret: "test-app-secret".into(),
            sandbox: true,
        })
        .unwrap()
        .with_segmenter(
            planner,
            SegmentLimits {
                max_segments: 3,
                max_segment_bytes: 8,
                max_pause_ms: 0,
            },
        )
        .unwrap()
        .with_interaction_observer(observer)
        .unwrap();
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
            directory,
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
        // 仅停止本内核拥有的 Node Child，不读取状态 PID 或向外部进程发信号。
        let stopped = self.kernel.stop_all().await;
        let bridge_error = std::fs::read_to_string(self.directory.path().join("bridge-error"));
        assert!(bridge_error.is_err(), "{bridge_error:?}");
        let status = closed.expect("QQ bridge did not close");
        assert!(!status.terminal_error, "{stopped:?}");
        stopped.unwrap();
        status
    }
    fn outgoing(&self) -> Vec<Value> {
        std::fs::read_to_string(self.directory.path().join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["direction"] == "out" && event["type"] != "stop")
            .collect()
    }
}

fn fixtures(strategy: Strategy) -> (Arc<MemoryStateStore>, Arc<Observer>, Arc<Planner>) {
    let store = Arc::new(MemoryStateStore::default());
    let observer = Arc::new(Observer {
        store: store.clone(),
        seen: Mutex::new(Vec::new()),
    });
    let planner = Arc::new(Planner {
        strategy,
        calls: AtomicUsize::new(0),
    });
    (store, observer, planner)
}
fn message(id: &str, text: &str, expected_type: &str) -> Value {
    json!({"id": id, "scope": "c2c", "target_id": "alice", "user_id": "alice",
        "text": text, "expected": text.trim(), "expected_type": expected_type})
}

#[tokio::test]
async fn unavailable_plans_fail_only_the_delivery_and_restart_never_replays_them() {
    for strategy in [Strategy::Error, Strategy::Invalid] {
        let (store, observer, planner) = fixtures(strategy);
        let rejected = message("oversize", "this exceeds eight bytes", "finish");
        let mut harness = Harness::open(
            store.clone(),
            observer.clone(),
            planner.clone(),
            json!([
                {"send": rejected}, {"wait_command": {"id": "oversize", "type": "finish"}},
                {"send": message("next", "okay", "reply")},
                {"wait_command": {"id": "next", "type": "reply"}},
            ]),
        )
        .await;
        let status = harness.closed().await;
        assert_eq!((status.completed, status.sent, status.failed), (2, 1, 1));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
        assert_eq!(planner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(*observer.seen.lock().unwrap(), ["okay"]);
        let outgoing = harness.outgoing();
        assert_eq!(outgoing.len(), 2);
        assert_eq!(outgoing[0]["type"], "finish");
        assert_eq!(outgoing[1]["text"], "okay");
        let ledger = receipts(&store);
        assert_eq!(ledger["version"], 1);
        assert_eq!(ledger["entries"][0]["state"], "Failed");
        assert_eq!(ledger["entries"][0]["reply"], "this exceeds eight bytes");
        assert!(ledger["entries"][0].get("segments").is_none());
        let sessions_before = bytes(&store, SESSION_PLUGIN_ID, "sessions.v1");
        let sessions: Value = serde_json::from_slice(&sessions_before).unwrap();
        let turns: Vec<_> = sessions["sessions"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|session| session["turns"].as_array().unwrap())
            .collect();
        assert_eq!(turns.len(), 2);
        assert!(
            turns
                .iter()
                .all(|turn| turn["status"]["state"] == "Completed")
        );
        let before = bytes(&store, QQBOT_PLUGIN_ID, "receipts.v1");
        let mut restarted = Harness::open(
            store.clone(),
            observer.clone(),
            planner.clone(),
            json!([
                {"send": rejected}, {"wait_command": {"id": "oversize", "type": "finish"}},
            ]),
        )
        .await;
        let status = restarted.closed().await;
        assert_eq!((status.completed, status.sent, status.failed), (0, 0, 0));
        assert_eq!(restarted.calls.load(Ordering::SeqCst), 0);
        assert_eq!(planner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(*observer.seen.lock().unwrap(), ["okay"]);
        assert_eq!(bytes(&store, QQBOT_PLUGIN_ID, "receipts.v1"), before);
        assert_eq!(
            bytes(&store, SESSION_PLUGIN_ID, "sessions.v1"),
            sessions_before
        );
        assert_eq!(restarted.outgoing()[0]["type"], "finish");
    }
}

#[tokio::test]
async fn valid_parts_may_together_exceed_the_single_part_budget() {
    let (store, observer, planner) = fixtures(Strategy::Paragraphs);
    let text = "first\n\nsecond";
    assert!(text.len() > 8);
    let mut inbound = message("parts", text, "reply");
    inbound["expected_segments"] = json!(["first", "second"]);
    let mut harness = Harness::open(
        store.clone(),
        observer.clone(),
        planner,
        json!([
            {"send": inbound}, {"wait_command": {"id": "parts", "type": "segment", "count": 2}},
        ]),
    )
    .await;
    let status = harness.closed().await;
    assert_eq!((status.completed, status.sent, status.failed), (1, 1, 0));
    let outgoing = harness.outgoing();
    assert_eq!(outgoing.len(), 2);
    for (index, part) in ["first", "second"].iter().enumerate() {
        assert_eq!(outgoing[index]["type"], "segment");
        assert_eq!(outgoing[index]["index"], index);
        assert_eq!(outgoing[index]["text"], *part);
    }
    assert_eq!(receipts(&store)["version"], 2);
    assert_eq!(*observer.seen.lock().unwrap(), [text]);
}

#[tokio::test]
async fn valid_single_or_fallback_sends_only_the_validated_slice() {
    for strategy in [Strategy::Single, Strategy::Error, Strategy::Invalid] {
        let (store, observer, planner) = fixtures(strategy);
        let text = " 12345678 \n";
        assert!(text.len() > 8);
        let mut harness = Harness::open(
            store.clone(),
            observer.clone(),
            planner,
            json!([
                {"send": message("trimmed", text, "reply")},
                {"wait_command": {"id": "trimmed", "type": "reply"}},
            ]),
        )
        .await;
        let status = harness.closed().await;
        assert_eq!((status.completed, status.sent, status.failed), (1, 1, 0));
        let outgoing = harness.outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["type"], "reply");
        assert_eq!(outgoing[0]["text"], "12345678");
        let ledger = receipts(&store);
        assert_eq!(ledger["version"], 1);
        assert_eq!(ledger["entries"][0]["state"], "Sent");
        assert_eq!(ledger["entries"][0]["reply"], text);
        assert!(ledger["entries"][0].get("segments").is_none());
        assert_eq!(*observer.seen.lock().unwrap(), [text]);
    }
}
