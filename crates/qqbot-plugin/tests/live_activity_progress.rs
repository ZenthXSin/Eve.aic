//! 真实 Rust 通道和 Node bridge-core；替换运行器/QQ出口，不配置任何进度模型。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmFuture, ToolCall, ToolExecutionContext, TurnEvent,
    TurnEventKind, TurnEventSink, TurnSteeringSource,
};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginId, ServiceId};
use eve_qqbot_plugin::*;
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionInput, SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};

struct Policy;
impl QqSlowProgress for Policy {
    fn after(&self) -> Option<Duration> {
        Some(Duration::from_millis(80))
    }
    fn min_interval(&self) -> Duration {
        Duration::from_millis(100)
    }
}
struct NoNarrator(Arc<AtomicUsize>);
impl QqProgressNarrator for NoNarrator {
    fn narrate(&self, _: QqProgressFacts) -> LlmFuture<'_, Option<String>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("系统活动不应请求进度模型")
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Slow,
    Fast,
    Quiet,
    Listen,
    DelayedReceipt,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Fast => "fast",
            Self::Quiet => "quiet",
            Self::Listen => "listen",
            Self::DelayedReceipt => "delayed",
        }
    }
}
struct Runner {
    sessions: Arc<dyn SessionService>,
    live: Arc<LiveDialogue>,
    directory: PathBuf,
    mode: Mode,
    runs: Arc<AtomicUsize>,
}
async fn emit(sink: &dyn TurnEventSink, turn: u64, kind: TurnEventKind) {
    sink.emit(TurnEvent {
        turn_id: Some(turn),
        kind,
    })
    .await
    .unwrap();
}
fn rows(directory: &Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("bridge.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("本地通道没有达到预期状态");
}
impl ControlRunner for Runner {
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        sink: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        metadata
            .expect("QQ输入元数据")
            .validate_scope(&input.key)
            .unwrap();
        self.run(input, sink)
    }
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let started = self.sessions.begin(input.clone()).unwrap();
            let turn_id = started.lease.turn_id;
            let scope = ContextScope {
                session_id: input.key.session_id.clone(),
                user_id: input.key.user_id.clone(),
            };
            let turn = self.live.open(&scope, turn_id).unwrap();
            emit(sink, turn_id, TurnEventKind::ProviderStarted { request: 1 }).await;
            if matches!(self.mode, Mode::Slow) {
                wait_until(|| {
                    rows(&self.directory).iter().any(|r| {
                        r["type"] == "delivery" && r["progress"] == true && r["ok"] == true
                    })
                })
                .await;
                // 同一等待阶段持续很久也不反复播报。
                tokio::time::sleep(Duration::from_millis(250)).await;
                assert_eq!(
                    rows(&self.directory)
                        .iter()
                        .filter(|r| r["type"] == "platform")
                        .count(),
                    1
                );
            } else if matches!(self.mode, Mode::Quiet) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            if matches!(self.mode, Mode::Slow | Mode::DelayedReceipt | Mode::Listen) {
                let name = if matches!(self.mode, Mode::Listen) {
                    "wait_for_input"
                } else {
                    "run_command"
                };
                emit(
                    sink,
                    turn_id,
                    TurnEventKind::ToolBatchStarted {
                        calls: vec![ToolCall {
                            id: "tool".into(),
                            name: name.into(),
                            arguments: json!({"private":"不能出现在进度里"}),
                        }],
                    },
                )
                .await;
                if matches!(self.mode, Mode::Slow) {
                    wait_until(|| {
                        rows(&self.directory)
                            .iter()
                            .filter(|r| {
                                r["type"] == "delivery" && r["progress"] == true && r["ok"] == true
                            })
                            .count()
                            == 2
                    })
                    .await;
                } else if matches!(self.mode, Mode::DelayedReceipt) {
                    wait_until(|| self.directory.join("host-written").exists()).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                emit(
                    sink,
                    turn_id,
                    TurnEventKind::ToolResult {
                        ordinal: 0,
                        result: eve_llm_api::ToolResult::success("tool", json!({"completed":true}))
                            .unwrap(),
                    },
                )
                .await;
            }
            if matches!(self.mode, Mode::Slow | Mode::DelayedReceipt) {
                // 此显式发送在自动进度回执返回前排队，不能被那份回执误结算。
                let tool = self.live.send_tool();
                let sending = tool.execute(
                    ToolCall {
                        id: "actual-answer".into(),
                        name: "send_message".into(),
                        arguments: json!({"text":"Eve核对后的正式结果","purpose":"answer"}),
                    },
                    ToolExecutionContext::new()
                        .with_scope(Some(scope))
                        .with_turn_id(Some(turn_id)),
                );
                tokio::pin!(sending);
                if matches!(self.mode, Mode::DelayedReceipt) {
                    // 首次 poll 已将明确发言放入 LiveDialogue 队列，随后等待平台回执。
                    std::future::poll_fn(|cx| match sending.as_mut().poll(cx) {
                        Poll::Pending => Poll::Ready(()),
                        Poll::Ready(result) => panic!("平台尚未回复就提前结束发送: {result:?}"),
                    })
                    .await;
                    tokio::time::sleep(Duration::from_millis(160)).await;
                    std::fs::write(self.directory.join("explicit-queued"), b"queued").unwrap();
                }
                let sent = tokio::time::timeout(Duration::from_secs(5), sending)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(sent["sent"], true);
                assert_eq!(
                    sent["message_id"],
                    if matches!(self.mode, Mode::Slow) {
                        "platform-3"
                    } else {
                        "platform-2"
                    }
                );
            }
            turn.take(true).unwrap();
            let transcript = vec![
                ChatMessage::text(ChatRole::User, input.text),
                ChatMessage::text(ChatRole::Assistant, "内部结束，不能自动发送"),
            ];
            self.sessions
                .complete(&started.lease, transcript.clone())
                .unwrap();
            emit(sink, turn_id, TurnEventKind::SessionSaved).await;
            RunReport {
                turn_id: Some(turn_id),
                commit: CommitState::Completed,
                text: Some("内部结束，不能自动发送".into()),
                transcript: Some(transcript),
                started_tools: Some(match self.mode {
                    Mode::Slow | Mode::DelayedReceipt => 2,
                    Mode::Listen => 1,
                    _ => 0,
                }),
                tool_results: vec![],
                failure: None,
            }
        })
    }
}

