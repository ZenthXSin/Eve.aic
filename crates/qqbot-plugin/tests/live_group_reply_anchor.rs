//! 真实 Rust 通道和 Node 桥复现旧群消息过期；外部模型、QQ 与时钟均为本地替身。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, ToolCall, ToolExecutionContext, TurnEvent, TurnEventKind,
    TurnEventSink, TurnSteeringSource,
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
    time::Duration,
};

struct Runner {
    sessions: Arc<dyn SessionService>,
    live: Arc<LiveDialogue>,
    directory: PathBuf,
    runs: Arc<AtomicUsize>,
}
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("群聊回复窗口夹具没有到达预期状态");
}
fn rows(directory: &Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("bridge.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
impl Runner {
    async fn say(&self, scope: &ContextScope, turn_id: u64, id: &str, text: &str) -> Value {
        let tool = self.live.send_tool();
        tokio::time::timeout(
            Duration::from_secs(5),
            tool.execute(
                ToolCall {
                    id: id.into(),
                    name: "send_message".into(),
                    arguments: json!({"text":text}),
                },
                ToolExecutionContext::new()
                    .with_scope(Some(scope.clone()))
                    .with_turn_id(Some(turn_id)),
            ),
        )
        .await
        .unwrap()
        .unwrap()
    }
}
impl ControlRunner for Runner {
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        sink: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        metadata
            .expect("可信群消息元数据")
            .validate_scope(&input.key)
            .unwrap();
        self.run(input, sink)
    }
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let started = self.sessions.begin(input.clone()).unwrap();
            let id = started.lease.turn_id;
            let scope = ContextScope {
                session_id: input.key.session_id.clone(),
                user_id: input.key.user_id.clone(),
            };
            let turn = self.live.open(&scope, id).unwrap();
            sink.emit(TurnEvent {
                turn_id: Some(id),
                kind: TurnEventKind::ProviderStarted { request: 1 },
            })
            .await
            .unwrap();
            std::fs::write(self.directory.join("expire-original"), b"expire").unwrap();
            wait_until(|| self.directory.join("original-expired").exists()).await;
            let old = self
                .say(&scope, id, "old-attempt", "实际核对后的结果")
                .await;
            assert_eq!(old["delivery_status"], "not_sent");
            assert_eq!(old["failure_kind"], "passive_window_elapsed");
            assert!(
                rows(&self.directory)
                    .iter()
                    .all(|r| r["type"] != "platform")
            );
            std::fs::write(self.directory.join("accept-followup"), b"new-input").unwrap();
            let mut consumed = Vec::new();
            wait_until(|| {
                consumed.extend(turn.take(false).unwrap());
                !consumed.is_empty()
            })
            .await;
            assert_eq!(consumed, ["还在吗，继续告诉我结果"]);
            // 旧窗口在 SDK 前明确拒绝；模型消费新输入后可以新调用重选同正文。
            for (number, text) in ["实际核对后的结果", "补充第二条发现", "补充第三条发现"]
                .iter()
                .enumerate()
            {
                let sent = self.say(&scope, id, &format!("fresh-{number}"), text).await;
                assert_eq!(sent["sent"], true);
                assert_eq!(sent["message_id"], format!("group-platform-{}", number + 1));
            }
            let full = self
                .say(&scope, id, "fresh-over-quota", "不能越过新窗口额度")
                .await;
            assert_eq!(full["delivery_status"], "not_sent");
            assert_eq!(full["failure_kind"], "passive_quota_exhausted");
            turn.take(true).unwrap();
            let transcript = vec![
                ChatMessage::text(ChatRole::User, input.text),
                ChatMessage::text(ChatRole::User, consumed.remove(0)),
                ChatMessage::text(ChatRole::Assistant, "内部结束"),
            ];
            self.sessions
                .complete(&started.lease, transcript.clone())
                .unwrap();
            sink.emit(TurnEvent {
                turn_id: Some(id),
                kind: TurnEventKind::SessionSaved,
            })
            .await
            .unwrap();
            RunReport {
                turn_id: Some(id),
                commit: CommitState::Completed,
                text: Some("内部结束".into()),
                transcript: Some(transcript),
                started_tools: Some(5),
                tool_results: vec![],
                failure: None,
            }
        })
    }
}

