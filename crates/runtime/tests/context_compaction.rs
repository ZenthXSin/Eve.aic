#[path = "support/session.rs"]
mod fixture;
use eve_llm_api::*;
use eve_plugin_api::ServiceId;
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost};
use fixture::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Policy;
impl HistoryWindowResolver for Policy {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        Ok(HistoryWindow {
            max_turns: 1,
            max_bytes: 1024,
        })
    }
    fn compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        Ok(Some(ContextCompactionPolicy {
            trigger_tokens: 150000,
            target_tokens: 100000,
        }))
    }
}
#[derive(Default)]
struct Digests {
    calls: AtomicUsize,
    fail: bool,
}
impl HistoryDigests for Digests {
    fn digest(
        &self,
        _: Option<&ContextScope>,
        _: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        unreachable!()
    }
    fn compact<'a>(
        &'a self,
        _: Option<&'a ContextScope>,
        messages: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryDigest>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err(LlmError::Backend("isolated-failure".into()));
            }
            Ok(Some(HistoryDigest {
                covered_turns: history_turns(messages).len(),
                text: "工具已经执行，回执完整保存在原轮次。继续当前用户任务。".into(),
            }))
        })
    }
}
fn host(rig: &Rig, provider: Arc<Provider>, digests: Arc<Digests>) -> SessionLlmHost {
    let host = LlmHost::new(
        provider,
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![ToolBinding {
            name: "receipt".into(),
            service_id: ServiceId::new(TOOL).unwrap(),
            expected_owner: id(OWNER),
        }],
        LlmHostConfig::default(),
    )
    .unwrap()
    .with_history_resolver(Arc::new(Policy))
    .with_history_digests(digests);
    SessionLlmHost::new(host, SessionBinding::builtin())
}
fn huge_call() -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::ToolCalls {
        calls: vec![ToolCall {
            id: "already-executed".into(),
            name: "receipt".into(),
            arguments: serde_json::json!({"text":"中".repeat(45000)}),
        }],
    })
}

#[tokio::test]
async fn compaction_during_tool_loop_continues_without_replaying_and_keeps_full_session_transcript()
{
    let provider = Provider::new(vec![
        Step::new(huge_call()),
        Step::new(final_response("完成")),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let digests = Arc::new(Digests::default());
    let session = host(&rig, provider.clone(), digests.clone());
    let result = session
        .run_turn(input("s", "执行并完成当前任务"))
        .await
        .unwrap();
    assert_eq!(result.output.text, "完成");
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    assert_eq!(digests.calls.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    requests[1].validate().unwrap();
    assert!(requests[1].messages.iter().any(|m| {
        m.text
            .as_deref()
            .is_some_and(|s| s.contains("宿主上下文压缩摘要"))
    }));
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|m| m.tool_calls.is_empty() && m.tool_results.is_empty())
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .find(|m| m.role == ChatRole::User)
            .unwrap()
            .text
            .as_deref(),
        Some("执行并完成当前任务")
    );
    let saved = serde_json::to_string(&rig.snapshot("s")).unwrap();
    assert!(saved.contains("already-executed"));
    assert!(saved.contains(&"中".repeat(45000)));
    rig.stop().await;
}

#[tokio::test]
async fn compression_failure_does_not_replay_or_discard_started_tool_results() {
    let provider = Provider::new(vec![Step::new(huge_call())]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let digests = Arc::new(Digests {
        fail: true,
        ..Digests::default()
    });
    let session = host(&rig, provider.clone(), digests);
    assert!(session.run_turn(input("s", "执行一次")).await.is_err());
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(
        serde_json::to_string(&rig.snapshot("s"))
            .unwrap()
            .contains("already-executed")
    );
    rig.stop().await;
}

struct Casual;
impl HistoryWindowResolver for Casual {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        Policy.resolve()
    }
    fn compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        Policy.compaction()
    }
    fn casual_compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        Ok(Some(ContextCompactionPolicy {
            trigger_tokens: 6000,
            target_tokens: 4000,
        }))
    }
}
fn casual_host(rig: &Rig, provider: Arc<Provider>, digests: Arc<Digests>) -> SessionLlmHost {
    let host = LlmHost::new(
        provider,
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![ToolBinding {
            name: "receipt".into(),
            service_id: ServiceId::new(TOOL).unwrap(),
            expected_owner: id(OWNER),
        }],
        LlmHostConfig::default(),
    )
    .unwrap()
    .with_history_resolver(Arc::new(Casual))
    .with_history_digests(digests);
    SessionLlmHost::new(host, SessionBinding::builtin())
}

