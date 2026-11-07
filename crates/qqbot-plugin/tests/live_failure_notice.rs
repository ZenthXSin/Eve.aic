//! 真实 Rust 通道和生产 Node bridge-core；失败事实不依赖模型或真实 QQ。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{
    CommitState, ControlReport, ControlRunner, RunFailure, RunFuture, RunReport,
};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{LlmError, LlmFuture, TurnEventSink};
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, StateStore};
use eve_qqbot_plugin::*;
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionFailure, SessionFailureCode, SessionInput,
    SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct FailedRunner(Arc<dyn SessionService>, CommitState);
impl ControlRunner for FailedRunner {
    fn run<'a>(&'a self, _: SessionInput, _: &'a dyn TurnEventSink) -> RunFuture<'a> {
        panic!("QQ 必须传入可信原消息元数据")
    }
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        _: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        let metadata = metadata.expect("可信原消息绑定");
        metadata.validate_scope(&input.key).unwrap();
        assert!(matches!(metadata.review,
            eve_control_api::ControlReviewEvidence::ChannelAnchor(anchor) if anchor == "failed-input"));
        Box::pin(async move {
            let started = self.0.begin(input).unwrap();
            if self.1 == CommitState::Failed {
                self.0
                    .fail(
                        &started.lease,
                        SessionFailure {
                            code: SessionFailureCode::Protocol,
                            started_tools: Some(0),
                            evidence: None,
                        },
                    )
                    .unwrap();
            }
            RunReport {
                turn_id: Some(started.lease.turn_id),
                commit: self.1,
                text: None,
                transcript: None,
                started_tools: Some(0),
                tool_results: vec![],
                failure: Some(RunFailure::Execution(LlmError::Protocol(
                    "Chat 函数参数不是有效 JSON".into(),
                ))),
            }
        })
    }
}

struct EnabledFailureNotices;
impl QqSlowProgress for EnabledFailureNotices {
    fn after(&self) -> Option<Duration> {
        None
    }
    fn failure_notices(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct Recovery(Mutex<Vec<ControlReport>>);
impl QqFailureRecovery for Recovery {
    fn recover(&self, report: &ControlReport) -> PluginResult<Option<String>> {
        self.0.lock().unwrap().push(report.clone());
        Ok(None)
    }
}

#[derive(Default)]
struct UnavailableSpeech(AtomicUsize);
impl QqSpeech for UnavailableSpeech {
    fn enqueue(&self, _: QqSpeechEvent) -> PluginResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(PluginError::Task("夹具发言服务不可用".into()))
    }
    fn pending(&self) -> PluginResult<Vec<QqSpeechEvent>> {
        Ok(vec![])
    }
    fn postpone(&self, _: &QqSpeechEvent, _: u64) -> PluginResult<()> {
        panic!("故障通知不得进入自主发言")
    }
    fn retire(&self, _: &QqSpeechEvent) -> PluginResult<()> {
        panic!("故障通知不得进入自主发言")
    }
    fn silenced(&self, _: &QqSpeechEvent) -> PluginResult<bool> {
        panic!("故障通知不得进入自主发言")
    }
    fn consider(&self, _: QqSpeechEvent) -> LlmFuture<'_, Option<QqSpeechDecision>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(LlmError::ProviderTimeout) })
    }
    fn dispatched(&self, _: &QqSpeechEvent) -> PluginResult<()> {
        panic!("故障通知不得进入自主发言")
    }
    fn delivered(&self, _: &QqSpeechEvent, _: QqOutreachResult) -> PluginResult<()> {
        panic!("故障通知不得进入自主发言")
    }
    fn record_expression(
        &self,
        _: QqSpeechEvent,
        _: &str,
        _: QqOutreachResult,
    ) -> PluginResult<()> {
        panic!("故障通知不得记为 Eve 的自主表达")
    }
}

fn receipts(store: &MemoryStateStore) -> Option<Value> {
    store
        .get(&PluginId::new(QQBOT_PLUGIN_ID).unwrap(), "receipts.v1")
        .unwrap()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
}

