//! 真实 Rust/Node 通道与长运行子进程；只替换模型和外部平台，不向生产 QQ 发消息。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFailure, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, InputReplyDelivery, InputReplyHandoff, LlmError,
    LlmFuture, ToolCall, TurnEvent, TurnEventKind, TurnEventSink, TurnSteeringSource,
};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId};
use eve_qqbot_plugin::*;
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionFailure, SessionFailureCode, SessionInput,
    SessionKey, SessionService, SessionServiceHandle,
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
struct Archive(Mutex<Vec<QqOriginalMessage>>, Option<Arc<ClaimGate>>);

#[derive(Clone, Copy)]
enum AfterClaim {
    Finish,
    Cancel,
    Replace,
}
struct ClaimGate {
    mode: AfterClaim,
    directory: PathBuf,
    claimed: AtomicBool,
    crossed: AtomicBool,
    completed_before_check: AtomicBool,
    changed: std::sync::Condvar,
    outcome: Mutex<(bool, bool)>,
}
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
        if let Some(gate) = &self.1
            && gate.claimed.load(Ordering::SeqCst)
            && !gate.crossed.swap(true, Ordering::SeqCst)
        {
            // 这是同步存档接口；先让出 Tokio 核心，子进程回收和控制任务才能继续。
            // 主轮只能在领取锁释放后消费；最后的原话核对前必须完成指定控制转移。
            let completed = tokio::task::block_in_place(|| {
                std::fs::write(gate.directory.join("release-tool"), b"release").unwrap();
                let outcome = gate.outcome.lock().unwrap();
                let (outcome, waited) = gate
                    .changed
                    .wait_timeout_while(outcome, Duration::from_secs(3), |(ready, _)| !*ready)
                    .unwrap();
                !waited.timed_out() && outcome.0 && outcome.1
            });
            gate.completed_before_check
                .store(completed, Ordering::SeqCst);
            if !completed {
                return Err(PluginError::State("领取后的确定性收尾闸门失败".into()));
            }
        }
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
    cancelled: AtomicBool,
    decision_dropped: AtomicBool,
    retired_before_drop: AtomicBool,
    consumed: Mutex<Vec<String>>,
    input_replies: Mutex<Vec<InputReplyHandoff>>,
    terminal_replies: Mutex<Vec<InputReplyHandoff>>,
}
struct Runner {
    sessions: Arc<dyn SessionService>,
    live: Arc<LiveDialogue>,
    directory: PathBuf,
    observed: Arc<Observed>,
    delayed_receipt: bool,
}
impl ControlRunner for Runner {
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        sink: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        if input.text == "新的控制代" {
            assert!(metadata.is_none());
            return self.run(input, sink);
        }
        let metadata = metadata.expect("真实入站元数据");
        metadata.validate_scope(&input.key).unwrap();
        assert!(
            matches!(metadata.review,eve_control_api::ControlReviewEvidence::ChannelAnchor(anchor) if anchor=="initial-input")
        );
        self.run(input, sink)
    }
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            if input.text == "新的控制代" {
                sink.closed().await.unwrap();
                return RunReport {
                    turn_id: None,
                    commit: CommitState::NotStarted,
                    text: None,
                    transcript: None,
                    started_tools: Some(0),
                    tool_results: vec![],
                    failure: None,
                };
            }
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
            let status = tokio::select! {
                _ = sink.closed() => None,
                status = tokio::time::timeout(Duration::from_secs(15), child.wait()) => {
                    Some(status.unwrap().unwrap())
                }
            };
            let Some(status) = status else {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
                self.observed.tool_running.store(false, Ordering::SeqCst);
                self.sessions
                    .fail(
                        &started.lease,
                        SessionFailure {
                            evidence: None,
                            code: SessionFailureCode::Cancelled,
                            started_tools: Some(2),
                        },
                    )
                    .unwrap();
                self.observed.cancelled.store(true, Ordering::SeqCst);
                return RunReport {
                    turn_id: Some(turn_id),
                    commit: CommitState::Failed,
                    text: None,
                    transcript: None,
                    started_tools: Some(2),
                    tool_results: vec![],
                    failure: Some(RunFailure::Execution(LlmError::Cancelled)),
                };
            };
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
            let update = turn.take_update(false).unwrap();
            *self.observed.input_replies.lock().unwrap() = update.input_replies;
            let batch = update.batch;
            assert_eq!(batch.review_anchor.as_deref(), Some("later-input"));
            *self.observed.consumed.lock().unwrap() = batch.messages.clone();
            sink.emit(TurnEvent {
                turn_id: Some(turn_id),
                kind: TurnEventKind::ProviderStarted { request: 2 },
            })
            .await
            .unwrap();
            if self.delayed_receipt {
                // 模拟原业务的下一项异步工作；回执返回并不负责唤醒这一步。
                std::fs::write(self.directory.join("input-consumed"), b"ready").unwrap();
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !self.directory.join("resume-next-step").exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                let next = turn.take_update(false).unwrap();
                assert!(next.batch.messages.is_empty());
                assert!(next.batch.review_anchor.is_none());
                *self.observed.terminal_replies.lock().unwrap() = next.input_replies;
            }
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
    retire_fails: bool,
    events: Mutex<BTreeMap<String, QqSpeechEvent>>,
    decisions: AtomicUsize,
    deliveries: Mutex<Vec<QqOutreachResult>>,
    retired: AtomicUsize,
}
struct DecisionWait<'a>(&'a Speech);
impl Drop for DecisionWait<'_> {
    fn drop(&mut self) {
        self.0
            .observed
            .retired_before_drop
            .store(self.0.retired.load(Ordering::SeqCst) > 0, Ordering::SeqCst);
        self.0
            .observed
            .decision_dropped
            .store(true, Ordering::SeqCst);
    }
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
        self.retired.fetch_add(1, Ordering::SeqCst);
        if self.retire_fails {
            return Err(PluginError::State("夹具撤销失败".into()));
        }
        self.events.lock().unwrap().remove(&event.id);
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
                let _wait = DecisionWait(self);
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
        if let Some(gate) = &self.archive.1 {
            gate.claimed.store(true, Ordering::SeqCst);
        }
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

