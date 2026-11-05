//! 真实 Rust/Node 通道与长运行子进程；只替换模型和外部平台，不向生产 QQ 发消息。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmFuture, ToolCall, TurnEvent, TurnEventKind,
    TurnEventSink, TurnSteeringSource,
};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginId, PluginResult, ServiceId};
use eve_qqbot_plugin::*;
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionInput, SessionKey, SessionService,
    SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Archive(Mutex<Vec<QqOriginalMessage>>);
impl QqReceiptArchive for Archive {
    fn identity(&self) -> PluginResult<(String, u64)> {
        Ok(("0123456789abcdef0123456789abcdef".into(), 0))
    }
    fn store(&self, _: &[QqArchivedReceipt]) -> PluginResult<u64> {
        panic!("夹具不归档")
    }
    fn get(&self, _: &str, _: &str) -> PluginResult<Option<Vec<u8>>> {
        Ok(None)
    }
    fn latest_failed(&self, _: &str, _: &str) -> PluginResult<Option<Vec<u8>>> {
        Ok(None)
    }
    fn original_identity(&self) -> PluginResult<Option<(String, u64)>> {
        Ok(Some((
            "abcdef0123456789abcdef0123456789".into(),
            self.0.lock().unwrap().len() as u64,
        )))
    }
    fn store_original(&self, record: &QqOriginalMessage) -> PluginResult<u64> {
        let mut rows = self.0.lock().unwrap();
        rows.push(record.clone());
        Ok(rows.len() as u64)
    }
    fn original_is_current_bound(
        &self,
        app: &str,
        session: &SessionKey,
        id: &str,
        expected: &QqOriginalBinding,
    ) -> PluginResult<bool> {
        let rows = self.0.lock().unwrap();
        assert!(expected.matches("abcdef0123456789abcdef0123456789", rows.len() as u64));
        Ok(rows
            .iter()
            .rev()
            .find(|r| r.app_id == app && r.session_key().ok().as_ref() == Some(session))
            .is_some_and(|r| r.message_id == id))
    }
}

