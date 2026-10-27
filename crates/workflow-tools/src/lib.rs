//! 可复用工具流程：持久化定义与回执，调用时复用宿主已授权的叶子工具。
mod store;
mod tools;
pub use store::WorkflowStore;
pub use tools::tools;

use eve_llm_api::{ContextScope, ToolCall, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOOL_NAMES: [&str; 3] = [
    "save_workflow_tool",
    "list_workflow_tools",
    "run_workflow_tool",
];
const MAX_STEPS: usize = 20;
const MAX_DEFINITION_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub name: String,
    pub description: String,
    /// 参数占位符是独立 JSON 对象 {"$param":"名称"}，不拼接命令字符串。
    pub steps: Vec<ToolCall>,
}
impl WorkflowDefinition {
    fn validate(&self) -> Result<(), ToolExecutionError> {
        if self.name.is_empty()
            || self.name.len() > 96
            || self.name.trim() != self.name
            || self.name.chars().any(char::is_control)
            || self.description.trim().is_empty()
            || self.description.len() > 1024
            || self.steps.is_empty()
            || self.steps.len() > MAX_STEPS
            || serde_json::to_vec(self).map_err(failed)?.len() > MAX_DEFINITION_BYTES
        {
            return Err(failed("流程名称、说明或步骤无效"));
        }
        let mut ids = std::collections::HashSet::new();
        for step in &self.steps {
            step.validate().map_err(failed)?;
            if step.id.len() > 128
                || !ids.insert(&step.id)
                || TOOL_NAMES.contains(&step.name.as_str())
            {
                return Err(failed("流程步骤编号重复或含递归流程调用"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWorkflow {
    pub id: String,
    pub revision: u64,
    pub owner: ContextScope,
    pub definition: WorkflowDefinition,
}

fn failed(message: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Rejected(message.to_string())
}

fn digest(value: &impl Serialize) -> Result<String, ToolExecutionError> {
    let bytes = serde_json::to_vec(value).map_err(failed)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn expand(value: &Value, parameters: &Value) -> Result<Value, ToolExecutionError> {
    match value {
        Value::Object(object) if object.contains_key("$param") => {
            let name = object
                .get("$param")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            if object.len() != 1 || name.is_none() {
                return Err(failed("参数引用必须只有 $param 字段"));
            }
            parameters
                .get(name.expect("checked"))
                .cloned()
                .ok_or_else(|| failed("调用缺少流程要求的参数"))
        }
        Value::Object(object) => Ok(Value::Object(
            object
                .iter()
                .map(|(k, v)| Ok((k.clone(), expand(v, parameters)?)))
                .collect::<Result<_, ToolExecutionError>>()?,
        )),
        Value::Array(array) => Ok(Value::Array(
            array
                .iter()
                .map(|v| expand(v, parameters))
                .collect::<Result<_, _>>()?,
        )),
        _ => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{
        SharedToolSteps, Tool, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolFuture,
        ToolValidationError,
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Leaf {
        allowed: AtomicBool,
        calls: AtomicUsize,
    }
    impl Tool for Leaf {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "run_command".into(),
                description: "测试叶子".into(),
                argument_schema: json!({"type":"object"}),
                required_permissions: vec![],
                concurrency: Some(ToolConcurrency::ParallelSafe),
            }
        }
        fn validate_arguments(&self, args: &Value) -> Result<(), ToolValidationError> {
            if args.get("mode").and_then(Value::as_str).is_none() {
                Err(ToolValidationError {
                    message: "缺少 mode".into(),
                })
            } else {
                Ok(())
            }
        }
        fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
            Box::pin(async move {
                if !self.allowed.load(Ordering::SeqCst) {
                    return Err(ToolExecutionError::Rejected("权限已撤回".into()));
                }
                self.calls.fetch_add(1, Ordering::SeqCst);
                if call.arguments["mode"] == "unknown" {
                    Err(ToolExecutionError::Failed("超时，结果未知".into()))
                } else {
                    Ok(
                        json!({"success":call.arguments["mode"] != "failed","exit_code":if call.arguments["mode"] == "failed" {2}else{0}}),
                    )
                }
            })
        }
    }
    fn setup() -> (tempfile::TempDir, Arc<WorkflowStore>, Arc<Leaf>) {
        let root = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open(root.path().to_path_buf()).unwrap();
        let leaf = Arc::new(Leaf {
            allowed: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
        });
        store.bind_tools(vec![leaf.clone()]).unwrap();
        (root, store, leaf)
    }
    fn owner() -> ContextScope {
        ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        }
    }
    fn context(budget: usize) -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(owner()))
            .with_shared_steps(SharedToolSteps::new(budget).unwrap())
    }
    fn spec() -> WorkflowDefinition {
        WorkflowDefinition {
            name: "检查_build".into(),
            description: "验证已确认的流程".into(),
            steps: vec![
                ToolCall {
                    id: "first".into(),
                    name: "run_command".into(),
                    arguments: json!({"mode":{"$param":"mode"}}),
                },
                ToolCall {
                    id: "second".into(),
                    name: "run_command".into(),
                    arguments: json!({"mode":"ok"}),
                },
            ],
        }
    }
    fn run_call(saved: &SavedWorkflow, mode: &str) -> ToolCall {
        ToolCall {
            id: "run".into(),
            name: TOOL_NAMES[2].into(),
            arguments: json!({"workflow_id":saved.id,"revision":saved.revision,"parameters":{"mode":mode}}),
        }
    }

    #[tokio::test]
    async fn workflow_uses_current_authorization_and_budget_and_stops_after_failure() {
        let (_root, store, leaf) = setup();
        let saved = store.save(&owner(), spec(), None).unwrap();
        let run = tools(store.clone()).remove(2);
        assert!(
            run.execute(run_call(&saved, "ok"), context(1))
                .await
                .is_err()
        );
        assert_eq!(leaf.calls.load(Ordering::SeqCst), 0);
        let result = run
            .execute(run_call(&saved, "failed"), context(3))
            .await
            .unwrap();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["steps"].as_array().unwrap().len(), 1);
        leaf.allowed.store(false, Ordering::SeqCst);
        let result = run
            .execute(run_call(&saved, "ok"), context(3))
            .await
            .unwrap();
        assert_eq!(result["steps"][0]["status"], "rejected");
        assert_eq!(leaf.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_step_and_saved_workflow_survive_restart_without_replay() {
        let (root, store, leaf) = setup();
        let saved = store.save(&owner(), spec(), None).unwrap();
        let run = tools(store.clone()).remove(2);
        assert!(matches!(
            run.execute(run_call(&saved, "unknown"), context(3)).await,
            Err(ToolExecutionError::Failed(_))
        ));
        let reopened = WorkflowStore::open(root.path().to_path_buf()).unwrap();
        assert_eq!(reopened.get(&owner(), &saved.id).unwrap().revision, 1);
        let record = reopened.read_run(&owner(), "workflow-run-1").unwrap();
        assert_eq!(record["status"], "unknown");
        assert_eq!(record["steps"].as_array().unwrap().len(), 1);
        assert_eq!(leaf.calls.load(Ordering::SeqCst), 1);
        let stranger = ContextScope {
            session_id: "another".into(),
            ..owner()
        };
        assert!(reopened.get(&stranger, &saved.id).is_err());
        assert!(reopened.read_run(&stranger, "workflow-run-1").is_err());
    }

    #[test]
    fn revision_checks_and_storage_errors_preserve_previous_definition() {
        let (root, store, _leaf) = setup();
        let saved = store.save(&owner(), spec(), None).unwrap();
        let mut changed = spec();
        changed.description = "更新定义".into();
        assert!(store.save(&owner(), changed.clone(), None).is_err());
        std::fs::remove_file(root.path().join("workflows.json")).unwrap();
        std::fs::create_dir(root.path().join("workflows.json")).unwrap();
        assert!(store.save(&owner(), changed, Some(1)).is_err());
        assert_eq!(store.get(&owner(), &saved.id).unwrap().definition, spec());
    }

    #[test]
    fn parameter_values_are_not_reparsed_as_shell_or_recursive_templates() {
        let value = expand(
            &json!({"args":[{"$param":"name"}]}),
            &json!({"name":"a; touch /tmp/should-not-exist"}),
        )
        .unwrap();
        assert_eq!(value, json!({"args":["a; touch /tmp/should-not-exist"]}));
        assert!(expand(&json!({"$param":"missing"}), &json!({})).is_err());
        assert!(expand(&json!({"$param":"name","extra":true}), &json!({"name":"x"})).is_err());
    }
}