async fn scenario(with_speech: bool, unknown: bool, notices: bool, commit: CommitState) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStateStore::default());
    let services = KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    };
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
    kernel
        .register(Box::new(
            ControlPlugin::new(Arc::new(FailedRunner(sessions, commit)), vec![]).unwrap(),
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
    let speech = Arc::new(UnavailableSpeech::default());
    let recovery = Arc::new(Recovery::default());
    let mut plugin = QqBotPlugin::new(QqBotConfig {
        node_program: "node".into(),
        bridge_script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/qqbot/test/live-failure-bridge.mjs"),
        bridge_args: vec![
            directory.path().into(),
            if !notices {
                "silent"
            } else if unknown {
                "unknown"
            } else {
                "sent"
            }
            .into(),
        ],
        app_id: "app".into(),
        app_secret: "local-fixture".into(),
        sandbox: true,
        markdown: false,
    })
    .unwrap()
    .with_live_dialogue(Arc::new(LiveDialogue::default()))
    .with_failure_recovery(recovery.clone());
    if notices {
        plugin = plugin.with_slow_progress(Arc::new(EnabledFailureNotices));
    }
    if with_speech {
        plugin = plugin.with_speech(speech.clone());
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
        .unwrap();
    let expected = if !notices {
        "Failed"
    } else if unknown {
        "Unknown"
    } else {
        "Sent"
    };
    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let input_released = notices
                || std::fs::read_to_string(directory.path().join("bridge.jsonl")).is_ok_and(
                    |rows| {
                        rows.lines()
                            .filter(|line| {
                                serde_json::from_str::<Value>(line)
                                    .is_ok_and(|row| row["type"] == "finished")
                            })
                            .count()
                            == 2
                    },
                );
            if input_released
                && receipts(&store).is_some_and(|value| value["entries"][0]["state"] == expected)
            {
                break;
            }
            if status.status.borrow().closed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    status.request_stop();
    let stopped = kernel.stop_all().await;
    finished.expect("失败应完成原输入收尾，不依赖自主发言模型");
    stopped.unwrap();
    assert!(!status.status.borrow().terminal_error);
    assert_eq!(speech.0.load(Ordering::SeqCst), 0);
    let recovered = recovery.0.lock().unwrap();
    assert_eq!(recovered.len(), 1, "重复输入不重新执行或创建恢复任务");
    assert_eq!(recovered[0].run.commit, commit);
    assert!(recovered[0].run.turn_id.is_some());
    assert!(recovered[0].run.failure.is_some());
    let rows: Vec<Value> = std::fs::read_to_string(directory.path().join("bridge.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests: Vec<_> = rows
        .iter()
        .filter(|row| row["type"] == "platform")
        .collect();
    let ledger = receipts(&store).unwrap();
    assert_eq!(ledger["entries"][0]["state"], expected);
    assert!(ledger["entries"][0]["control_turn_id"].is_null());
    assert_eq!(status.status.borrow().completed, 0);
    assert_eq!(
        status.status.borrow().failed,
        1 + u64::from(notices && unknown)
    );
    assert_eq!(status.status.borrow().sent, u64::from(notices && !unknown));
    if !notices {
        assert!(requests.is_empty());
        assert!(ledger["entries"][0]["reply"].is_null());
        assert!(!rows.iter().any(|row| row["direction"] == "host"
            && matches!(
                row["type"].as_str(),
                Some("reply" | "push" | "progress" | "segment" | "sticker")
            )));
        assert_eq!(
            rows.iter()
                .filter(|row| row["direction"] == "channel" && row["type"] == "message")
                .count(),
            2,
            "原输入已释放，重复投递由持久回执去重"
        );
        return;
    }
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["target"]["scope"], "c2c");
    assert_eq!(requests[0]["target"]["targetId"], "alice");
    assert_eq!(requests[0]["target"]["msgId"], "failed-input");
    let text = requests[0]["text"].as_str().unwrap();
    assert!(text.starts_with("系统通知："));
    assert!(text.contains("本批工具未执行") && text.contains("尚未启动工具调用"));
    assert!(!rows.iter().any(|row| row["direction"] == "host"
        && matches!(
            row["type"].as_str(),
            Some("push" | "segment" | "sticker" | "finish")
        )));
    assert_eq!(ledger["entries"][0]["reply"], text);
}

#[tokio::test]
async fn failure_notice_reaches_the_original_route_without_a_speech_service() {
    scenario(false, false, true, CommitState::Failed).await;
}

#[tokio::test]
async fn unavailable_speech_service_cannot_swallow_the_failure_notice() {
    scenario(true, false, true, CommitState::Failed).await;
}

#[tokio::test]
async fn unknown_platform_result_is_recorded_without_retry_or_speech_fallback() {
    scenario(true, true, true, CommitState::Failed).await;
}

#[tokio::test]
async fn default_failure_keeps_recovery_and_releases_input_without_speech_or_notice() {
    scenario(false, false, false, CommitState::Failed).await;
}

#[tokio::test]
async fn unknown_control_result_is_preserved_and_releases_input_without_a_notice() {
    scenario(true, false, false, CommitState::Unknown).await;
}
