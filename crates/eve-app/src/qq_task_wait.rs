//! 当前后台任务的技术等待。模型只提供观察条件，身份与接续目标由宿主绑定。
//! 条件与调用来源先写入触发账本；最后的完整工具回执确认 Blocked 后才可接续。
use crate::qq_triggers::Observations;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolOutput, ToolValidationError,
};
use eve_session_api::SessionKey;
use eve_task_api::{Task, TaskAdmin, TaskCompletionContract, TaskOwner, TaskStatus};
use eve_trigger_api::{Condition, MatchMode, Status, Trigger, TriggerAdmin, TriggerSpec};
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

pub(crate) const TOOL_NAME: &str = "register_task_wait";
const SOURCE_PREFIX: &str = "task-wait:";
const MAX_REASON_BYTES: usize = 1200;

pub(crate) struct WaitTool {
    pub(crate) tasks: Arc<dyn TaskAdmin>,
    pub(crate) triggers: Arc<dyn TriggerAdmin>,
    pub(crate) observations: Arc<OnceLock<Arc<Observations>>>,
}

fn failed(message: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Rejected(message.to_string())
}
fn invalid(message: &str) -> ToolValidationError {
    ToolValidationError {
        message: message.into(),
    }
}
fn source(task_id: &str, call_id: &str) -> String {
    let bytes = serde_json::to_vec(&(task_id, call_id)).expect("strings serialize");
    let hash: String = digest(&SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{SOURCE_PREFIX}{hash}")
}
fn remaining(trigger: &Trigger) -> String {
    format!("技术等待 {}：{}", trigger.id, trigger.spec.instructions)
}
fn receipt(trigger: &Trigger) -> Value {
    json!({
        "registered": true,
        "kind": "technical_task_wait",
        "trigger_id": trigger.id,
        "task_id": trigger.spec.resume_task_id,
        "condition": trigger.spec.conditions[0],
        "expires_at_ms": trigger.spec.expires_at_ms,
        "remaining": remaining(trigger),
        "next": "立即单独调用 task_progress，status=blocked，remaining 完整使用本回执的 remaining，然后结束本轮。条件只表示可以核对并继续原任务，不证明业务成功，也不增加授权。"
    })
}
fn receipt_matches(output: &Value, trigger: &Trigger) -> bool {
    let expected = receipt(trigger);
    // 文案可随升级修正；持久等待只绑定稳定的身份、观察条件和精确阻塞标记。
    [
        "registered",
        "kind",
        "trigger_id",
        "task_id",
        "condition",
        "expires_at_ms",
        "remaining",
    ]
    .iter()
    .all(|field| output.get(*field) == expected.get(*field))
}
fn validate_arguments(value: &Value) -> Result<(), ToolValidationError> {
    let args = value.as_object().ok_or_else(|| invalid("参数必须为对象"))?;
    if args.len() != 2 || !args.contains_key("condition") || !args.contains_key("reason") {
        return Err(invalid(
            "仅接受 condition 和 reason，不接受任务、身份或预算参数",
        ));
    }
    if args["reason"].as_str().is_none_or(|reason| {
        reason.trim().is_empty()
            || reason.trim() != reason
            || reason.len() > MAX_REASON_BYTES
            || reason.contains('\0')
    }) {
        return Err(invalid(
            "reason 需要 1 至 1200 字节，说明技术条件与剩余任务",
        ));
    }
    let condition = args["condition"]
        .as_object()
        .ok_or_else(|| invalid("condition 无效"))?;
    let valid = condition.len() == 2
        && match condition.get("type").and_then(Value::as_str) {
            Some("file_changed") => {
                condition
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| {
                        !path.trim().is_empty()
                            && path.trim() == path
                            && path.len() <= 4096
                            && !path.contains('\0')
                    })
            }
            Some("process_exit") => condition
                .get("pid")
                .and_then(Value::as_u64)
                .is_some_and(|pid| pid > 0 && u32::try_from(pid).is_ok()),
            _ => false,
        };
    if !valid {
        return Err(invalid(
            "condition 仅支持 file_changed/path 或 process_exit/pid",
        ));
    }
    Ok(())
}

