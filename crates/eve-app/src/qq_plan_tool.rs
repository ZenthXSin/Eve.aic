//! 模型只选择有类型的操作，现有计划服务负责持久化、原目标绑定和步骤执行。
use super::*;
use crate::qq_natural_goals::{Review, failed, session};
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolFuture,
    ToolValidationError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::OnceLock;

#[cfg(test)]
#[path = "qq_plan_tool_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    goal_id: String,
    plan_id: Option<String>,
    expected_revision: Option<u64>,
}

fn args(value: &Value) -> Result<Args, ToolValidationError> {
    let invalid = || {
        ToolValidationError { message: "需要inspect/propose(goal_id)或confirm/withdraw(goal_id/plan_id/expected_revision)，不接受身份或确认标记".into() }
    };
    let a: Args = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    if validate_id(&a.goal_id).is_err()
        || a.plan_id
            .as_deref()
            .is_some_and(|id| validate_id(id).is_err())
    {
        return Err(invalid());
    }
    let valid = match a.action.as_str() {
        "inspect" | "propose" => a.plan_id.is_none() && a.expected_revision.is_none(),
        "confirm" | "withdraw" => {
            a.plan_id.is_some() && a.expected_revision.is_some_and(|revision| revision > 0)
        }
        _ => false,
    };
    if !valid {
        return Err(invalid());
    }
    Ok(a)
}

pub(crate) struct ManageTool {
    pub(crate) commands: Arc<OnceLock<Arc<Commands>>>,
    pub(crate) review: Review,
}

impl Tool for ManageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_plan".into(),
            description: "自然聊天查看、建议、确认或撤销当前用户待办的多步计划。inspect取得实际计划编号、当前版本和全部步骤；propose只请求建议，不执行步骤。confirm/withdraw必须使用刚读取的plan_id与expected_revision，不能用旧建议替代当前目标；confirm的完整步骤和用户原话独立送审，确认后由原后台按依赖验证执行。计划各步满足不等于用户现实目标完成。已有业务授权和停止规则继续适用，无需用户输入内部命令。".into(),
            argument_schema: json!({"type":"object","properties":{"action":{"enum":["inspect","propose","confirm","withdraw"]},"goal_id":{"type":"string"},"plan_id":{"type":"string"},"expected_revision":{"type":"integer","minimum":1}},"required":["action","goal_id"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "natural-plans".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        args(value).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let a = args(&call.arguments).map_err(|e| failed(e.message))?;
            self.review.authorize("manage_plan", &call, &context)?;
            let commands = self
                .commands
                .get()
                .ok_or_else(|| failed("计划服务尚未就绪"))?;
            let enabled = commands
                .enabled
                .as_ref()
                .ok_or_else(|| failed("计划服务没有装配可用能力"))?;
            let session = session(&context)?;
            let goal = enabled
                .goal(&session.user_id, &a.goal_id)
                .map_err(failed)?
                .ok_or_else(|| failed("没有属于当前可信用户的这件待办"))?;
            let snapshot = enabled.snapshot().map_err(failed)?;
            let plans: Vec<_> = snapshot
                .plans
                .iter()
                .filter(|plan| plan.binding.goal_id == goal.id)
                .cloned()
                .collect();
            if a.action == "inspect" {
                let proposals: Vec<_> = snapshot
                    .proposals
                    .into_iter()
                    .filter(|p| p.binding.goal_id == goal.id)
                    .collect();
                return Ok(
                    json!({"goal":goal,"plans":plans,"proposals":proposals,"goal_completed":goal.completion.is_some()}),
                );
            }
            let plan = if a.action == "propose" {
                None
            } else {
                Some(
                    plans
                        .iter()
                        .find(|plan| {
                            Some(&plan.id) == a.plan_id.as_ref()
                                && Some(plan.revision) == a.expected_revision
                                && plan.is_open()
                        })
                        .cloned()
                        .ok_or_else(|| failed("计划编号或版本已变化，请先查看当前计划"))?,
                )
            };
            if a.action == "confirm"
                && (goal.status != GoalStatus::Waiting
                    || plan.as_ref().is_none_or(|plan| {
                        plan.status != PlanStatus::Proposed || plan.binding != binding(&goal)
                    }))
            {
                return Err(failed("计划尚未待确认或已经过时，未开始执行"));
            }
            self.review.approve("manage_plan", &call, &context,
                json!({"arguments":call.arguments,"goal":goal,"plan":plan,"capabilities":enabled.capabilities})).await?;
            let current = enabled
                .goal(&session.user_id, &goal.id)
                .map_err(failed)?
                .ok_or_else(|| failed("审核期间待办已不可用"))?;
            if current != goal {
                return Err(failed("审核期间待办发生变化，未执行旧计划操作"));
            }
            let at_ms = now_ms().map_err(failed)?;
            if a.action == "propose" {
                if goal.status != GoalStatus::Waiting || enabled.capabilities.is_empty() {
                    return Err(failed("当前待办状态或可用能力不支持生成计划"));
                }
                enabled
                    .propose(&session.user_id, &goal.id)
                    .map_err(failed)?;
                let proposal = enabled
                    .snapshot()
                    .map_err(failed)?
                    .proposals
                    .into_iter()
                    .find(|proposal| proposal.binding == binding(&goal))
                    .ok_or_else(|| failed("计划建议请求未保存，请查看现有计划与正在执行的步骤"))?;
                return Ok(
                    json!({"proposal":proposal,"steps_started":false,"goal_completed":false}),
                );
            }
            let plan = plan.ok_or_else(|| failed("计划不存在"))?;
            let current_plan = enabled
                .snapshot()
                .map_err(failed)?
                .plans
                .into_iter()
                .find(|p| p.id == plan.id);
            if current_plan.as_ref() != Some(&plan) {
                return Err(failed("审核期间计划发生变化，原状态保留"));
            }
            let updated = if a.action == "confirm" {
                enabled
                    .plans
                    .confirm(&plan.id, plan.revision, &binding(&goal), at_ms)
            } else {
                enabled.plans.withdraw(&plan.id, plan.revision, at_ms)
            }
            .map_err(failed)?;
            Ok(json!({"changed":true,"plan":updated,"goal_completed":false}))
        })
    }
}
