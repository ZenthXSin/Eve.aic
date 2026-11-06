//! 使用真实 Rust live loop 与生产 Node bridge-core，所有平台/模型接口均为本地替身。
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::{CommitState, ControlRunner, RunFuture, RunReport};
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, LlmFuture, ToolCall, ToolExecutionContext, TurnEvent,
    TurnEventKind, TurnEventSink, TurnSteeringSource,
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
    sync::{Arc, Mutex},
    time::Duration,
};

struct Runner {
    sessions: Arc<dyn SessionService>,
    live: Arc<LiveDialogue>,
}
impl ControlRunner for Runner {
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<eve_control_api::ControlRunMetadata>,
        sink: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        let metadata = metadata.expect("QQ 轮次携带真实入站身份");
        metadata.validate_scope(&input.key).unwrap();
        assert!(
            matches!(metadata.review, eve_control_api::ControlReviewEvidence::ChannelAnchor(anchor) if anchor == "fresh-input")
        );
        self.run(input, sink)
    }
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            let started = self.sessions.begin(input.clone()).unwrap();
            sink.emit(TurnEvent {
                turn_id: Some(started.lease.turn_id),
                kind: TurnEventKind::ProviderStarted { request: 1 },
            })
            .await
            .unwrap();
            let scope = ContextScope {
                session_id: input.key.session_id.clone(),
                user_id: input.key.user_id.clone(),
            };
            let turn = self.live.open(&scope, started.lease.turn_id).unwrap();
            let result = self
                .live
                .send_tool()
                .execute(
                    ToolCall {
                        id: "expression-1".into(),
                        name: "send_message".into(),
                        arguments: json!({"text":"这是本轮真实回复","purpose":"answer"}),
                    },
                    ToolExecutionContext::new()
                        .with_scope(Some(scope))
                        .with_turn_id(Some(started.lease.turn_id)),
                )
                .await
                .unwrap();
            assert_eq!(result["sent"], true);
            turn.take(true).unwrap();
            let transcript = vec![
                ChatMessage::text(ChatRole::User, &input.text),
                ChatMessage::text(ChatRole::Assistant, "内部结束不是补发通知"),
            ];
            self.sessions
                .complete(&started.lease, transcript.clone())
                .unwrap();
            RunReport {
                turn_id: Some(started.lease.turn_id),
                commit: CommitState::Completed,
                text: Some("内部结束不是补发通知".into()),
                transcript: Some(transcript),
                started_tools: Some(1),
                tool_results: vec![],
                failure: None,
            }
        })
    }
}
#[derive(Default)]
struct Archive(Mutex<Vec<QqOriginalMessage>>);
impl QqReceiptArchive for Archive {
    fn identity(&self) -> PluginResult<(String, u64)> {
        Ok(("0123456789abcdef0123456789abcdef".into(), 0))
    }
    fn store(&self, _: &[QqArchivedReceipt]) -> PluginResult<u64> {
        panic!("小型夹具无需归档回执")
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
struct Source {
    claimed: Mutex<Option<String>>,
    delivered: Mutex<Vec<QqOutreachResult>>,
}
impl QqOutreach for Source {
    fn passive_candidates(
        &self,
        session: &SessionKey,
        anchor: &str,
    ) -> PluginResult<Vec<QqOutreachCandidate>> {
        if self.claimed.lock().unwrap().is_some() {
            return Ok(vec![]);
        }
        Ok(vec![QqOutreachCandidate {
            id: "task-pending".into(),
            version: format!("failed-once-{anchor}"),
            session: Some(session.clone()),
            user_id: session.user_id.clone(),
            facts: json!({"kind":"background_task_result","proactive_failed":true,"result":"保留的后台任务结果"}),
        }])
    }
    fn claim_passive_candidate(
        &self,
        candidate: &QqOutreachCandidate,
        text: &str,
        anchor: &str,
    ) -> PluginResult<Option<QqOutreachPush>> {
        assert_eq!(anchor, "fresh-input");
        assert_eq!(text, "Eve 根据任务结果写出的补充");
        assert!(
            self.claimed
                .lock()
                .unwrap()
                .replace(anchor.into())
                .is_none()
        );
        Ok(Some(QqOutreachPush {
            session: candidate.session.clone(),
            user_id: candidate.user_id.clone(),
            message: QqOutreachMessage {
                id: "task-pending:attempt-2".into(),
                text: text.into(),
            },
        }))
    }
    fn claim_candidate(
        &self,
        _: &QqOutreachCandidate,
        _: &str,
        _: &str,
    ) -> PluginResult<Option<QqOutreachPush>> {
        panic!("不得第二次主动发送")
    }
    fn due(&self, _: &SessionKey) -> PluginResult<bool> {
        panic!("live 不得退回旧模板")
    }
    fn judge(&self, _: QqOutreachMoment) -> QqOutreachFuture {
        panic!("live 不得退回旧模板")
    }
    fn attach(&self, _: &SessionKey, _: &str) -> PluginResult<Option<QqOutreachMessage>> {
        panic!("live 不得退回旧模板")
    }
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        panic!("不得第二次主动发送")
    }
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        assert_eq!(id, "task-pending:attempt-2");
        self.delivered.lock().unwrap().push(result);
        Ok(())
    }
}
#[derive(Default)]
struct Speech {
    events: Mutex<BTreeMap<String, QqSpeechEvent>>,
    decisions: Mutex<Vec<QqSpeechEvent>>,
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
        Ok(())
    }
    fn silenced(&self, _: &QqSpeechEvent) -> PluginResult<bool> {
        Ok(false)
    }
    fn consider(&self, event: QqSpeechEvent) -> LlmFuture<'_, Option<QqSpeechDecision>> {
        Box::pin(async move {
            assert_eq!(event.anchor, "fresh-input");
            assert_eq!(event.facts["proactive_failed"], true);
            self.decisions.lock().unwrap().push(event);
            Ok(Some(QqSpeechDecision::Speak {
                text: "Eve 根据任务结果写出的补充".into(),
                after_ms: 0,
            }))
        })
    }
    fn dispatched(&self, event: &QqSpeechEvent) -> PluginResult<()> {
        self.retire(event)
    }
    fn delivered(&self, _: &QqSpeechEvent, result: QqOutreachResult) -> PluginResult<()> {
        assert!(matches!(
            result,
            QqOutreachResult::Sent {
                platform_message_id: Some(_)
            }
        ));
        Ok(())
    }
    fn record_expression(
        &self,
        _: QqSpeechEvent,
        text: &str,
        result: QqOutreachResult,
    ) -> PluginResult<()> {
        assert_eq!(text, "这是本轮真实回复");
        assert!(matches!(
            result,
            QqOutreachResult::Sent {
                platform_message_id: Some(_)
            }
        ));
        Ok(())
    }
}
#[tokio::test]
async fn live_reply_reviews_failed_proactive_notice_and_delivers_once_in_the_real_passive_window() {
    let directory = tempfile::tempdir().unwrap();
    let events = directory.path().join("bridge.jsonl");
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
    let source = Arc::new(Source::default());
    let speech = Arc::new(Speech::default());
    kernel
        .register(Box::new(
            ControlPlugin::new(
                Arc::new(Runner {
                    sessions,
                    live: live.clone(),
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
            .join("../../connectors/qqbot/test/live-passive-bridge.mjs"),
        bridge_args: vec![events.clone().into()],
        app_id: "app".into(),
        app_secret: "local-fixture".into(),
        sandbox: true,
        markdown: false,
    })
    .unwrap()
    .with_receipt_archive(Arc::new(Archive::default()))
    .with_live_dialogue(live)
    .with_speech(speech.clone())
    .with_outreach(source.clone(), None)
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
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if !source.delivered.lock().unwrap().is_empty() {
                break;
            }
            assert!(!status.status.borrow().closed, "本地 bridge 提前结束");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    status.request_stop();
    kernel.stop_all().await.unwrap();
    result.expect("live 被动通知没有投递");
    assert_eq!(
        *source.delivered.lock().unwrap(),
        vec![QqOutreachResult::Sent {
            platform_message_id: Some("platform-2".into())
        }]
    );
    assert_eq!(speech.decisions.lock().unwrap().len(), 1);
    let rows: Vec<Value> = std::fs::read_to_string(events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let sent: Vec<_> = rows.iter().filter(|v| v["type"] == "platform").collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["text"], "这是本轮真实回复");
    assert_eq!(sent[1]["text"], "Eve 根据任务结果写出的补充");
    assert_eq!(sent[1]["target"]["msgId"], "fresh-input");
    assert_eq!(
        rows.iter().filter(|v| v["type"] == "passive_push").count(),
        1
    );
    assert!(!rows.iter().any(|v| v["type"] == "push"));
}
