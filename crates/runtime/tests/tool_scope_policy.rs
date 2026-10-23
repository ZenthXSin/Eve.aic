#[path = "support/session.rs"]
mod fixture;
use eve_llm_api::*;
use eve_plugin_api::ServiceId;
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use fixture::*;
use std::sync::{Arc, atomic::Ordering};
struct Policy;
impl ToolScopePolicy for Policy {
    fn tools_allowed(&self, scope: &ContextScope) -> bool {
        scope.user_id != "observer"
    }
}
#[tokio::test]
async fn tool_less_scope_hides_tools_and_rejects_unsolicited_calls_without_affecting_other_users() {
    for (user, starts) in [("observer", 0), ("normal-user", 1)] {
        let provider = Provider::new(vec![
            Step::new(calls()),
            Step::new(final_response("已完成核对")),
        ]);
        let config = LlmHostConfig::default();
        let rig = Rig::new(
            provider.clone(),
            Arc::new(FaultStore::default()),
            config.clone(),
        )
        .await;
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
        .with_tool_scope_policy(Arc::new(Policy));
        let output = host
            .run_scoped_turn_with_events(
                TurnInput {
                    text: "验证工具范围".into(),
                },
                Some(ContextScope {
                    session_id: "scope-policy".into(),
                    user_id: user.into(),
                }),
                &DiscardTurnEvents,
            )
            .await
            .unwrap();
        assert_eq!(output.text, "已完成核对");
        assert_eq!(rig.starts.load(Ordering::SeqCst), starts);
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests[0].tools.is_empty(), user == "observer");
        }
        rig.stop().await;
    }
}
