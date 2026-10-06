//! 真实 QQ 插件与受控 Node 子进程的在途普通文字验收；不访问模型或 QQ 网络。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFailure, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole, LlmError, TurnEvent, TurnEventKind, TurnEventSink};
use eve_message_api::{
    IntentPart, MessageIntent, RelationDecision, RelationFuture, RelationInput, RelationJudge,
    TextSpan,
};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin, RulesJudge};
use eve_plugin_api::{PluginDependency, PluginId, ServiceId, StateStore};
use eve_qqbot_plugin::{
    QQBOT_PLUGIN_ID, QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin, QqBotStatus,
    QqBotStatusHandle, QqCommandHandler, QqCommandInput,
};
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionFailure, SessionFailureCode, SessionInput,
    SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

async fn wait_file(path: &Path) {
    while !path.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

struct Runner {
    directory: PathBuf,
    sessions: Arc<dyn SessionService>,
    calls: Arc<Mutex<Vec<SessionInput>>>,
    started_tools: u64,
}
impl ControlRunner for Runner {
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(input.clone());
            let started = self.sessions.begin(input.clone()).unwrap();
            sink.emit(TurnEvent {
                turn_id: Some(started.lease.turn_id),
                kind: TurnEventKind::ProviderStarted { request: 1 },
            })
            .await
            .unwrap();
            if input.text == "原始任务" {
                std::fs::write(self.directory.join("runner-started"), b"ready").unwrap();
                let release = self.directory.join("runner-release");
                let cancelled = tokio::select! {
                    _ = sink.closed() => true,
                    _ = wait_file(&release) => false,
                };
                if cancelled {
                    self.sessions
                        .fail(
                            &started.lease,
                            SessionFailure {
                                code: SessionFailureCode::Cancelled,
                                started_tools: Some(self.started_tools),
                            },
                        )
                        .unwrap();
                    return RunReport {
                        turn_id: Some(started.lease.turn_id),
                        commit: CommitState::Failed,
                        text: None,
                        transcript: None,
                        started_tools: Some(self.started_tools),
                        tool_results: vec![],
                        failure: Some(RunFailure::Execution(LlmError::Cancelled)),
                    };
                }
            }
            let text = format!("回复：{}", input.text);
            let transcript = vec![
                ChatMessage::text(ChatRole::User, &input.text),
                ChatMessage::text(ChatRole::Assistant, &text),
            ];
            self.sessions
                .complete(&started.lease, transcript.clone())
                .unwrap();
            RunReport {
                turn_id: Some(started.lease.turn_id),
                commit: CommitState::Completed,
                text: Some(text),
                transcript: Some(transcript),
                started_tools: Some(0),
                tool_results: vec![],
                failure: None,
            }
        })
    }
}

struct Judge {
    directory: PathBuf,
    hold: bool,
    calls: Arc<Mutex<Vec<RelationInput>>>,
    dropped: Arc<AtomicBool>,
}
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl RelationJudge for Judge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        Box::pin(async move {
            if input.message.text.starts_with('/') {
                return RulesJudge.judge(input).await;
            }
            self.calls.lock().unwrap().push(input.clone());
            let _guard = DropFlag(self.dropped.clone());
            std::fs::write(self.directory.join("judge-started"), b"ready").unwrap();
            if self.hold {
                wait_file(&self.directory.join("judge-release")).await;
            }
            Ok(RelationDecision {
                target: input.message.target.clone(),
                message_id: input.message.message_id.clone(),
                parts: vec![IntentPart {
                    intent: MessageIntent::Correction,
                    confidence: 100,
                    span: Some(TextSpan {
                        start: 0,
                        end: input.message.text.len(),
                    }),
                }],
                explanation: "可控测试判断器".into(),
            })
        })
    }
}

struct ReadOnlyHandler(PathBuf);
impl QqCommandHandler for ReadOnlyHandler {
    fn handle(&self, input: QqCommandInput<'_>) -> eve_plugin_api::PluginResult<Option<String>> {
        std::fs::write(&self.0, b"handled").unwrap();
        Ok((input.text == "/memory status").then(|| "只读状态".into()))
    }
}