#[derive(Default)]
struct Observed {
    runs: AtomicUsize,
    tool_running: AtomicBool,
    completed: AtomicBool,
    consumed: Mutex<Vec<String>>,
}
struct Runner {
    sessions: Arc<dyn SessionService>,
    live: Arc<LiveDialogue>,
    directory: PathBuf,
    observed: Arc<Observed>,
}
impl ControlRunner for Runner {
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        sink: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        let metadata = metadata.expect("真实入站元数据");
        metadata.validate_scope(&input.key).unwrap();
        assert!(
            matches!(metadata.review,eve_control_api::ControlReviewEvidence::ChannelAnchor(anchor) if anchor=="initial-input")
        );
        self.run(input, sink)
    }
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            self.observed.runs.fetch_add(1, Ordering::SeqCst);
            let started = self.sessions.begin(input.clone()).unwrap();
            let turn_id = started.lease.turn_id;
            let scope = ContextScope {
                session_id: input.key.session_id.clone(),
                user_id: input.key.user_id.clone(),
            };
            let turn = self.live.open(&scope, turn_id).unwrap();
            sink.emit(TurnEvent {
                turn_id: Some(turn_id),
                kind: TurnEventKind::ProviderStarted { request: 1 },
            })
            .await
            .unwrap();
            sink.emit(TurnEvent {
                turn_id: Some(turn_id),
                kind: TurnEventKind::ToolBatchStarted {
                    calls: vec![
                        ToolCall {
                            id: "fast-read".into(),
                            name: "read_file".into(),
                            arguments: json!({}),
                        },
                        ToolCall {
                            id: "long-command".into(),
                            name: "run_command".into(),
                            arguments: json!({"program":"local-fixture"}),
                        },
                    ],
                },
            })
            .await
            .unwrap();
            // 真正独立的长子进程，直到验收步骤释放；输入排队和交流期间始终只有这一份。
            let mut child=tokio::process::Command::new("node").arg("-e")
            .arg("const fs=require('node:fs');setInterval(()=>{if(fs.existsSync(process.argv[1]))process.exit(0)},10)")
            .arg(self.directory.join("release-tool")).kill_on_drop(true).spawn().unwrap();
            self.observed.tool_running.store(true, Ordering::SeqCst);
            // 替换执行器可先回快工具；长子进程仍在等，不能因此撤销交流机会。
            sink.emit(TurnEvent {
                turn_id: Some(turn_id),
                kind: TurnEventKind::ToolResult {
                    ordinal: 0,
                    result: eve_llm_api::ToolResult::success("fast-read", json!({"read":true}))
                        .unwrap(),
                },
            })
            .await
            .unwrap();
            std::fs::write(self.directory.join("tools-ready"), b"ready").unwrap();
            let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(status.success());
            self.observed.tool_running.store(false, Ordering::SeqCst);
            sink.emit(TurnEvent {
                turn_id: Some(turn_id),
                kind: TurnEventKind::ToolProgress {
                    completed_calls: 2,
                    failed_calls: 0,
                    tool_names: vec!["run_command".into()],
                },
            })
            .await
            .unwrap();
            let batch = turn.take_batch(false).unwrap();
            assert_eq!(batch.review_anchor.as_deref(), Some("later-input"));
            *self.observed.consumed.lock().unwrap() = batch.messages.clone();
            sink.emit(TurnEvent {
                turn_id: Some(turn_id),
                kind: TurnEventKind::ProviderStarted { request: 2 },
            })
            .await
            .unwrap();
            assert!(turn.take(true).unwrap().is_empty());
            let mut transcript = vec![ChatMessage::text(ChatRole::User, &input.text)];
            transcript.extend(
                batch
                    .messages
                    .into_iter()
                    .map(|text| ChatMessage::text(ChatRole::User, text)),
            );
            transcript.push(ChatMessage::text(
                ChatRole::Assistant,
                "内部结束不得自动发出",
            ));
            self.sessions
                .complete(&started.lease, transcript.clone())
                .unwrap();
            self.observed.completed.store(true, Ordering::SeqCst);
            RunReport {
                turn_id: Some(turn_id),
                commit: CommitState::Completed,
                text: Some("内部结束不得自动发出".into()),
                transcript: Some(transcript),
                started_tools: Some(2),
                tool_results: vec![],
                failure: None,
            }
        })
    }
}

struct Speech {
    archive: Arc<Archive>,
    observed: Arc<Observed>,
    hold: bool,
    events: Mutex<BTreeMap<String, QqSpeechEvent>>,
    decisions: AtomicUsize,
    deliveries: Mutex<Vec<QqOutreachResult>>,
    retired: AtomicUsize,
}
impl QqSpeech for Speech {
    fn enqueue(&self, event: QqSpeechEvent) -> PluginResult<()> {
        self.events.lock().unwrap().insert(event.id.clone(), event);
        Ok(())
    }
    fn pending(&self) -> PluginResult<Vec<QqSpeechEvent>> {
        Ok(self.events.lock().unwrap().values().cloned().collect())
    }
    fn postpone(&self, _: &QqSpeechEvent, _: u64) -> PluginResult<()> {
        Ok(())
    }
    fn retire(&self, event: &QqSpeechEvent) -> PluginResult<()> {
        self.events.lock().unwrap().remove(&event.id);
        self.retired.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn silenced(&self, _: &QqSpeechEvent) -> PluginResult<bool> {
        Ok(false)
    }
    fn consider(&self, event: QqSpeechEvent) -> LlmFuture<'_, Option<QqSpeechDecision>> {
        Box::pin(async move {
            assert_eq!(event.facts["kind"], "foreground_input_pending");
            assert_eq!(event.facts["phase"], "tools");
            assert_eq!(event.anchor, "later-input");
            let latest = self.archive.0.lock().unwrap().last().unwrap().clone();
            assert_eq!(latest.session_key().unwrap(), event.session);
            assert_eq!(
                latest.text.as_deref(),
                Some("你还在吗？先和我说说现在的情况")
            );
            assert!(self.observed.tool_running.load(Ordering::SeqCst));
            assert!(self.observed.consumed.lock().unwrap().is_empty());
            self.decisions.fetch_add(1, Ordering::SeqCst);
            if self.hold {
                std::future::pending::<()>().await;
            }
            Ok(Some(QqSpeechDecision::Speak {
                text: "我在，刚才的命令还没返回；我看见你的新消息了".into(),
                after_ms: 0,
            }))
        })
    }
    fn dispatched(&self, event: &QqSpeechEvent) -> PluginResult<()> {
        self.events.lock().unwrap().remove(&event.id);
        Ok(())
    }
    fn delivered(&self, _: &QqSpeechEvent, result: QqOutreachResult) -> PluginResult<()> {
        self.deliveries.lock().unwrap().push(result);
        Ok(())
    }
    fn record_expression(
        &self,
        _: QqSpeechEvent,
        _: &str,
        _: QqOutreachResult,
    ) -> PluginResult<()> {
        panic!("夹具主轮没有显式发送")
    }
}

