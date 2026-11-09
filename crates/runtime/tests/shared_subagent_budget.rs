#[path = "support/session.rs"]
mod fixture;
use eve_llm_api::*;
use eve_plugin_api::ServiceId;
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use fixture::*;
use serde_json::json;
use std::sync::{Arc, Mutex, atomic::Ordering};

struct Journal(Mutex<TurnCheckpoint>);
impl TurnJournal for Journal {
    fn load(&self) -> Result<Option<TurnCheckpoint>, LlmError> {
        Ok(Some(self.0.lock().unwrap().clone()))
    }
    fn save(&self, frame: &TurnCheckpoint) -> Result<(), LlmError> {
        *self.0.lock().unwrap() = frame.clone();
        Ok(())
    }
}
#[tokio::test]
async fn resumed_parent_keeps_child_charges_and_refuses_a_batch_beyond_remaining_steps() {
    let provider = Provider::new(vec![
        Step::new(calls()),
        Step::new(final_response("保留余额，未启动超额批次")),
    ]);
    let config = LlmHostConfig {
        max_tool_calls: 4,
        max_tool_rounds: 0,
        final_answer_on_round_limit: true,
        ..LlmHostConfig::default()
    };
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        config.clone(),
    )
    .await;
    let scope = ContextScope {
        session_id: "scoped-parent".into(),
        user_id: "用户一".into(),
    };
    let past = ToolCall {
        id: "past-parent-call".into(),
        name: "receipt".into(),
        arguments: json!({}),
    };
    let frame = TurnCheckpoint {
        pending_model_request: None,
        pending_request_fingerprint: None,
        model_session: None,
        version: 2,
        scope: scope.clone(),
        messages: vec![
            ChatMessage::text(ChatRole::System, "旧宿主头"),
            ChatMessage::text(ChatRole::System, "旧工具清单"),
            ChatMessage::text(ChatRole::User, "核验任务"),
            ChatMessage::assistant_tool_calls(vec![past]).unwrap(),
            ChatMessage::tool_results(vec![
                ToolResult::success("past-parent-call", json!({"verified":true})).unwrap(),
            ])
            .unwrap(),
        ],
        pending_calls: vec![],
        used_tool_calls: 1,
        delegated_tool_calls: 2,
        delegated_model_requests: 2,
        provider_requests: 1,
        tool_batches: 1,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: None,
    };
    frame.validate(4).unwrap();
    let journal = Arc::new(Journal(Mutex::new(frame)));
    let pool = SharedToolSteps::new(4).unwrap();
    let host = LlmHost::new(
        provider.clone(),
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
        config,
    )
    .unwrap()
    .with_shared_steps(pool.clone())
    .with_turn_journal(journal.clone());
    let output = host
        .run_scoped_turn_with_events(
            TurnInput {
                text: "核验任务".into(),
            },
            Some(scope),
            &DiscardTurnEvents,
        )
        .await
        .unwrap();
    assert_eq!(output.text, "保留余额，未启动超额批次");
    assert_eq!(rig.starts.load(Ordering::SeqCst), 0);
    assert_eq!(pool.usage().unwrap().tool_calls, 3);
    assert_eq!(pool.remaining().unwrap(), 1);
    {
        let saved = journal.0.lock().unwrap();
        assert_eq!((saved.used_tool_calls, saved.delegated_tool_calls), (1, 2));
        assert_eq!(saved.delegated_model_requests, 2);
        assert!(saved.closing);
        saved.validate(4).unwrap();
    }
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].tools.is_empty());
        for request in requests.iter() {
            assert!(
                request
                    .messages
                    .last()
                    .unwrap()
                    .text
                    .as_deref()
                    .unwrap()
                    .contains(
                        "根任务工具总额 4，父线程及全部子代理合计已用 3，本执行者当前最多可用 1",
                    )
            );
        }
    }
    rig.stop().await;
}

