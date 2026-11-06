//! 自然待办与计划入口：宿主核对实时身份并持久记录动作，不进行目标收益或逐步授权审查。
use crate::qq_admin::Admin;
use crate::qq_business::{HOST_AUTHORIZED, HostOperation};
use eve_llm_api::{ToolCall, ToolExecutionContext, ToolExecutionError};
use eve_session_api::SessionKey;
use serde_json::Value;
use std::sync::Arc;

#[cfg(test)]
#[path = "qq_natural_goals_tests.rs"]
pub(crate) mod tests;

pub(crate) fn failed(message: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Rejected(message.to_string())
}

pub(crate) fn session(context: &ToolExecutionContext) -> Result<SessionKey, ToolExecutionError> {
    let scope = context
        .scope()
        .ok_or_else(|| failed("需要宿主绑定的当前会话"))?;
    SessionKey::new(&scope.session_id, &scope.user_id).map_err(failed)
}

pub(crate) fn input_id(
    call: &ToolCall,
    context: &ToolExecutionContext,
) -> Result<String, ToolExecutionError> {
    if context.call_id() != Some(call.id.as_str())
        || context.turn_id().is_none_or(|id| id == 0)
        || call.id.trim().is_empty()
        || call.id.len() > 1024
        || call.id.chars().any(char::is_control)
    {
        return Err(failed("缺少宿主绑定的轮次和调用编号"));
    }
    Ok(format!(
        "natural:{}:{}",
        context.turn_id().unwrap(),
        eve_task_api::task_strategy_digest(&call.id)
    ))
}

/// 保存身份通过时的完整动作与范围，供计划恢复核对；不授予任何新的业务权限。
pub(crate) struct Review {
    pub(crate) admin: Arc<Admin>,
}

impl Review {
    pub(crate) fn authorize(
        &self,
        tool: &str,
        call: &ToolCall,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        crate::qq_access::authorize_tool(&self.admin, false, tool, Some(&call.arguments), context)?;
        if tool != call.name {
            return Err(failed("实际工具与宿主调用不一致"));
        }
        if context.is_delegated() && tool == "manage_goal" && call.arguments["action"] == "feedback"
        {
            return Err(failed(
                "用户反馈须由当前用户会话提交；后台说明不能冒充用户本人的反馈",
            ));
        }
        context.check_review_evidence()?;
        input_id(call, context)?;
        Ok(())
    }

    pub(crate) fn approve(
        &self,
        tool: &str,
        call: &ToolCall,
        context: &ToolExecutionContext,
        evidence: Value,
    ) -> Result<(), ToolExecutionError> {
        self.authorize(tool, call, context)?;
        let scope = context.scope().ok_or_else(|| failed("缺少可信范围"))?;
        if evidence["arguments"] != call.arguments {
            return Err(failed("记录动作与实际调用参数不一致"));
        }
        let operation = HostOperation {
            version: 1,
            tool: tool.into(),
            operation: evidence,
            trusted_scope: scope.clone(),
            turn_id: context.turn_id().ok_or_else(|| failed("缺少可信轮次"))?,
            call_id: call.id.clone(),
            delegated: context.is_delegated(),
        };
        let revision = self.admin.business.revision().map_err(failed)?;
        self.authorize(tool, call, context)?;
        // 与业务账本共用原子持久化；真实原话仍留在通道账本，不复制任务说明为授权证据。
        self.admin
            .business
            .mutate(crate::qq_business::Mutation {
                scope: scope.clone(),
                actor: scope.user_id.clone(),
                action: tool.into(),
                target: Some(serde_json::to_string(&operation).map_err(failed)?),
                at_ms: crate::qq_tasks::now_ms(),
                turn_id: context.turn_id(),
                user_messages: vec![],
                verdict: None,
                outcome: HOST_AUTHORIZED.into(),
                expected_revision: revision,
            })
            .map_err(failed)?;
        self.authorize(tool, call, context)
    }
}