enum Scenario {
    Speak,
    DelayedReceipt,
    FinishAfterClaim,
    CancelAfterClaim,
    ReplaceAfterClaim,
    ToolCompletesDuringDecision,
    StopDuringDecision,
    StopWithRetireFailure,
}

async fn scenario(mode: Scenario) {
    let hold = matches!(
        mode,
        Scenario::ToolCompletesDuringDecision
            | Scenario::StopDuringDecision
            | Scenario::StopWithRetireFailure
    );
    let delayed_receipt = matches!(mode, Scenario::DelayedReceipt);
    let stopping = matches!(
        mode,
        Scenario::StopDuringDecision | Scenario::StopWithRetireFailure
    );
    let retire_fails = matches!(mode, Scenario::StopWithRetireFailure);
    let directory = tempfile::tempdir().unwrap();
    if delayed_receipt {
        std::fs::write(directory.path().join("delay-receipt"), b"delay").unwrap();
    }
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
    let after_claim = match mode {
        Scenario::FinishAfterClaim => Some(AfterClaim::Finish),
        Scenario::CancelAfterClaim => Some(AfterClaim::Cancel),
        Scenario::ReplaceAfterClaim => Some(AfterClaim::Replace),
        _ => None,
    };
    let gate = after_claim.map(|mode| {
        Arc::new(ClaimGate {
            mode,
            directory: directory.path().into(),
            claimed: AtomicBool::new(false),
            crossed: AtomicBool::new(false),
            completed_before_check: AtomicBool::new(false),
            changed: std::sync::Condvar::new(),
            outcome: Mutex::new((false, false)),
        })
    });
    let archive = Arc::new(Archive(Mutex::new(vec![]), gate.clone()));
    let speech = Arc::new(Speech {
        archive: archive.clone(),
        observed: observed.clone(),
        hold,
        retire_fails,
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
                    delayed_receipt,
                }),
                vec![],
            )
            .unwrap(),
        ))
        .unwrap();
    kernel
        .start(&PluginId::new(eve_control_api::CONTROL_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let control = registry
        .get(&ServiceId::new(eve_control_api::CONTROL_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<eve_control_api::ControlServiceHandle>()
        .unwrap()
        .0
        .clone();
    let race = gate.clone().map(|gate| {
        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                let state = loop {
                    if gate.crossed.load(Ordering::SeqCst) {
                        let sessions = control.list_keys(None, 10).unwrap();
                        if let Some(session) = sessions.first()
                            && let Some(state) = control.snapshot(session).unwrap()
                            && state.phase == eve_control_api::ControlPhase::Finished
                        {
                            break state;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                };
                assert!(!state.cancel_requested);
                assert!(!state.events_retired);
                assert_eq!(
                    state.report.as_ref().unwrap().run.commit,
                    CommitState::Completed
                );
                match gate.mode {
                    AfterClaim::Finish => {}
                    AfterClaim::Cancel => {
                        assert_eq!(
                            control.cancel(&state.key).unwrap(),
                            eve_control_api::CancelDisposition::AlreadyFinished
                        );
                        assert!(
                            control
                                .snapshot(&state.key.session)
                                .unwrap()
                                .unwrap()
                                .events_retired
                        );
                    }
                    AfterClaim::Replace => {
                        let next = control
                            .submit_if_current(
                                &state.key,
                                eve_control_api::ControlInput {
                                    session: SessionInput {
                                        key: state.key.session.clone(),
                                        text: "新的控制代".into(),
                                    },
                                    task_id: "replacement".into(),
                                },
                                Arc::new(eve_control_api::DiscardControlEvents),
                            )
                            .unwrap();
                        assert_ne!(next, state.key);
                    }
                }
            })
            .await;
            *gate.outcome.lock().unwrap() = (true, result.is_ok());
            gate.changed.notify_all();
            result.expect("原轮未在领取后完成");
        })
    });
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
            } else if delayed_receipt {
                directory.path().join("platform-started").exists()
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
    if stopping {
        status.request_stop();
        let stopped = tokio::time::timeout(Duration::from_secs(3), kernel.stop_all())
            .await
            .expect("停止必须回收长工具和未完成的交流决定");
        reviewed.expect("停止前没有进入交流决定");
        assert_eq!(stopped.is_err(), retire_fails);
        assert_eq!(status.status.borrow().terminal_error, retire_fails);
        assert!(still_running);
        assert!(observed.cancelled.load(Ordering::SeqCst));
        assert!(!observed.tool_running.load(Ordering::SeqCst));
        assert!(!observed.completed.load(Ordering::SeqCst));
        assert!(observed.consumed.lock().unwrap().is_empty());
        assert!(observed.decision_dropped.load(Ordering::SeqCst));
        assert!(
            observed.retired_before_drop.load(Ordering::SeqCst),
            "丢弃前台决定前必须先尝试撤销事实"
        );
        assert_eq!(speech.decisions.load(Ordering::SeqCst), 1);
        assert_eq!(speech.retired.load(Ordering::SeqCst), 1);
        assert!(speech.deliveries.lock().unwrap().is_empty());
        assert_eq!(
            speech.events.lock().unwrap().len(),
            usize::from(retire_fails)
        );
        let rows: Vec<Value> = std::fs::read_to_string(directory.path().join("bridge.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let closed = rows
            .iter()
            .position(|row| row["type"] == "close_foreground")
            .unwrap();
        let stopped = rows.iter().position(|row| row["type"] == "stop").unwrap();
        assert!(closed < stopped, "先撤销窗口再停止 Node");
        assert!(!rows.iter().any(|row| row["type"] == "platform"));
        return;
    }
    std::fs::write(directory.path().join("release-tool"), b"release").unwrap();
    if delayed_receipt {
        reviewed.as_ref().expect("平台发送没有开始");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !directory.path().join("input-consumed").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("主轮未消费输入");
        {
            let handoffs = observed.input_replies.lock().unwrap();
            assert_eq!(handoffs.len(), 1);
            assert_eq!(handoffs[0].delivery, InputReplyDelivery::Sending);
        }
        // 等旧前台窗口真实关闭，终态仍必须能找到原轮，而不是借最新前台输入归属。
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let rows = std::fs::read_to_string(directory.path().join("bridge.jsonl")).unwrap();
                if rows
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .any(|row| row["type"] == "close_foreground")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("已消费输入的前台窗口未关闭");
        std::fs::write(directory.path().join("release-receipt"), b"release").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while speech.deliveries.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("平台终态未返回");
        std::fs::write(directory.path().join("resume-next-step"), b"continue").unwrap();
    }
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
    if after_claim.is_none() {
        assert!(still_running, "交流前工具应仍在执行");
    }
    if let Some(race) = race {
        race.await.unwrap();
    }
    if let Some(gate) = &gate {
        assert!(
            gate.completed_before_check.load(Ordering::SeqCst),
            "闸门必须在桥最终核对前完成，超时不能伪装成正常撤销"
        );
    }
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
    if matches!(after_claim, Some(AfterClaim::Cancel | AfterClaim::Replace)) {
        assert!(sent.is_empty(), "领取后真实取消或换代不得发送");
        assert_eq!(
            *speech.deliveries.lock().unwrap(),
            [QqOutreachResult::NotSent]
        );
        assert_eq!(
            observed.input_replies.lock().unwrap()[0].delivery,
            InputReplyDelivery::Sending
        );
    } else if hold {
        assert!(sent.is_empty());
        assert!(speech.deliveries.lock().unwrap().is_empty());
        assert!(speech.retired.load(Ordering::SeqCst) > 0);
        assert!(observed.decision_dropped.load(Ordering::SeqCst));
        assert!(observed.retired_before_drop.load(Ordering::SeqCst));
        assert!(observed.input_replies.lock().unwrap().is_empty());
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
        let handoffs = observed.input_replies.lock().unwrap();
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].input_message_id, "later-input");
        assert_eq!(handoffs[0].input_text, "你还在吗？先和我说说现在的情况");
        assert_eq!(
            Some(handoffs[0].reply_text.as_str()),
            sent[0]["text"].as_str()
        );
        assert_eq!(
            handoffs[0].delivery,
            if delayed_receipt || after_claim.is_some() {
                InputReplyDelivery::Sending
            } else {
                InputReplyDelivery::Sent {
                    platform_message_id: "platform-1".into(),
                }
            }
        );
        let terminal = observed.terminal_replies.lock().unwrap();
        if delayed_receipt {
            assert_eq!(terminal.len(), 1);
            assert_eq!(terminal[0].input_message_id, handoffs[0].input_message_id);
            assert_eq!(terminal[0].event_id, handoffs[0].event_id);
            assert_eq!(
                terminal[0].delivery,
                InputReplyDelivery::Sent {
                    platform_message_id: "platform-1".into()
                }
            );
        } else {
            assert!(terminal.is_empty());
        }
    }
}

#[tokio::test]
async fn new_input_can_be_answered_while_the_original_tool_runs_and_is_consumed_once() {
    scenario(Scenario::Speak).await;
}

#[tokio::test]
async fn receipt_after_foreground_removal_updates_only_the_original_live_turn() {
    scenario(Scenario::DelayedReceipt).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claimed_input_reply_still_sends_once_after_original_turn_finishes() {
    scenario(Scenario::FinishAfterClaim).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_finished_generation_after_claim_still_revokes_the_send() {
    scenario(Scenario::CancelAfterClaim).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacing_finished_generation_after_claim_still_revokes_the_send() {
    scenario(Scenario::ReplaceAfterClaim).await;
}

#[tokio::test]
async fn tool_completion_and_consumed_input_retire_an_inflight_speech_decision() {
    scenario(Scenario::ToolCompletesDuringDecision).await;
}

#[tokio::test]
async fn stopping_retires_foreground_before_dropping_decision_and_reaps_long_tool() {
    scenario(Scenario::StopDuringDecision).await;
}

#[tokio::test]
async fn retirement_failure_still_closes_window_drops_decision_and_reaps_long_tool() {
    scenario(Scenario::StopWithRetireFailure).await;
}
