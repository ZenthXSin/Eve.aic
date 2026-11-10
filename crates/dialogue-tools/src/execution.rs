use eve_llm_api::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// 可取消的异步等待，不阻塞运行线程，不创建后台重放。
pub struct SleepTool;
impl Tool for SleepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "sleep".into(),
            description: "等待 1 至 300 秒后在本轮继续工作；可由用户取消。用于短暂等待后检查状态；长时间等待或事件监听应设置唤醒触发器，不要反复 sleep 轮询。".into(),
            argument_schema: json!({"type":"object","properties":{"seconds":{"type":"integer","minimum":1,"maximum":300}},"required":["seconds"],"additionalProperties":false}),
            required_permissions: vec![], concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), ToolValidationError> {
        if args.as_object().is_none_or(|o| o.len() != 1)
            || args["seconds"]
                .as_u64()
                .is_none_or(|s| !(1..=300).contains(&s))
        {
            return Err(ToolValidationError {
                message: "seconds 必须为 1 至 300 的整数，不接受额外字段".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|e| ToolExecutionError::Failed(e.message))?;
            let duration = Duration::from_secs(call.arguments["seconds"].as_u64().unwrap());
            let start = Instant::now();
            loop {
                if context.is_cancel_requested() {
                    return Err(ToolExecutionError::Cancelled("等待已取消".into()));
                }
                let Some(left) = duration.checked_sub(start.elapsed()) else {
                    break;
                };
                tokio::time::sleep(left.min(Duration::from_millis(25))).await;
            }
            Ok(
                json!({"waited":true,"elapsed_ms":start.elapsed().as_millis(),"next":"继续本轮尚未完成的任务，必要时调用工具检查实际状态"}),
            )
        })
    }
}

/// 显式任务状态驱动宿主收尾；普通聊天无需登记，委派观察可采用其自述。
pub struct TaskProgressTool;
impl Tool for TaskProgressTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name:"task_progress".into(),
            description:"按需记录值得保留的任务阶段、恢复信息或实际阻塞，不要求每个多步任务固定登记开头和收尾。登记 in_progress 时写明真实剩余工作；采用后仍须继续推进，工作完成并验证后更新 completed，缺少权限、信息或确实无法继续时更新 blocked，说明已有成果、实际阻塞和需要的决定，不能把未完成事项标记 completed。后台 blocked 会保存已有成果并通知原用户；用户待决定或信息不全时保持阻塞。若已单独 register_task_wait 登记文件或进程技术条件，remaining 完整使用登记回执原文，随后结束本轮；宿主核对条件和真实回执后接续原任务，保留原授权、预算和期限。仍登记为 in_progress 而模型提前收尾时，宿主会继续本轮，最多纠正三次；不会增加工具预算。进度登记不代替冻结验收合同，普通聊天无需调用。子代理可按需用它向原会话交回阶段说明；宿主仅转交为子代理自述，不代表已验证或已经向用户发送。".into(),
            argument_schema:json!({"type":"object","properties":{"status":{"type":"string","enum":["in_progress","completed","blocked"]},"remaining":{"type":"string","minLength":1,"maxLength":2048}},"required":["status","remaining"],"additionalProperties":false}),
            required_permissions:vec![], concurrency:Some(ToolConcurrency::Serial{scope:"eve.task-progress".into()}),
        }
    }
    fn validate_arguments(&self, a: &Value) -> Result<(), ToolValidationError> {
        if a.as_object().is_none_or(|o| o.len() != 2)
            || !matches!(
                a["status"].as_str(),
                Some("in_progress" | "completed" | "blocked")
            )
            || a["remaining"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty() || s.len() > 2048)
        {
            return Err(ToolValidationError {
                message: "需要 status 与非空 remaining（至多 2048 字节）；不接受额外字段".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|e| ToolExecutionError::Failed(e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("任务已取消".into()));
            }
            let pending = call.arguments["status"] == "in_progress";
            context.continuation().set_remaining(
                pending.then(|| call.arguments["remaining"].as_str().unwrap().into()),
            )?;
            Ok(
                json!({"status":call.arguments["status"],"remaining":call.arguments["remaining"],"verified_by_host":false}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(args: Value) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: "sleep".into(),
            arguments: args,
        }
    }
    #[tokio::test]
    async fn sleep_waits_and_cooperates_with_cancellation() {
        let t = Instant::now();
        SleepTool
            .execute(call(json!({"seconds":1})), ToolExecutionContext::new())
            .await
            .unwrap();
        assert!(t.elapsed() >= Duration::from_secs(1));
        let c = ToolExecutionContext::new();
        let cancel = c.cancellation_handle();
        let task =
            tokio::spawn(async move { SleepTool.execute(call(json!({"seconds":300})), c).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(ToolExecutionError::Cancelled(_))
        ));
        for args in [
            json!({"seconds":0}),
            json!({"seconds":301}),
            json!({"seconds":1,"owner":"admin"}),
        ] {
            assert!(SleepTool.validate_arguments(&args).is_err());
        }
    }
}
