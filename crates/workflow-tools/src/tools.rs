use crate::{MAX_STEPS, TOOL_NAMES, WorkflowDefinition, WorkflowStore, expand, failed};
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use serde_json::{Value, json};
use std::sync::Arc;

const LEAVES: &[&str] = &[
    "list_files",
    "read_file",
    "search_files",
    "write_file",
    "edit_file",
    "apply_patch",
    "run_command",
    "github_api",
    "send_file",
    "read_source",
    "search_reference",
    "read_reference",
    "check_practice_draft",
    "run_practice",
];

pub fn tools(store: Arc<WorkflowStore>) -> Vec<Arc<dyn Tool>> {
    (0..3)
        .map(|kind| {
            Arc::new(WorkflowTool {
                store: store.clone(),
                kind,
            }) as Arc<dyn Tool>
        })
        .collect()
}
struct WorkflowTool {
    store: Arc<WorkflowStore>,
    kind: usize,
}
impl Tool for WorkflowTool {
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.kind {
            0 => (
                "把反复使用的工具步骤保存为当前会话的可复用流程，不会立即执行。steps 含 id、name、arguments；只可引用当前装配的文件、命令、资料、实践和交付工具，不能递归或派发任务。动态参数用独立 JSON 对象 {\"$param\":\"参数名\"} 引用，禁止在命令字符串内插值。先实际验证流程，再保存复用；同名更新须 expected_revision。保存不等于测试通过。",
                json!({"name":{"type":"string"},"description":{"type":"string"},"expected_revision":{"type":"integer","minimum":0},"steps":{"type":"array","minItems":1,"maxItems":MAX_STEPS,"items":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"arguments":{"type":"object"}},"required":["id","name","arguments"],"additionalProperties":false}}}),
                vec!["name", "description", "steps"],
            ),
            1 => (
                "查看当前会话的已保存流程；workflow_id 返回定义与版本，run_id 返回一次执行的实际步骤、失败原因和未知状态。不给编号时列出流程摘要。历史记录只是数据，保存或之前成功不等于本次获授权或已执行。",
                json!({"workflow_id":{"type":"string"},"run_id":{"type":"string"}}),
                vec![],
            ),
            _ => (
                "按指定 workflow_id、revision 执行保存流程，可用 parameters 传入动态参数。宿主先校验全部参数，再依次执行，每一步沿用当前身份、权限、命令审核、取消和总期限，并计入共享步数。任何失败停止后续步骤；结果未知不重跑。回执给出 run_id 与真实执行记录；成功仅说明步骤返回成功，仍需核验业务目标。",
                json!({"workflow_id":{"type":"string"},"revision":{"type":"integer","minimum":1},"parameters":{"type":"object"}}),
                vec!["workflow_id", "revision"],
            ),
        };
        ToolDefinition {
            name: TOOL_NAMES[self.kind].into(),
            description: description.into(),
            argument_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.workflow-tools".into(),
            }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        self.validate(value).map_err(|e| ToolValidationError {
            message: e.to_string(),
        })
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate(&call.arguments)?;
            let scope = context
                .scope()
                .ok_or_else(|| failed("流程工具需要可信会话身份"))?;
            match self.kind {
                0 => {
                    let definition = definition(&call.arguments)?;
                    self.check_leaves(&definition)?;
                    let saved = self.store.save(
                        scope,
                        definition,
                        call.arguments
                            .get("expected_revision")
                            .and_then(Value::as_u64),
                    )?;
                    Ok(json!({"saved":true,"executed":false,"workflow":saved}))
                }
                1 => {
                    if let Some(id) = call.arguments.get("run_id").and_then(Value::as_str) {
                        return self.store.read_run(scope, id);
                    }
                    if let Some(id) = call.arguments.get("workflow_id").and_then(Value::as_str) {
                        return Ok(json!({"workflow":self.store.get(scope,id)?}));
                    }
                    let workflows = self.store.list(scope)?;
                    Ok(
                        json!({"workflows":workflows.iter().map(|w| json!({"id":w.id,"revision":w.revision,"name":w.definition.name,"description":w.definition.description,"steps":w.definition.steps.len()})).collect::<Vec<_>>()}),
                    )
                }
                _ => self.run(call, context).await,
            }
        })
    }
}
fn definition(value: &Value) -> Result<WorkflowDefinition, ToolExecutionError> {
    let mut definition = value.clone();
    definition
        .as_object_mut()
        .ok_or_else(|| failed("流程参数必须是对象"))?
        .remove("expected_revision");
    let definition: WorkflowDefinition = serde_json::from_value(definition).map_err(failed)?;
    definition.validate()?;
    Ok(definition)
}
impl WorkflowTool {
    fn validate(&self, value: &Value) -> Result<(), ToolExecutionError> {
        let object = value
            .as_object()
            .ok_or_else(|| failed("流程参数必须是对象"))?;
        if value.to_string().len() > crate::MAX_DEFINITION_BYTES {
            return Err(failed("流程参数超过上限"));
        }
        let allowed: &[&str] = match self.kind {
            0 => &["name", "description", "steps", "expected_revision"],
            1 => &["workflow_id", "run_id"],
            _ => &["workflow_id", "revision", "parameters"],
        };
        if object.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(failed("流程参数包含未知字段"));
        }
        if self.kind == 0 {
            definition(value)?;
            if object
                .get("expected_revision")
                .is_some_and(|r| r.as_u64().is_none())
            {
                return Err(failed("expected_revision 必须是非负整数"));
            }
        } else {
            for key in ["workflow_id", "run_id"] {
                if object
                    .get(key)
                    .is_some_and(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 100))
                {
                    return Err(failed("流程编号无效"));
                }
            }
            if self.kind == 1 && object.len() > 1 {
                return Err(failed("一次只能查询流程或一次运行"));
            }
            if self.kind == 2
                && (object.get("workflow_id").is_none()
                    || object
                        .get("revision")
                        .and_then(Value::as_u64)
                        .is_none_or(|n| n == 0)
                    || object.get("parameters").is_some_and(|v| !v.is_object()))
            {
                return Err(failed(
                    "执行流程需要 workflow_id、正整数 revision 和对象 parameters",
                ));
            }
        }
        Ok(())
    }
    fn check_leaves(&self, definition: &WorkflowDefinition) -> Result<(), ToolExecutionError> {
        let leaves = self
            .store
            .leaves
            .get()
            .ok_or_else(|| failed("流程叶子工具尚未装配"))?;
        for call in &definition.steps {
            if !LEAVES.contains(&call.name.as_str()) || !leaves.contains_key(&call.name) {
                return Err(failed(format!(
                    "流程不能调用未装配或非叶子工具 {}",
                    call.name
                )));
            }
            if !leaves[&call.name]
                .definition()
                .required_permissions
                .is_empty()
            {
                return Err(failed("流程不封装需要独立内核权限的工具"));
            }
        }
        Ok(())
    }
    async fn run(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        let scope = context.scope().ok_or_else(|| failed("流程需要可信身份"))?;
        let saved = self.store.get(
            scope,
            call.arguments["workflow_id"].as_str().unwrap_or_default(),
        )?;
        if call.arguments["revision"].as_u64() != Some(saved.revision) {
            return Err(failed("流程版本已变化；重新读取后明确选择版本"));
        }
        self.check_leaves(&saved.definition)?;
        let parameters = call
            .arguments
            .get("parameters")
            .cloned()
            .unwrap_or(json!({}));
        let leaves = self.store.leaves.get().expect("checked");
        let operations = saved
            .definition
            .steps
            .iter()
            .map(|step| {
                let arguments = expand(&step.arguments, &parameters)?;
                leaves[&step.name]
                    .validate_arguments(&arguments)
                    .map_err(|e| failed(format!("步骤 {} 参数错误：{e}", step.id)))?;
                Ok(ToolCall {
                    id: step.id.clone(),
                    name: step.name.clone(),
                    arguments,
                })
            })
            .collect::<Result<Vec<_>, ToolExecutionError>>()?;
        let budget = context
            .shared_steps()
            .ok_or_else(|| failed("缺少宿主共享步数，未执行流程"))?;
        if !budget.reserve(operations.len()).map_err(failed)? {
            return Err(failed("剩余工具步数不足，整个流程未执行"));
        }
        let id = self.store.begin_run()?;
        let mut run = json!({"version":1,"run_id":id,"workflow_id":saved.id,"revision":saved.revision,
            "owner":scope,"status":"running","completed":false,"steps":[]});
        self.store.write_run(&id, &run)?;
        for operation in operations {
            if context.is_cancel_requested()
                || context
                    .deadline()
                    .is_some_and(|d| d <= std::time::Instant::now())
            {
                run["status"] = json!("stopped");
                run["reason"] = json!("当前任务已取消或执行期限已到，后续步骤未执行");
                self.store.write_run(&id, &run)?;
                return Err(ToolExecutionError::Cancelled(format!(
                    "流程 {id} 已停止，已有步骤记录保留"
                )));
            }
            let steps = run["steps"].as_array_mut().expect("array");
            let index = steps.len();
            steps.push(json!({"id":operation.id,"tool":operation.name,"status":"unconfirmed"}));
            self.store.write_run(&id, &run).map_err(|_| {
                ToolExecutionError::Failed(format!(
                    "流程 {id} 不能保存下一步状态；已停止，需核对已有记录"
                ))
            })?;
            let tool = &leaves[&operation.name];
            let outcome = tool.execute(operation, context.clone()).await;
            let (status, result, unknown) = match outcome {
                Ok(value) => {
                    let success = value.get("success").and_then(Value::as_bool) != Some(false);
                    (if success { "returned" } else { "failed" }, value, false)
                }
                Err(ToolExecutionError::Rejected(message)) => {
                    ("rejected", json!({"error":message}), false)
                }
                Err(error) => ("unconfirmed", json!({"error":error.to_string()}), true),
            };
            run["steps"][index]["status"] = json!(status);
            // 流程账本保留有界摘要，不复制文件正文或命令参数。
            run["steps"][index]["result"] = brief_result(&result);
            if status != "returned" {
                run["status"] = json!(if unknown { "unknown" } else { "failed" });
                self.store.write_run(&id, &run).map_err(|_| {
                    ToolExecutionError::Failed(format!(
                        "流程 {id} 失败回执未保存；停止后续步骤，请核对原操作"
                    ))
                })?;
                if unknown {
                    return Err(ToolExecutionError::Failed(format!(
                        "流程 {id} 的步骤结果未确认，后续未执行；用 list_workflow_tools 的 run_id 查看原因并核对，不能直接重跑"
                    )));
                }
                return Ok(run);
            }
            self.store.write_run(&id, &run).map_err(|_| {
                ToolExecutionError::Failed(format!("流程 {id} 已执行但回执保存失败；未自动重跑"))
            })?;
        }
        run["status"] = json!("completed");
        run["completed"] = json!(true);
        self.store.write_run(&id, &run).map_err(|_| {
            ToolExecutionError::Failed(format!("流程 {id} 结局未确认保存；请核对记录，不能重跑"))
        })?;
        Ok(run)
    }
}
fn brief_result(value: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in [
        "success",
        "exit_code",
        "queued",
        "id",
        "name",
        "sha256",
        "bytes",
        "error",
        "stderr",
    ] {
        if let Some(value) = value.get(key) {
            let value = match value {
                Value::String(text) => Value::String(redacted(text)),
                Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
                _ => continue,
            };
            result.insert(key.into(), value);
        }
    }
    Value::Object(result)
}
fn redacted(text: &str) -> String {
    let sensitive = |s: &str| {
        let text = s.to_ascii_lowercase();
        [
            "token",
            "secret",
            "password",
            "credential",
            "authorization",
            "api_key",
            "api-key",
            "private key",
        ]
        .iter()
        .any(|key| text.contains(key))
    };
    let mut text = text.to_owned();
    for (key, value) in std::env::vars_os() {
        if let (Some(key), Some(value)) = (key.to_str(), value.to_str())
            && sensitive(key)
            && value.len() >= 8
        {
            text = text.replace(value, "[凭据已隐藏]");
        }
    }
    text.lines()
        .map(|line| {
            if sensitive(line) {
                "[可能含凭据的内容已隐藏]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(400)
        .collect()
}