async fn scenario(hold: bool) {
    let directory = tempfile::tempdir().unwrap();
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
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
    let live = Arc::new(LiveDialogue::default());
    let observed = Arc::new(Observed::default());
    let archive = Arc::new(Archive::default());
    let speech = Arc::new(Speech {
        archive: archive.clone(),
        observed: observed.clone(),
        hold,
        events: Mutex::default(),
        decisions: AtomicUsize::new(0),
        deliveries: Mutex::default(),
        retired: AtomicUsize::new(0),
    });
    kernel
        .register(Box::new(
            ControlPlugin::new(
                Arc::new(Runner {
                    sessions,
                    live: live.clone(),
                    directory: directory.path().into(),
                    observed: observed.clone(),
                }),
                vec![],
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
            .join("../../connectors/qqbot/test/live-foreground-bridge.mjs"),
        bridge_args: vec![directory.path().into()],
        app_id: "app".into(),
        app_secret: "local-fixture".into(),
        sandbox: true,
        markdown: false,
    })
    .unwrap()
    .with_receipt_archive(archive)
    .with_live_dialogue(live)
    .with_speech(speech.clone());
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
        .unwrap();
    let reviewed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ready = if hold {
                speech.decisions.load(Ordering::SeqCst) > 0
            } else {
                !speech.deliveries.lock().unwrap().is_empty()
            };
            if ready {
                break;
            }
            assert!(!status.status.borrow().closed, "bridge 提前结束");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let still_running = observed.tool_running.load(Ordering::SeqCst);
    std::fs::write(directory.path().join("release-tool"), b"release").unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if observed.completed.load(Ordering::SeqCst) && status.status.borrow().completed == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // 让工具终态和窗口撤销穿过真实进程，确认没有迟到正文。
    tokio::time::sleep(Duration::from_millis(150)).await;
    status.request_stop();
    kernel.stop_all().await.unwrap();
    reviewed.expect("长工具期间没有交流机会");
    completed.expect("主轮没有消费原输入并完成");
    assert!(still_running, "交流前工具应仍在执行");
    assert_eq!(observed.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        *observed.consumed.lock().unwrap(),
        ["你还在吗？先和我说说现在的情况"]
    );
    assert_eq!(speech.decisions.load(Ordering::SeqCst), 1);
    let rows: Vec<Value> = std::fs::read_to_string(directory.path().join("bridge.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!rows.iter().any(|row| row["type"] == "push"));
    assert!(rows.iter().any(|row| row["type"] == "close_foreground"));
    let sent: Vec<_> = rows
        .iter()
        .filter(|row| row["type"] == "platform")
        .collect();
    if hold {
        assert!(sent.is_empty());
        assert!(speech.deliveries.lock().unwrap().is_empty());
        assert!(speech.retired.load(Ordering::SeqCst) > 0);
    } else {
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["target"]["msgId"], "later-input");
        assert_eq!(
            sent[0]["text"],
            "我在，刚才的命令还没返回；我看见你的新消息了"
        );
        assert_eq!(
            *speech.deliveries.lock().unwrap(),
            [QqOutreachResult::Sent {
                platform_message_id: Some("platform-1".into())
            }]
        );
    }
}

#[tokio::test]
async fn new_input_can_be_answered_while_the_original_tool_runs_and_is_consumed_once() {
    scenario(false).await;
}

#[tokio::test]
async fn tool_completion_and_consumed_input_retire_an_inflight_speech_decision() {
    scenario(true).await;
}