fn registration_task(
    tasks: &dyn TaskAdmin,
    call: &ToolCall,
    context: &ToolExecutionContext,
    now: u64,
) -> Result<Task, ToolExecutionError> {
    if context.is_cancel_requested() {
        return Err(ToolExecutionError::Cancelled("任务已取消".into()));
    }
    if !context.is_delegated() || context.call_id() != Some(call.id.as_str()) {
        return Err(failed("技术等待只供可信后台执行器的当前调用使用"));
    }
    let scope = context.scope().ok_or_else(|| failed("缺少可信任务身份"))?;
    let id = context
        .background_task_id()
        .ok_or_else(|| failed("没有宿主绑定的当前后台任务"))?;
    let owner = TaskOwner {
        session_id: scope.session_id.clone(),
        user_id: scope.user_id.clone(),
    };
    let task = tasks
        .get_owned(&owner, id)
        .map_err(failed)?
        .ok_or_else(|| failed("当前后台任务不存在"))?;
    if task.owner != owner
        || task.id != id
        || task.status != TaskStatus::Running
        || task.cancel_requested
    {
        return Err(failed("当前后台任务已经停止或身份不匹配"));
    }
    let execution = task
        .execution
        .as_ref()
        .ok_or_else(|| failed("任务缺少持久执行状态"))?;
    let frame = execution
        .checkpoint
        .as_ref()
        .ok_or_else(|| failed("任务缺少持久检查点"))?;
    if execution.completion_contract.is_none()
        || matches!(
            execution.completion_contract,
            Some(TaskCompletionContract::Text { .. })
        )
        || execution.recoveries >= eve_task_api::MAX_AUTO_RECOVERIES
        || execution.deadline_at_ms.is_some_and(|at| at <= now)
        || execution
            .authorization_deadline_at_ms
            .is_some_and(|at| at <= now)
        || execution.resume_at_ms.is_some()
        || frame.closing
        || frame.scope != *scope
        || frame
            .used_tool_calls
            .saturating_add(frame.delegated_tool_calls)
            .saturating_add(1)
            >= task.spec.max_steps as usize
        || frame.pending_calls.as_slice() != std::slice::from_ref(call)
    {
        return Err(failed(
            "任务无法安全等待：必须单独调用、保留原期限和接续步数，且不得处于收尾或恢复上限",
        ));
    }
    let mut settled = frame.clone();
    settled.pending_calls.clear();
    if !settled.resumable() {
        return Err(failed("已有工具效果未知，不能登记自动接续"));
    }
    Ok(task)
}

impl Tool for WaitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: TOOL_NAME.into(),
            description: "当前后台任务因文件尚未变化或真实进程尚未退出而暂时不能推进时，单独调用本工具登记技术等待。condition=file_changed 使用工程相对 path；启用宿主文件访问的操作者也可用绝对 path；condition=process_exit 使用仍在运行的 pid，宿主捕获进程实例。reason 写清等待原因和剩余工作。成功后必须立即单独 task_progress(status=blocked,remaining=本回执原文) 并结束本轮；宿主在条件满足后接续同一任务，保留原授权、验收标准、工具预算和期限。缺少用户决定、授权或信息不能用技术条件冒充解除；未知工具副作用不自动重放。此工具不创建新任务，不发送消息，不执行命令。".into(),
            argument_schema: json!({"type":"object","properties":{
                "reason":{"type":"string","minLength":1,"maxLength":MAX_REASON_BYTES},
                "condition":{"oneOf":[
                    {"type":"object","properties":{"type":{"const":"file_changed"},"path":{"type":"string"}},"required":["type","path"],"additionalProperties":false},
                    {"type":"object","properties":{"type":{"const":"process_exit"},"pid":{"type":"integer","minimum":1,"maximum":u32::MAX}},"required":["type","pid"],"additionalProperties":false}
                ]}
            },"required":["condition","reason"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.triggers".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        validate_arguments(value)
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            validate_arguments(&call.arguments).map_err(|error| failed(error.message))?;
            if call.name != TOOL_NAME {
                return Err(failed("技术等待调用名称不匹配"));
            }
            let now = crate::qq_triggers::now_ms();
            let task = registration_task(self.tasks.as_ref(), &call, &context, now)?;
            let owner =
                SessionKey::new(&task.owner.session_id, &task.owner.user_id).map_err(failed)?;
            let source = source(&task.id, &call.id);
            let records = self.triggers.snapshot().map_err(failed)?;
            if let Some(existing) = records
                .iter()
                .find(|trigger| trigger.owner == owner && trigger.source == source)
            {
                if !matches!(existing.status, Status::Armed | Status::Ready)
                    || existing.spec.expires_at_ms <= now
                    || !registration_matches(&call, existing)
                {
                    return Err(failed("同一调用已有不同或已了结的等待记录"));
                }
                return Ok(receipt(existing));
            }
            if records.iter().any(|trigger| {
                trigger.owner == owner
                    && trigger.spec.resume_task_id.as_deref() == Some(task.id.as_str())
                    && matches!(
                        trigger.status,
                        Status::Armed | Status::Ready | Status::Running
                    )
            }) {
                return Err(failed("当前任务已有尚未了结的条件等待"));
            }
            let observations = self
                .observations
                .get()
                .ok_or_else(|| failed("条件观察服务未就绪"))?;
            let condition = &call.arguments["condition"];
            let condition = if condition["type"] == "file_changed" {
                let path = condition["path"].as_str().expect("validated path");
                Condition::FileChanged {
                    path: path.into(),
                    baseline: observations.file(&owner, path)?,
                }
            } else {
                let pid = condition["pid"].as_u64().expect("validated pid") as u32;
                let identity = observations.process(&owner, pid)?;
                if identity == "missing" {
                    return Err(failed(
                        "该进程已经退出；请直接核对结果，不能登记不存在的进程实例",
                    ));
                }
                Condition::ProcessState {
                    pid,
                    running: false,
                    identity,
                }
            };
            let execution = task.execution.as_ref().expect("validated execution");
            let expires_at_ms = [
                execution.deadline_at_ms,
                execution.authorization_deadline_at_ms,
            ]
            .into_iter()
            .flatten()
            .fold(now.saturating_add(7 * 86_400_000), u64::min);
            let trigger = self
                .triggers
                .create(
                    owner,
                    source,
                    TriggerSpec {
                        title: eve_task_api::bounded(
                            &format!("技术等待：{}", task.spec.title),
                            160,
                        ),
                        instructions: call.arguments["reason"]
                            .as_str()
                            .expect("validated reason")
                            .into(),
                        mode: MatchMode::All,
                        conditions: vec![condition],
                        expires_at_ms,
                        recurrence: None,
                        resume_task_id: Some(task.id),
                    },
                    now,
                )
                .map_err(|error| ToolExecutionError::Failed(error.to_string()))?;
            Ok(receipt(&trigger))
        })
    }
}

