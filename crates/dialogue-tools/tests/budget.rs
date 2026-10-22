use eve_budget_api::{
    AutonomyBudget, BudgetError, BudgetPolicy, BudgetResult, BudgetSnapshot, BudgetWindow,
};
use eve_dialogue_tools::Services;
use eve_llm_api::{ContextScope, Tool, ToolCall, ToolExecutionContext};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

const HOUR: u64 = 3_600_000;

struct Budget {
    reserves: Arc<AtomicUsize>,
    broken: bool,
}
impl AutonomyBudget for Budget {
    fn snapshot(&self, now_ms: u64) -> BudgetResult<BudgetSnapshot> {
        if self.broken {
            return Err(BudgetError::Unavailable);
        }
        let policy = BudgetPolicy {
            window_ms: 24 * HOUR,
            limit: 10,
        };
        let start_ms = policy.window_start(now_ms);
        Ok(BudgetSnapshot {
            policy,
            current: BudgetWindow {
                start_ms,
                used: 3,
                by_category: BTreeMap::from([("interest".into(), 1), ("research".into(), 2)]),
            },
            resets_at_ms: start_ms + policy.window_ms,
            history: vec![BudgetWindow {
                start_ms: start_ms - policy.window_ms,
                used: 7,
                by_category: BTreeMap::new(),
            }],
        })
    }
    fn reserve(&self, _: &str, _: u32, _: u64) -> BudgetResult<u32> {
        self.reserves.fetch_add(1, Ordering::SeqCst);
        Err(BudgetError::Unavailable)
    }
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: "session-1".into(),
        user_id: "user-1".into(),
    }))
}
fn tool(broken: bool) -> (Arc<dyn Tool>, Arc<AtomicUsize>) {
    let reserves = Arc::new(AtomicUsize::new(0));
    let services = Services {
        budget: Some(Arc::new(Budget {
            reserves: reserves.clone(),
            broken,
        })),
        ..Services::default()
    };
    let (mut tools, _) = services.tools();
    assert_eq!(tools.len(), 1);
    (tools.remove(0), reserves)
}
fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "show_budget".into(),
        arguments,
    }
}

#[tokio::test]
async fn budget_tool_is_registered_only_when_assembled_and_reads_without_reserving() {
    assert!(
        Services::default()
            .tools()
            .0
            .iter()
            .all(|t| t.definition().name != "show_budget")
    );
    let (tool, reserves) = tool(false);
    assert_eq!(tool.definition().name, "show_budget");
    let result = tool.execute(call(json!({})), context()).await.unwrap();
    assert_eq!(result["limit"], 10);
    assert_eq!(result["window_ms"], 24 * HOUR);
    assert_eq!(result["used"], 3);
    assert_eq!(result["remaining"], 7);
    assert_eq!(result["by_category"], json!({"interest": 1, "research": 2}));
    assert_eq!(result["previous_window"]["used"], 7);
    let minutes = result["resets_in_minutes"].as_u64().unwrap();
    assert!((1..=24 * 60).contains(&minutes));
    assert_eq!(
        result["resets_at_ms"].as_u64().unwrap() - result["window_started_at_ms"].as_u64().unwrap(),
        24 * HOUR
    );
    assert_eq!(reserves.load(Ordering::SeqCst), 0, "查看不记账");
}

#[tokio::test]
async fn budget_tool_rejects_arguments_and_missing_scope_and_reports_unavailable_budget() {
    let (tool, reserves) = tool(false);
    assert!(
        tool.validate_arguments(&json!({"user_id": "user-2"}))
            .is_err()
    );
    assert!(
        tool.execute(call(json!({"limit": 99})), context())
            .await
            .is_err()
    );
    assert!(
        tool.execute(call(json!({})), ToolExecutionContext::new())
            .await
            .is_err()
    );
    let (broken, _) = self::tool(true);
    let error = broken
        .execute(call(json!({})), context())
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("暂时不可用"));
    assert_eq!(reserves.load(Ordering::SeqCst), 0);
}