async fn scenario(mode: Mode) {
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
    let runs = Arc::new(AtomicUsize::new(0));
    let narrations = Arc::new(AtomicUsize::new(0));
    kernel
        .register(Box::new(
            ControlPlugin::new(
                Arc::new(Runner {
                    sessions,
                    live: live.clone(),
                    directory: directory.path().into(),
                    mode,
                    runs: runs.clone(),
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
            .join("../../connectors/qqbot/test/live-activity-bridge.mjs"),
        bridge_args: vec![directory.path().into(), mode.name().into()],
        app_id: "app".into(),
        app_secret: "local-fixture".into(),
        sandbox: true,
        markdown: false,
    })
    .unwrap()
    .with_live_dialogue(live)
    .with_slow_progress(Arc::new(Policy))
    .with_progress_narrator(Arc::new(NoNarrator(narrations.clone())));
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
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        while status.status.borrow().completed != 1 {
            assert!(!status.status.borrow().closed, "通道提前结束");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    status.request_stop();
    kernel.stop_all().await.unwrap();
    completed.expect("真实通道没有完成本轮");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(narrations.load(Ordering::SeqCst), 0);
    let rows = rows(directory.path());
    let sent: Vec<_> = rows.iter().filter(|r| r["type"] == "platform").collect();
    let count = match mode {
        Mode::Slow => 3,
        Mode::DelayedReceipt => 2,
        _ => 0,
    };
    assert_eq!(sent.len(), count);
    assert!(sent.iter().all(|r| r["target"]["msgId"] == "initial-input"));
    assert!(
        !sent
            .iter()
            .any(|r| r["text"].as_str().unwrap().contains("内部结束"))
    );
    assert!(
        !sent
            .iter()
            .any(|r| r["text"].as_str().unwrap().contains("不能出现在"))
    );
    assert!(
        !rows
            .iter()
            .any(|r| r["type"] == "push" || r["proactive"] == true)
    );
    if matches!(mode, Mode::Slow) {
        assert!(sent[0]["text"].as_str().unwrap().contains("等待回复生成"));
        assert!(sent[1]["text"].as_str().unwrap().contains("命令执行"));
    }
}

#[tokio::test]
async fn live_reports_slow_model_and_tools_without_another_model_and_keeps_the_real_answer() {
    scenario(Mode::Slow).await;
}
#[tokio::test]
async fn system_progress_receipt_does_not_complete_a_queued_explicit_send() {
    scenario(Mode::DelayedReceipt).await;
}
#[tokio::test]
async fn fast_quiet_and_listening_turns_do_not_receive_automatic_progress() {
    for mode in [Mode::Fast, Mode::Quiet, Mode::Listen] {
        scenario(mode).await;
    }
}
