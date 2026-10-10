#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
#[path = "support/session.rs"]
mod support;
use eve_kernel::backends::FileStateStore;
use eve_llm_api::{ChatMessage, ChatRole, ToolCall, ToolResult};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use eve_runtime::{LlmHostConfig, SessionRunError};
use eve_session_api::*;
use http_support::{Reply, Server, final_response};
use serde_json::json;
use std::{sync::Arc, sync::atomic::Ordering};
use support::*;

fn provider(url: &str) -> Arc<OpenAiProvider> {
    Arc::new(
        OpenAiProvider::new(
            OpenAiConfig {
                responses_url: url.into(),
                ..OpenAiConfig::new("fixture-model")
            },
            "fake-key",
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn file_recovery_sends_paired_history_and_final_answer_phase_with_stable_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![
        Reply::json(json!({"status":"completed","error":null,"output":[
            {"type":"function_call","call_id":"call-b","name":"receipt","arguments":"{\"text\":\"中文回执\"}"},
            {"type":"function_call","call_id":"call-a","name":"receipt","arguments":"{}"}
        ]})),
        Reply::json(final_response("第一轮完成")),
        Reply::json(final_response("第二轮完成")),
    ]).await;
    let first_rig = Rig::new(
        provider(&server.url),
        Arc::new(FileStateStore::open(dir.path()).unwrap()),
        LlmHostConfig::default(),
    )
    .await;
    let first = first_rig.host.run_turn(input("s", "第一轮")).await.unwrap();
    assert_eq!(first_rig.starts.load(Ordering::SeqCst), 1);
    first_rig.stop().await;
    drop(first_rig); // 真正释放旧后端与目录锁，再重新装配全部插件。
    let second_rig = Rig::new(
        provider(&server.url),
        Arc::new(FileStateStore::open(dir.path()).unwrap()),
        LlmHostConfig::default(),
    )
    .await;
    assert_eq!(second_rig.snapshot("s").history(), first.output.transcript);
    second_rig
        .host
        .run_turn(input("s", "第二轮"))
        .await
        .unwrap();
    assert_eq!(second_rig.starts.load(Ordering::SeqCst), 0);
    let captured = server.next().await;
    assert!(captured.headers.starts_with("POST /v1/responses HTTP/1.1"));
    let first_request = captured.body;
    let tool_request = server.next().await.body;
    let recovered_request = server.next().await.body;
    assert_eq!(first_request["tools"], recovered_request["tools"]);
    let original = first_request["input"].as_array().unwrap();
    let recovered = recovered_request["input"].as_array().unwrap();
    assert_eq!(
        &original[..original.len() - 2],
        &recovered[..original.len() - 2]
    );
    let n = original.len() - 1;
    assert_eq!(
        &tool_request["input"].as_array().unwrap()[n..],
        &recovered[n..n + 4]
    );
    assert_eq!(recovered[n]["call_id"], "call-b");
    assert_eq!(recovered[n + 1]["call_id"], "call-a");
    assert_eq!(recovered[n + 2]["call_id"], "call-b");
    assert_eq!(recovered[n + 3]["call_id"], "call-a");
    let receipt: serde_json::Value =
        serde_json::from_str(recovered[n + 2]["output"].as_str().unwrap()).unwrap();
    assert_eq!(receipt, json!({"receipt":"中文回执"}));
    let failed: serde_json::Value =
        serde_json::from_str(recovered[n + 3]["output"].as_str().unwrap()).unwrap();
    assert_eq!(failed["error"]["code"], "InvalidArguments");
    assert_eq!(recovered[n + 4]["role"], "assistant");
    assert_eq!(recovered[n + 4]["phase"], "final_answer");
    assert_eq!(recovered[n + 4]["content"], "第一轮完成");
    assert_eq!(recovered[n + 5], original[original.len() - 2]);
    assert_eq!(recovered[n + 6]["content"], "第二轮");
    assert!(server.requests.try_recv().is_err());
    second_rig.stop().await;
}

#[tokio::test]
async fn malformed_history_is_rejected_before_http() {
    // 替换实现也必须受 Runtime 的协议校验约束。
    struct InvalidSession;
    impl SessionService for InvalidSession {
        fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            Ok(None)
        }
        fn begin(&self, input: SessionInput) -> SessionResult<StartedTurn> {
            Ok(StartedTurn {
                lease: TurnLease {
                    key: input.key,
                    turn_id: 1,
                },
                revision: 1,
                history: vec![
                    ChatMessage::text(ChatRole::User, "旧输入"),
                    ChatMessage::assistant_tool_calls(vec![ToolCall {
                        id: "expected".into(),
                        name: "receipt".into(),
                        arguments: json!({}),
                    }])
                    .unwrap(),
                    ChatMessage::tool_results(vec![
                        ToolResult::success("wrong", json!({})).unwrap(),
                    ])
                    .unwrap(),
                    ChatMessage::text(ChatRole::Assistant, "旧回复"),
                ],
            })
        }
        fn complete(&self, _: &TurnLease, _: Vec<ChatMessage>) -> SessionResult<()> {
            panic!("非法历史不能完成")
        }
        fn fail(&self, _: &TurnLease, failure: SessionFailure) -> SessionResult<()> {
            assert_eq!(failure.code, SessionFailureCode::Context);
            Ok(())
        }
    }
    let server = Server::start(vec![]).await;
    let rig = Rig::new(
        provider(&server.url),
        Arc::new(eve_kernel::backends::MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let alternate_id = eve_plugin_api::ServiceId::new("session-test.alternate").unwrap();
    let cleanup = rig
        .registry
        .clone()
        .provide(
            id(OWNER),
            alternate_id.clone(),
            Arc::new(SessionServiceHandle(Arc::new(InvalidSession))),
        )
        .unwrap();
    let host = Rig::make_host(
        &rig.kernel,
        &rig.registry,
        &rig.permissions,
        &rig.logger,
        provider(&server.url),
        LlmHostConfig::default(),
        eve_runtime::SessionBinding {
            service_id: alternate_id,
            expected_owner: id(OWNER),
        },
    );
    assert!(matches!(
        host.run_turn(input("s", "新输入")).await,
        Err(SessionRunError::Turn(_))
    ));
    assert!(server.requests.is_empty());
    cleanup().await.unwrap();
    rig.stop().await;
}
