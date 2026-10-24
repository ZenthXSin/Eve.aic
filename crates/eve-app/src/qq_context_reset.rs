//! 按平台身份重置后续轮次的上下文；不删除历史、记忆或发送回执。
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_plugin_api::{ServiceId, ServiceRegistry};
use eve_qqbot_plugin::QqConversations;
use eve_session_api::{SESSION_SERVICE_ID, SessionKey, SessionServiceHandle};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) struct ResetTool {
    pub(crate) all: bool,
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    pub(crate) conversations: QqConversations,
    pub(crate) authorization: Arc<dyn DeveloperAuthorization>,
}
fn failed(message: &str) -> ToolExecutionError {
    ToolExecutionError::Failed(message.into())
}
impl Tool for ResetTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: if self.all { "clear_all_contexts" } else { "clear_context" }.into(),
            description: if self.all {
                "仅在主人或管理员明确要求清空所有会话上下文时调用。将全部现有会话的后续轮次上下文重置为空；权限由宿主按实际发送者核对，不接受指定身份。不会删除原始历史、长期记忆、数据库、配置或工具回执，不中断正在运行的轮次；清空边界包含这些轮次，重启后仍生效。完成后简短告知，结束本轮。"
            } else {
                "在用户明确要求清空当前会话上下文、重新开始聊天时调用。所有用户均可使用，但只重置当前实际发送者在当前会话的后续轮次上下文，不能指定其他用户或会话。不会删除原始历史、长期记忆、数据库、配置或工具回执；本轮结束后开始干净上下文，重启后仍生效。完成后简短告知，结束本轮。"
            }.into(),
            argument_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.context.reset".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_some_and(|object| object.is_empty()) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "清空上下文不接受参数；目标由可信会话身份决定".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|e| failed(&e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
            }
            let scope = context
                .scope()
                .ok_or_else(|| failed("清空上下文需要可信平台身份"))?;
            let key = SessionKey::new(&scope.session_id, &scope.user_id)
                .map_err(|_| failed("会话身份无效"))?;
            if self
                .conversations
                .identity(&key)
                .map_err(|_| failed("平台身份不可用"))?
                .is_none()
                || scope.user_id == crate::qq_group::USER
            {
                return Err(failed("只允许真实用户会话清空上下文"));
            }
            if self.all && !self.authorization.is_operator(scope) {
                return Err(failed("全局清空上下文仅限主人或管理员"));
            }
            let service = self
                .registry
                .get(&ServiceId::new(SESSION_SERVICE_ID).map_err(|_| failed("会话服务不可用"))?)
                .map_err(|_| failed("会话服务不可用"))?
                .ok_or_else(|| failed("会话服务未启动"))?;
            let service = service
                .value
                .downcast::<SessionServiceHandle>()
                .map_err(|_| failed("会话服务类型错误"))?;
            let affected = service
                .0
                .reset_context(if self.all { None } else { Some(&key) })
                .map_err(|_| failed("上下文重置未确认；原始记录保留，不能声称已成功"))?;
            Ok(
                json!({"cleared":true,"scope":if self.all {"all"} else {"current_session_user"},
                "affected_sessions":affected,"effective":"next_turn","raw_history_preserved":true,"memory_preserved":true,
                "note":"已保存清空边界，本轮结束后生效；简短告知用户并结束本轮"}),
            )
        })
    }
}