struct Adaptive;
impl HistoryWindowResolver for Adaptive {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        Policy.resolve()
    }
    fn adaptive_limit(&self) -> Result<Option<usize>, LlmError> {
        Ok(Some(200000))
    }
    fn compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        panic!("adaptive mode must not read legacy thresholds")
    }
    fn casual_compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        panic!("adaptive mode must not read casual thresholds")
    }
}

#[derive(Default)]
struct Notices(AtomicUsize);
impl TurnEventSink for Notices {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        if matches!(event.kind, TurnEventKind::ContextCompactionStarted) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn adaptive_context_grows_past_old_soft_budget_without_compressing_each_short_message() {
    let provider = Provider::new(vec![
        Step::new(final_response("在")),
        Step::new(final_response("嗯")),
        Step::new(final_response("好")),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let digests = Arc::new(Digests::default());
    let config = LlmHostConfig {
        system_prompt: "固定资料".repeat(9000),
        ..LlmHostConfig::default()
    };
    let host = LlmHost::new(
        provider.clone(),
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![],
        config,
    )
    .unwrap()
    .with_history_resolver(Arc::new(Adaptive))
    .with_history_digests(digests.clone());
    let host = SessionLlmHost::new(host, SessionBinding::builtin());
    let notices = Notices::default();
    for text in ["在吗", "嗯", "好"] {
        host.run_turn_with_events(input("s", text), &notices)
            .await
            .unwrap();
    }
    assert_eq!(notices.0.load(Ordering::SeqCst), 0);
    assert_eq!(digests.calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert_eq!(rig.snapshot("s").turns.len(), 3);
    rig.stop().await;
}

#[tokio::test]
async fn casual_turns_compact_at_the_smaller_budget_while_tool_work_keeps_the_main_one() {
    let provider = Provider::new(vec![
        Step::new(final_response("记下了")),
        Step::new(final_response("好的")),
        Step::new(Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                id: "medium".into(),
                name: "receipt".into(),
                arguments: serde_json::json!({"text":"中".repeat(20000)}),
            }],
        })),
        Step::new(final_response("完成")),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let digests = Arc::new(Digests::default());
    let session = casual_host(&rig, provider.clone(), digests.clone());
    // 第一轮闲聊带一大段文字；当前输入本身超过闲聊预算时退回主阈值，不让本轮失败。
    let long = "中".repeat(4000);
    session.run_turn(input("s", &long)).await.unwrap();
    assert_eq!(digests.calls.load(Ordering::SeqCst), 0);
    // 第二轮闲聊：历史已超过闲聊预算，先压缩再请求。
    session.run_turn(input("s", "在吗")).await.unwrap();
    assert_eq!(digests.calls.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap().clone();
    assert!(requests[1].messages.iter().any(|m| {
        m.text
            .as_deref()
            .is_some_and(|s| s.contains("宿主上下文压缩摘要"))
    }));
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|m| m.text.as_deref() == Some(long.as_str()))
    );
    // 第三轮开始调用工具：回执远超闲聊预算，但仍低于主阈值，不再压缩。
    session.run_turn(input("s", "执行任务")).await.unwrap();
    // 首个请求尚未调用工具，仍按闲聊预算压缩；工具回执之后按主阈值，不再压缩。
    assert_eq!(digests.calls.load(Ordering::SeqCst), 2);
    let requests = provider.requests.lock().unwrap().clone();
    let last = requests.last().unwrap();
    assert!(last.messages.iter().any(|m| !m.tool_results.is_empty()));
    rig.stop().await;
}