fn registration_matches(call: &ToolCall, trigger: &Trigger) -> bool {
    if call.name != TOOL_NAME
        || validate_arguments(&call.arguments).is_err()
        || call.arguments["reason"].as_str() != Some(trigger.spec.instructions.as_str())
    {
        return false;
    }
    let condition = &call.arguments["condition"];
    match trigger.spec.conditions.as_slice() {
        [Condition::FileChanged { path, .. }] => {
            condition["type"] == "file_changed" && condition["path"].as_str() == Some(path.as_str())
        }
        [
            Condition::ProcessState {
                pid,
                running: false,
                identity,
            },
        ] => {
            condition["type"] == "process_exit"
                && condition["pid"].as_u64() == Some(u64::from(*pid))
                && identity != "missing"
        }
        _ => false,
    }
}

/// None 为普通前台创建的条件；false 是自登记技术等待但最终回执不再证明可以自动接续。
pub(crate) fn confirmed_wait(trigger: &Trigger, task: &Task) -> Option<bool> {
    if !trigger.source.starts_with(SOURCE_PREFIX) {
        return None;
    }
    Some((|| {
        if trigger.spec.resume_task_id.as_deref() != Some(task.id.as_str())
            || trigger.owner.session_id != task.owner.session_id
            || trigger.owner.user_id != task.owner.user_id
            || task.status != TaskStatus::Blocked
        {
            return false;
        }
        let Some(frame) = task
            .execution
            .as_ref()
            .and_then(|execution| execution.checkpoint.as_ref())
        else {
            return false;
        };
        if !frame.resumable()
            || frame.closing
            || frame.result.is_none()
            || frame.validate(task.spec.max_steps as usize).is_err()
        {
            return false;
        }
        let calls: Vec<_> = frame
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .collect();
        let [.., registration, progress] = calls.as_slice() else {
            return false;
        };
        if progress.name != "task_progress"
            || progress.arguments != json!({"status":"blocked","remaining":remaining(trigger)})
            || source(&task.id, &registration.id) != trigger.source
            || !registration_matches(registration, trigger)
        {
            return false;
        }
        // 必须先收到登记回执，再单独标记阻塞；同一并行批次或随后追加工具均不能自动接续。
        let mut registration_received = false;
        let mut progress_received = false;
        for message in &frame.messages {
            // 技术阻塞之后若已有宿主/用户新一轮输入，这份旧登记不再证明当前等待。
            if progress_received && message.role == eve_llm_api::ChatRole::User {
                return false;
            }
            if message.tool_calls.iter().any(|call| call.id == progress.id)
                && (!registration_received || message.tool_calls.len() != 1)
            {
                return false;
            }
            for result in &message.tool_results {
                if result.call_id == registration.id {
                    registration_received = matches!(&result.output, ToolOutput::Success(output) if receipt_matches(output, trigger));
                }
                if result.call_id == progress.id {
                    progress_received = true;
                }
            }
        }
        registration_received && frame.messages.iter().flat_map(|message| &message.tool_results).any(|result| {
            result.call_id == progress.id && matches!(&result.output, ToolOutput::Success(output)
                if output["status"] == "blocked" && output["remaining"].as_str() == Some(remaining(trigger).as_str()))
        })
    })())
}