struct Harness {
    kernel: Kernel,
    status: QqBotStatusHandle,
    store: Arc<MemoryStateStore>,
    calls: Arc<Mutex<Vec<SessionInput>>>,
    judgments: Arc<Mutex<Vec<RelationInput>>>,
    dropped: Arc<AtomicBool>,
    directory: tempfile::TempDir,
}
impl Harness {
    async fn open(
        directory: tempfile::TempDir,
        store: Arc<MemoryStateStore>,
        scenario: Value,
        enabled: Option<bool>,
        hold: bool,
        tools: u64,
    ) -> Self {
        let scenario_path = directory.path().join("scenario.json");
        let mut scenario = scenario;
        scenario["error_file"] = json!(directory.path().join("bridge-error"));
        std::fs::write(&scenario_path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let backends = KernelServices {
            state: store.clone(),
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
        let calls = Arc::new(Mutex::new(vec![]));
        kernel
            .register(Box::new(
                ControlPlugin::new(
                    Arc::new(Runner {
                        directory: directory.path().into(),
                        sessions,
                        calls: calls.clone(),
                        started_tools: tools,
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
        let judgments = Arc::new(Mutex::new(vec![]));
        let dropped = Arc::new(AtomicBool::new(false));
        kernel
            .register(Box::new(
                RelationPlugin::new(Arc::new(Judge {
                    directory: directory.path().into(),
                    hold,
                    calls: judgments.clone(),
                    dropped: dropped.clone(),
                }))
                .unwrap(),
            ))
            .unwrap();
        kernel
            .register(Box::new(MessageRouterPlugin::builtin().unwrap()))
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
        .unwrap();
        plugin = plugin.with_command_handler(Arc::new(ReadOnlyHandler(
            directory.path().join("command-handled"),
        )));
        if let Some(enabled) = enabled {
            plugin = plugin.with_natural_message_judgement(enabled);
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
            store,
            calls,
            judgments,
            dropped,
            directory,
        }
    }
    async fn close(&mut self) -> QqBotStatus {
        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let current = *self.status.status.borrow_and_update();
                if current.closed {
                    return current;
                }
                self.status.status.changed().await.unwrap();
            }
        })
        .await;
        if closed.is_err() {
            self.status.request_stop();
            tokio::time::timeout(Duration::from_secs(3), async {
                while !self.status.status.borrow_and_update().closed {
                    self.status.status.changed().await.unwrap();
                }
            })
            .await
            .expect("owned QQ channel did not stop");
        }
        let stopped = self.kernel.stop_all().await;
        let status = closed.unwrap_or_else(|_| {
            panic!(
                "bridge timeout; error={:?}",
                std::fs::read_to_string(self.directory.path().join("bridge-error"))
            )
        });
        assert!(
            !status.terminal_error,
            "bridge error: {:?}",
            std::fs::read_to_string(self.directory.path().join("bridge-error"))
        );
        stopped.unwrap();
        status
    }
    fn receipts(&self) -> Value {
        serde_json::from_slice(
            &self
                .store
                .get(&PluginId::new(QQBOT_PLUGIN_ID).unwrap(), "receipts.v1")
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }
}
fn message(id: &str, user: &str, text: &str) -> Value {
    json!({"id":id,"scope":"c2c","target_id":user,"user_id":user,"text":text,"expected":format!("回复：{text}")})
}
fn retired(id: &str, user: &str, text: &str) -> Value {
    let mut value = message(id, user, text);
    value["expected_type"] = json!("finish");
    value
}
fn correction(id: &str, text: &str) -> Value {
    let mut value = message(id, "alice", text);
    value["expected_contains"] = json!([
        "request_kind",
        "revision",
        "原始任务",
        serde_json::to_string(text).unwrap()
    ]);
    value
}

#[tokio::test]
async fn default_and_explicitly_disabled_keep_ordinary_messages_queued() {
    for enabled in [None, Some(false)] {
        let directory = tempfile::tempdir().unwrap();
        let scenario = json!({"script":[
            {"send":message("old","alice","原始任务")},
            {"wait_file":directory.path().join("runner-started")},
            {"send":message("natural","alice","请改用中文")},
            {"touch":directory.path().join("runner-release")},
            {"wait_command":{"type":"reply","id":"old"}},
            {"wait_command":{"type":"reply","id":"natural"}}
        ]});
        let mut harness = Harness::open(
            directory,
            Arc::new(MemoryStateStore::default()),
            scenario,
            enabled,
            false,
            0,
        )
        .await;
        assert_eq!(harness.close().await.sent, 2);
        assert!(harness.judgments.lock().unwrap().is_empty());
        let inputs = harness.calls.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[1].text, "请改用中文");
    }
}

#[tokio::test]
async fn enabled_revises_raw_text_and_finished_generation_starts_a_fresh_turn() {
    let directory = tempfile::tempdir().unwrap();
    let raw = "请改用中文，保留\n  原始空白和引号\"。";
    let scenario = json!({"script":[
        {"send":retired("old","alice","原始任务")},
        {"wait_file":directory.path().join("runner-started")},
        {"send":correction("natural", raw)},
        {"wait_command":{"type":"finish","id":"old"}},
        {"wait_command":{"type":"reply","id":"natural"}},
        {"send":message("next","alice","新的普通任务")},
        {"wait_command":{"type":"reply","id":"next"}}
    ]});
    let mut harness = Harness::open(
        directory,
        Arc::new(MemoryStateStore::default()),
        scenario,
        Some(true),
        false,
        0,
    )
    .await;
    assert_eq!(harness.close().await.sent, 2);
    let inputs = harness.calls.lock().unwrap();
    assert_eq!(inputs.len(), 3);
    let revision: Value = serde_json::from_str(&inputs[1].text).unwrap();
    assert_eq!(revision["base_request"], "原始任务");
    assert_eq!(revision["changes"][0]["text"], raw);
    assert_eq!(inputs[2].text, "新的普通任务");
    assert_eq!(harness.judgments.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn waiting_judgment_does_not_block_other_user_or_explicit_cancel() {
    let directory = tempfile::tempdir().unwrap();
    let mut cancel = message("cancel", "alice", "/cancel");
    cancel["expected_contains"] = json!("已取消当前任务");
    let scenario = json!({"script":[
        {"send":retired("old","alice","原始任务")},
        {"wait_file":directory.path().join("runner-started")},
        {"send":retired("natural","alice","请改用中文")},
        {"wait_file":directory.path().join("judge-started")},
        {"send":message("other","bob","另一个会话")},
        {"wait_command":{"type":"reply","id":"other"}},
        {"send":cancel},
        {"wait_command":{"type":"finish","id":"natural"}},
        {"wait_command":{"type":"finish","id":"old"}},
        {"wait_command":{"type":"reply","id":"cancel"}}
    ]});
    let mut harness = Harness::open(
        directory,
        Arc::new(MemoryStateStore::default()),
        scenario,
        Some(true),
        true,
        0,
    )
    .await;
    assert_eq!(harness.close().await.sent, 2);
    assert!(harness.dropped.load(Ordering::SeqCst));
    let inputs = harness.calls.lock().unwrap();
    assert_eq!(inputs.len(), 2);
    assert_ne!(inputs[0].key, inputs[1].key);
    let judgments = harness.judgments.lock().unwrap();
    assert_eq!(judgments.len(), 1);
    assert_eq!(judgments[0].message.target.session, inputs[0].key);
}

#[tokio::test]
async fn started_tools_require_clarification_and_never_start_revision() {
    let directory = tempfile::tempdir().unwrap();
    let mut natural = message("natural", "alice", "请改用中文");
    natural["expected_contains"] = json!("工具");
    let scenario = json!({"script":[
        {"send":retired("old","alice","原始任务")},
        {"wait_file":directory.path().join("runner-started")},
        {"send":natural},
        {"wait_command":{"type":"finish","id":"old"}},
        {"wait_command":{"type":"reply","id":"natural"}}
    ]});
    let mut harness = Harness::open(
        directory,
        Arc::new(MemoryStateStore::default()),
        scenario,
        Some(true),
        false,
        1,
    )
    .await;
    assert_eq!(harness.close().await.sent, 1);
    assert_eq!(harness.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn shutdown_settles_judgment_and_restart_does_not_replay_received_messages() {
    let directory = tempfile::tempdir().unwrap();
    let scenario = json!({"script":[
        {"send":retired("old","alice","原始任务")},
        {"wait_file":directory.path().join("runner-started")},
        {"send":retired("natural","alice","请改用中文")},
        {"wait_file":directory.path().join("judge-started")},
        {"send":retired("queued-natural","alice","旧代排队纠正")},
        {"send":message("other","bob","确保排队输入已经保存")},
        {"wait_command":{"type":"reply","id":"other"}},
        {"touch":directory.path().join("ready-to-stop")},
        {"wait_file":directory.path().join("never-release")}
    ]});
    let store = Arc::new(MemoryStateStore::default());
    let mut harness = Harness::open(directory, store.clone(), scenario, Some(true), true, 0).await;
    tokio::time::timeout(
        Duration::from_secs(3),
        wait_file(&harness.directory.path().join("ready-to-stop")),
    )
    .await
    .unwrap();
    harness.status.request_stop();
    assert_eq!(harness.close().await.sent, 1);
    assert!(harness.dropped.load(Ordering::SeqCst));
    let before = harness.receipts();
    assert_eq!(before["entries"].as_array().unwrap().len(), 4);
    let scenario = json!({"messages":[retired("old","alice","原始任务"),retired("natural","alice","请改用中文"),retired("queued-natural","alice","旧代排队纠正"),message("fresh","alice","重启后的新输入")]});
    let mut restarted = Harness::open(
        tempfile::tempdir().unwrap(),
        store,
        scenario,
        Some(true),
        false,
        0,
    )
    .await;
    assert_eq!(restarted.close().await.sent, 1);
    assert_eq!(restarted.calls.lock().unwrap().len(), 1);
    assert!(restarted.judgments.lock().unwrap().is_empty());
    let after = restarted.receipts();
    assert_eq!(after["entries"][0], before["entries"][0]);
    for index in 1..4 {
        assert_eq!(after["entries"][index], before["entries"][index]);
    }
}

#[tokio::test]
async fn readonly_and_invalid_slash_preserve_inflight_natural_judgment() {
    for text in [
        "/memory status",
        "/cancelxxx",
        "/cancel extra",
        "/new x\n/correct y",
        "/answer 未验证引用",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut command = retired("command", "alice", text);
        if text == "/memory status" {
            command["expected_type"] = json!("reply");
            command["expected"] = json!("只读状态");
        }
        let outcome = if text == "/memory status" {
            "reply"
        } else {
            "finish"
        };
        let scenario = json!({"script":[
            {"send":retired("old","alice","原始任务")},
            {"wait_file":directory.path().join("runner-started")},
            {"send":correction("natural", "请改用中文")},
            {"wait_file":directory.path().join("judge-started")},
            {"send":command},
            {"wait_file":directory.path().join("command-handled")},
            {"touch":directory.path().join("judge-release")},
            {"wait_command":{"type":"reply","id":"natural"}},
            {"wait_command":{"type":outcome,"id":"command"}}
        ]});
        let mut harness = Harness::open(
            directory,
            Arc::new(MemoryStateStore::default()),
            scenario,
            Some(true),
            true,
            0,
        )
        .await;
        assert_eq!(
            harness.close().await.sent,
            if text == "/memory status" { 2 } else { 1 }
        );
        assert_eq!(harness.calls.lock().unwrap().len(), 2, "{text}");
        assert_eq!(harness.judgments.lock().unwrap().len(), 1, "{text}");
    }
}

#[tokio::test]
async fn queued_natural_message_retains_its_captured_generation() {
    let directory = tempfile::tempdir().unwrap();
    let scenario = json!({"script":[
        {"send":retired("old","alice","原始任务")},
        {"wait_file":directory.path().join("runner-started")},
        {"send":correction("natural", "请改用中文")},
        {"wait_file":directory.path().join("judge-started")},
        {"send":retired("stale","alice","这个要求属于旧代")},
        {"send":message("other","bob","确认旧代消息已准入")},
        {"wait_command":{"type":"reply","id":"other"}},
        {"touch":directory.path().join("judge-release")},
        {"wait_command":{"type":"reply","id":"natural"}},
        {"wait_command":{"type":"finish","id":"stale"}}
    ]});
    let mut harness = Harness::open(
        directory,
        Arc::new(MemoryStateStore::default()),
        scenario,
        Some(true),
        true,
        0,
    )
    .await;
    assert_eq!(harness.close().await.sent, 2);
    let inputs = harness.calls.lock().unwrap();
    assert_eq!(inputs.len(), 3);
    assert!(
        !inputs
            .iter()
            .any(|input| input.text.contains("这个要求属于旧代"))
    );
    assert_eq!(harness.judgments.lock().unwrap().len(), 1);
    let ledger = harness.receipts();
    let stale = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["message"]["id"] == "stale")
        .unwrap();
    assert_eq!(stale["state"], "Failed");
}