#[tokio::test]
async fn new_group_input_keeps_its_window_with_parent_receipts_and_readable_restart() {
    let directory = tempfile::tempdir().unwrap();
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let state = services.state.clone();
    let kernel = Kernel::with_services(services);
    let qq_id = PluginId::new(QQBOT_PLUGIN_ID).unwrap();
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
    kernel
        .register(Box::new(
            ControlPlugin::new(
                Arc::new(Runner {
                    sessions,
                    live: live.clone(),
                    directory: directory.path().into(),
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
    kernel
        .register(Box::new(
            QqBotPlugin::new(QqBotConfig {
                node_program: "node".into(),
                bridge_script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../connectors/qqbot/test/group-live-anchor-bridge.mjs"),
                bridge_args: vec![directory.path().into()],
                app_id: "app".into(),
                app_secret: "local-fixture".into(),
                sandbox: true,
                markdown: false,
            })
            .unwrap()
            .with_live_dialogue(live),
        ))
        .unwrap();
    kernel.start(&qq_id).await.unwrap();
    let completed =
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let Some(bytes) = state.get(&qq_id, "receipts.v1").unwrap() else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let ledger: Value = serde_json::from_slice(&bytes).unwrap();
                if ledger["entries"].as_array().unwrap().iter().any(|r| {
                    r["message"]["id"] == "original-group-input" && r["state"] == "Handled"
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    kernel.stop(&qq_id).await.unwrap();
    completed.expect("原群聊轮次未完成");
    let bytes = state.get(&qq_id, "receipts.v1").unwrap().unwrap();
    validate_receipt_state(&bytes).unwrap();
    let ledger: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(ledger["version"], 10);
    let entries = ledger["entries"].as_array().unwrap();
    let parent = entries
        .iter()
        .find(|r| r["message"]["id"] == "original-group-input")
        .unwrap();
    let child = entries
        .iter()
        .find(|r| r["message"]["id"] == "fresh-group-input")
        .unwrap();
    assert_eq!(child["state"], "Merged");
    assert_eq!(child["merged_into"], "original-group-input");
    let progress = parent["progress"].as_array().unwrap();
    assert_eq!(progress.len(), 4);
    assert_eq!(progress[0]["state"], "Failed");
    assert!(progress[0].get("reply_anchor").is_none());
    for p in &progress[1..] {
        assert_eq!(p["state"], "Sent");
        assert_eq!(p["reply_anchor"], "fresh-group-input");
        assert!(p["platform_message_id"].as_str().is_some());
        assert_ne!(p["proactive"], true);
    }
    let records = rows(directory.path());
    let sends: Vec<_> = records.iter().filter(|r| r["type"] == "platform").collect();
    assert_eq!(sends.len(), 3);
    assert!(sends.iter().all(|r| r["target"]["scope"] == "group"
        && r["target"]["targetId"] == "group-one"
        && r["target"]["msgId"] == "fresh-group-input"));
    assert!(records.iter().any(|r| r["type"] == "finish"
        && r["id"] == "fresh-group-input"
        && r["keep_live_reply"] == true
        && r["parent_id"] == "original-group-input"));
    // 实际重开同一插件与 Node 进程，不能仅靠 serde 形状检查宣称可恢复。
    kernel.start(&qq_id).await.unwrap();
    wait_until(|| directory.path().join("reopened").exists()).await;
    kernel.stop_all().await.unwrap();
    assert_eq!(state.get(&qq_id, "receipts.v1").unwrap().unwrap(), bytes);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        rows(directory.path())
            .iter()
            .filter(|r| r["type"] == "platform")
            .count(),
        3
    );
}
