//! 直接对话的只读具体操作预检，复用脚本和直接工具的可信宿主授权边界。
use super::{LateDispatch, Result, is_synchronous_leaf, rejected};
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_script_runtime::AuthorizedToolDispatch;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Instant};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    name: String,
    arguments: Value,
}
fn args(value: &Value) -> Result<Args> {
    if serde_json::to_vec(value)
        .map_err(|_| rejected("预检参数无法编码"))?
        .len()
        > 1024 * 1024
    {
        return Err(rejected("预检参数超过1MiB"));
    }
    let args: Args = serde_json::from_value(value.clone())
        .map_err(|_| rejected("预检只接受 name 和 arguments，不能指定身份或会话"))?;
    if !is_synchronous_leaf(&args.name) {
        return Err(rejected(
            "这里只预检已审核同步叶子，不支持管理、派发或预检工具本身",
        ));
    }
    Ok(args)
}
fn stopped(context: &ToolExecutionContext) -> Result<()> {
    if context.is_cancel_requested() || context.deadline().is_some_and(|at| at <= Instant::now()) {
        return Err(ToolExecutionError::Cancelled("权限预检已取消或到期".into()));
    }
    Ok(())
}
struct PermissionTool(Arc<LateDispatch>);
pub(super) fn tool(dispatch: Arc<LateDispatch>) -> Arc<dyn Tool> {
    Arc::new(PermissionTool(dispatch))
}
impl Tool for PermissionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect_tool_permission".into(),
            description: "只读预检当前真实发起者能否用实际 arguments 调用指定同步叶子工具；普通成员也可查询，不需要先创建脚本。用于不确定文件、交付或开发操作的参数/权限时，不要每步轮询。只能检查当前已装配同步叶子；不支持管理/派发/自身，不接受身份、会话或授权声明。复用真实参数格式校验、宿主授权及既有文件交付检查，不执行、不写业务文件、不发送消息、不扣额外叶子步骤、不调用审核模型。allowed:true 只表示当时预检通过，不确认文件版本/存在、服务可用或危险动作审核放行，实际执行仍需重新校验。错误不能当作已授权。".into(),
            argument_schema: json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{}},"required":["name","arguments"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> std::result::Result<(), ToolValidationError> {
        args(value)
            .map(|_| ())
            .map_err(|error| ToolValidationError {
                message: error.to_string(),
            })
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            stopped(&context)?;
            if context.scope().is_none_or(|scope| {
                scope.user_id.trim().is_empty() || scope.session_id.trim().is_empty()
            }) {
                return Err(rejected("权限预检需要可信平台身份"));
            }
            let args = args(&call.arguments)?;
            let candidate = ToolCall {
                id: call.id,
                name: args.name,
                arguments: args.arguments,
            };
            let dispatch = self.0.0.get().ok_or_else(|| {
                ToolExecutionError::Failed("权限预检尚未装配，未执行目标工具".into())
            })?;
            let decision = dispatch.preflight_call(&candidate, &context);
            stopped(&context)?;
            let allowed = match decision {
                Ok(()) => true,
                Err(ToolExecutionError::Rejected(_)) => false,
                Err(ToolExecutionError::Cancelled(_)) => {
                    return Err(ToolExecutionError::Cancelled(
                        "权限预检已停止，未执行目标工具".into(),
                    ));
                }
                Err(_) => {
                    return Err(ToolExecutionError::Failed(
                        "权限预检不可用，未执行目标工具；不能据此确认授权".into(),
                    ));
                }
            };
            Ok(
                json!({"tool":candidate.name,"allowed":allowed,"executed":false,
                "authorization_reserved":false,"review_verified":false,"business_result_verified":false,
                "reason":if allowed {"preflight_passed"} else {"rejected"}}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ContextScope, SharedToolSteps};
    use eve_script_runtime::{CheckedTools, ToolAuthorizer};
    use std::sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    };

    type Observation = (String, Option<Value>, ContextScope);
    struct Policy {
        mode: AtomicUsize,
        seen: Mutex<Vec<Observation>>,
    }
    impl ToolAuthorizer for Policy {
        fn authorize(
            &self,
            name: &str,
            arguments: Option<&Value>,
            ctx: &ToolExecutionContext,
        ) -> Result<()> {
            self.seen.lock().unwrap().push((
                name.into(),
                arguments.cloned(),
                ctx.scope().unwrap().clone(),
            ));
            match self.mode.load(Ordering::SeqCst) {
                1 => return Err(rejected("PRIVATE_POLICY_DETAIL")),
                2 => return Err(ToolExecutionError::Failed("PRIVATE_BACKEND_DETAIL".into())),
                3 => ctx.cancellation_handle().cancel(),
                _ => {}
            }
            if arguments.is_some_and(|args| args["path"] != "member.txt") {
                return Err(rejected("PRIVATE_PATH_DETAIL"));
            }
            Ok(())
        }
    }
    struct Read;
    impl Tool for Read {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "read_file".into(),
                description: "test".into(),
                argument_schema: json!({"type":"object"}),
                required_permissions: vec![],
                concurrency: None,
            }
        }
        fn validate_arguments(
            &self,
            value: &Value,
        ) -> std::result::Result<(), ToolValidationError> {
            if value.as_object().is_some_and(|args| {
                args.len() == 1 && args.get("path").is_some_and(Value::is_string)
            }) {
                Ok(())
            } else {
                Err(ToolValidationError {
                    message: "PRIVATE_VALIDATOR_DETAIL".into(),
                })
            }
        }
        fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
            Box::pin(async { panic!("只读预检不得执行目标工具") })
        }
    }
    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(ContextScope {
                session_id: "group".into(),
                user_id: "member".into(),
            }))
            .with_shared_steps(SharedToolSteps::new(2).unwrap())
    }
    async fn query(tool: &Arc<dyn Tool>, value: Value, ctx: ToolExecutionContext) -> Result<Value> {
        tool.execute(
            ToolCall {
                id: "query".into(),
                name: "inspect_tool_permission".into(),
                arguments: value,
            },
            ctx,
        )
        .await
    }
    #[tokio::test]
    async fn actual_parameter_policy_identity_and_revocation_are_checked_without_execution() {
        let policy = Arc::new(Policy {
            mode: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
        });
        let dispatch = Arc::new(LateDispatch(OnceLock::new()));
        assert!(
            dispatch
                .0
                .set(CheckedTools::new(vec![Arc::new(Read)], policy.clone()).unwrap())
                .is_ok()
        );
        let tool = tool(dispatch);
        let ctx = context();
        let value = json!({"name":"read_file","arguments":{"path":"member.txt"}});
        let result = query(&tool, value.clone(), ctx.clone()).await.unwrap();
        assert_eq!(
            result,
            json!({"tool":"read_file","allowed":true,"executed":false,
            "authorization_reserved":false,"review_verified":false,"business_result_verified":false,"reason":"preflight_passed"})
        );
        assert_eq!(
            *policy.seen.lock().unwrap(),
            vec![
                ("read_file".into(), None, ctx.scope().unwrap().clone()),
                (
                    "read_file".into(),
                    Some(json!({"path":"member.txt"})),
                    ctx.scope().unwrap().clone()
                )
            ]
        );
        for arguments in [json!({}), json!({"path":"private.txt"})] {
            let result = query(
                &tool,
                json!({"name":"read_file","arguments":arguments}),
                ctx.clone(),
            )
            .await
            .unwrap();
            assert_eq!(result["allowed"], false);
            assert!(!result.to_string().contains("PRIVATE"));
        }
        policy.mode.store(1, Ordering::SeqCst);
        assert_eq!(
            query(&tool, value.clone(), ctx.clone()).await.unwrap()["allowed"],
            false
        );
        policy.mode.store(2, Ordering::SeqCst);
        let error = query(&tool, value.clone(), ctx.clone()).await.unwrap_err();
        assert!(matches!(error, ToolExecutionError::Failed(_)));
        assert!(!error.to_string().contains("PRIVATE"));
        policy.mode.store(3, Ordering::SeqCst);
        assert!(matches!(
            query(&tool, value, ctx.clone()).await,
            Err(ToolExecutionError::Cancelled(_))
        ));
        assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 2);
    }
    #[tokio::test]
    async fn query_rejects_identity_injection_recursion_unbound_dispatch_and_stopped_contexts() {
        let tool = tool(Arc::new(LateDispatch(OnceLock::new())));
        let value = json!({"name":"read_file","arguments":{"path":"member.txt"}});
        assert!(matches!(
            query(&tool, value.clone(), context()).await,
            Err(ToolExecutionError::Failed(_))
        ));
        for forbidden in [
            "inspect_tool_permission",
            "run_subagent",
            "apply_upgrade",
            "run_script_plugin",
            "new_external_plugin",
        ] {
            assert!(
                tool.validate_arguments(&json!({"name":forbidden,"arguments":{}}))
                    .is_err()
            );
        }
        for field in ["session_id", "user_id", "owner_confirmed"] {
            let mut forged = value.clone();
            forged[field] = json!("forged");
            assert!(tool.validate_arguments(&forged).is_err());
        }
        assert!(matches!(
            query(&tool, value.clone(), ToolExecutionContext::new()).await,
            Err(ToolExecutionError::Rejected(_))
        ));
        let expired = context().with_deadline(Some(Instant::now()));
        assert!(matches!(
            query(&tool, value, expired).await,
            Err(ToolExecutionError::Cancelled(_))
        ));
    }
}
