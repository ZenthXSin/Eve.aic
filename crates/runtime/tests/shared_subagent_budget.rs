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
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(provider.requests.lock().unwrap()[1].tools.is_empty());
    rig.stop().await;
}
