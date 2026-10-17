#[path = "support/session.rs"]
mod fixture;
use eve_llm_api::*;
use eve_plugin_api::ServiceId;
use eve_runtime::{
    ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost, SessionRunError,
};
use fixture::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Window {
    value: Mutex<HistoryWindow>,
    calls: AtomicUsize,
}
impl HistoryWindowResolver for Window {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(*self.value.lock().unwrap())
    }
}
fn host(rig: &Rig, provider: Arc<Provider>, reader: Arc<Window>) -> SessionLlmHost {
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
    .with_history_resolver(reader);
    SessionLlmHost::new(host, SessionBinding::builtin())
}

#[tokio::test]
async fn whole_tool_turns_and_failures_survive_hot_window_changes_without_replay_or_store_trimming()
{
    let provider = Provider::new(vec![
        Step::new(calls()),
        Step::new(final_response("工具完成")),
        Step::new(Err(LlmError::ProviderTimeout)),
        Step::new(final_response("已接续")),
        Step::new(final_response("窗口扩大")),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let reader = Arc::new(Window {
        value: Mutex::new(HistoryWindow {
            max_turns: 1,
            max_bytes: 65536,
        }),
        calls: AtomicUsize::new(0),
    });
    let session = host(&rig, provider.clone(), reader.clone());
    session.run_turn(input("s", "先调用工具")).await.unwrap();
    assert!(
        session
            .run_turn(input("s", "完成一个新请求"))
            .await
            .is_err()
    );
    session.run_turn(input("s", "继续")).await.unwrap();
    {
        let requests = provider.requests.lock().unwrap();
        requests[2].validate().unwrap();
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|m| m.role == ChatRole::Tool)
        );
        assert!(
            !requests[3]
                .messages
                .iter()
                .any(|m| m.role == ChatRole::Tool)
        );
        assert!(
            requests[3]
                .messages
                .iter()
                .any(|m| m.text.as_deref() == Some("完成一个新请求"))
        );
        assert!(requests[3].messages.iter().any(|m| {
            m.text
                .as_deref()
                .is_some_and(|s| s.contains("宿主失败记录"))
        }));
        assert!(requests[3].messages.iter().any(|m| {
            m.text
                .as_deref()
                .is_some_and(|s| s.contains("eve.runtime.history-window"))
        }));
    }
    reader.value.lock().unwrap().max_turns = 8;
    session.run_turn(input("s", "扩大后查看")).await.unwrap();
    assert!(
        provider.requests.lock().unwrap()[4]
            .messages
            .iter()
            .any(|m| m.role == ChatRole::Tool)
    );
    assert_eq!(rig.snapshot("s").turns.len(), 4);
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1); // 仅最初一条有效调用；历史未执行。
    assert_eq!(reader.calls.load(Ordering::SeqCst), 4);
    assert_eq!(rig.snapshot("s").turns[0].input, "先调用工具");
    rig.stop().await;
}

#[tokio::test]
async fn oversized_latest_turn_stops_before_provider_and_invalid_policy_stops_before_pending() {
    let provider = Provider::new(vec![Step::new(final_response(&"中文".repeat(600)))]);
    let store = Arc::new(FaultStore::default());
    let rig = Rig::new(provider.clone(), store.clone(), LlmHostConfig::default()).await;
    let reader = Arc::new(Window {
        value: Mutex::new(HistoryWindow {
            max_turns: 16,
            max_bytes: 1024,
        }),
        calls: AtomicUsize::new(0),
    });
    let session = host(&rig, provider.clone(), reader.clone());
    session.run_turn(input("s", "生成大轮次")).await.unwrap();
    let first = rig.snapshot("s").turns[0].clone();
    assert!(matches!(session.run_turn(input("s", "继续")).await,
        Err(SessionRunError::Turn(failure)) if failure.error == LlmError::HistoryWindowExceeded && failure.diagnostics.provider_requests == 0 && failure.diagnostics.started_tools == 0));
    assert_eq!(rig.snapshot("s").turns[0], first);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let before = store.bytes();
    reader.value.lock().unwrap().max_turns = 0;
    assert!(matches!(
        session.run_turn(input("s", "无效配置")).await,
        Err(SessionRunError::NotStarted(_))
    ));
    assert_eq!(store.bytes(), before);
    rig.stop().await;
}