#[tokio::test]
async fn exhausted_delegate_stops_tools_while_parent_can_verify_with_reserved_steps() {
    let valid_calls = |prefix: &str| {
        Ok(ModelResponse::ToolCalls {
            calls: (0..2)
                .map(|index| ToolCall {
                    id: format!("{prefix}-{index}"),
                    name: "receipt".into(),
                    arguments: json!({"text":format!("{prefix} 实际核验 {index}")}),
                })
                .collect(),
        })
    };
    let child_provider = Provider::new(vec![
        Step::new(valid_calls("child")),
        Step::new(final_response("子任务交回真实结果")),
    ]);
    let config = LlmHostConfig {
        max_tool_calls: 4,
        max_tool_rounds: 0,
        final_answer_on_round_limit: true,
        ..LlmHostConfig::default()
    };
    let rig = Rig::new(
        child_provider.clone(),
        Arc::new(FaultStore::default()),
        config.clone(),
    )
    .await;
    let pool = SharedToolSteps::new(4).unwrap();
    let make_host = |provider, steps| {
        LlmHost::new(
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
            config.clone(),
        )
        .unwrap()
        .with_shared_steps(steps)
    };
    let child = make_host(child_provider.clone(), pool.with_parent_reserve(2));
    let output = child
        .run_scoped_turn_with_events(
            TurnInput {
                text: "执行子任务".into(),
            },
            None,
            &DiscardTurnEvents,
        )
        .await
        .unwrap();
    assert_eq!(output.diagnostics.calls.len(), 2);
    assert_eq!(output.diagnostics.started_tools, 2);
    assert!(
        output
            .diagnostics
            .tool_results
            .iter()
            .all(|result| { matches!(result.output, ToolOutput::Success(_)) })
    );
    assert_eq!(rig.starts.load(Ordering::SeqCst), 2);
    assert_eq!(pool.remaining().unwrap(), 2);
    {
        let requests = child_provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].tools.is_empty());
        assert!(requests[0].messages.last().unwrap().text.as_deref().unwrap().contains(
            "根任务工具总额 4，父线程及全部子代理合计已用 0，本执行者当前最多可用 2，本执行者不可消费的父线程保留额度为 2",
        ));
        assert!(requests[1].messages.last().unwrap().text.as_deref().unwrap().contains(
            "根任务工具总额 4，父线程及全部子代理合计已用 2，本执行者当前最多可用 0，本执行者不可消费的父线程保留额度为 2",
        ));
        assert!(requests[1].messages.iter().any(|m| {
            m.text
                .as_deref()
                .is_some_and(|s| s.contains("本子任务可用工具额度已用尽"))
        }));
    }
    let parent_provider = Provider::new(vec![
        Step::new(valid_calls("parent")),
        Step::new(final_response("主线程核验完毕")),
    ]);
    let parent = make_host(parent_provider.clone(), pool.clone());
    let output = parent
        .run_scoped_turn_with_events(
            TurnInput {
                text: "核验交付物".into(),
            },
            None,
            &DiscardTurnEvents,
        )
        .await
        .unwrap();
    assert_eq!(output.diagnostics.calls.len(), 2);
    assert_eq!(output.diagnostics.started_tools, 2);
    assert!(
        output
            .diagnostics
            .tool_results
            .iter()
            .all(|result| { matches!(result.output, ToolOutput::Success(_)) })
    );
    assert_eq!(pool.usage().unwrap().tool_calls, 4);
    assert_eq!(rig.starts.load(Ordering::SeqCst), 4);
    assert!(!pool.reserve(1).unwrap());
    {
        let requests = parent_provider.requests.lock().unwrap();
        assert!(requests[1].tools.is_empty());
        assert!(requests[0].messages.last().unwrap().text.as_deref().unwrap().contains(
            "根任务工具总额 4，父线程及全部子代理合计已用 2，本执行者当前最多可用 2，本执行者不可消费的父线程保留额度为 0",
        ));
        assert!(requests[1].messages.last().unwrap().text.as_deref().unwrap().contains(
            "根任务工具总额 4，父线程及全部子代理合计已用 4，本执行者当前最多可用 0，本执行者不可消费的父线程保留额度为 0",
        ));
    }
    rig.stop().await;
}
