//! 委派审核证据只继承可信宿主冻结的真实用户原话，不从模型任务说明或历史摘要提取。
use eve_llm_api::{ContextScope, ToolExecutionContext, ToolExecutionError};
use eve_task_api::{Task, TaskOwner, validate_review_messages};

fn rejected() -> ToolExecutionError {
    ToolExecutionError::Rejected("委派授权证据与可信会话不匹配；没有派发任务".into())
}

/// 与 include_context 无关：普通上下文可关闭，但宿主已采集的授权原话仍须独立冻结。
/// 子代理只能继承父执行者的同一专用槽；委派任务正文从未被提升为用户原话。
pub(crate) fn capture(
    context: &ToolExecutionContext,
    owner: &TaskOwner,
) -> Result<Vec<String>, ToolExecutionError> {
    eve_task_api::validate_owner(owner).map_err(|_| rejected())?;
    if context
        .scope()
        .is_none_or(|scope| scope.session_id != owner.session_id || scope.user_id != owner.user_id)
    {
        return Err(rejected());
    }
    let originals = context.review_user_messages().unwrap_or_default();
    let messages = &originals[originals.len().saturating_sub(6)..];
    validate_review_messages(messages).map_err(|_| {
        ToolExecutionError::Rejected("最近六条审核原话超过上限；未裁剪、未派发任务".into())
    })?;
    Ok(messages.to_vec())
}

/// 恢复时只读取同一完整任务里的原始证据；旧任务无证据时保持空，绝不从检查点补造。
pub(crate) fn restore(task: &Task, owner: &TaskOwner) -> Result<Vec<String>, ToolExecutionError> {
    eve_task_api::validate_owner(owner).map_err(|_| rejected())?;
    if task.owner != *owner {
        return Err(rejected());
    }
    let messages = task
        .execution
        .as_ref()
        .map_or(&[][..], |execution| execution.review_messages.as_slice());
    validate_review_messages(messages)
        .map_err(|_| ToolExecutionError::Rejected("持久化审核原话无效；未执行恢复任务".into()))?;
    Ok(messages.to_vec())
}

pub(crate) fn attach(
    host: eve_runtime::LlmHost,
    scope: Option<&ContextScope>,
    messages: Vec<String>,
) -> Result<eve_runtime::LlmHost, eve_llm_api::LlmError> {
    match scope {
        Some(scope) => host.with_delegated_review_messages(scope.clone(), messages),
        None if messages.is_empty() => Ok(host.delegated()),
        None => Err(eve_llm_api::LlmError::Context(
            "没有可信会话，不能继承委派授权原话".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, ChatRole};
    use std::sync::Arc;

    fn owner() -> TaskOwner {
        TaskOwner {
            session_id: "trusted-window".into(),
            user_id: "trusted-owner".into(),
        }
    }
    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: owner().session_id,
            user_id: owner().user_id,
        }))
    }

    #[test]
    fn captures_six_complete_originals_without_recovering_authorization_from_prompt() {
        let originals: Vec<_> = (0..8)
            .map(|index| format!("第 {index} 条原话\n{}\n保留尾部", "授权范围 ".repeat(500)))
            .collect();
        let execution = context()
            .with_review_user_messages(Arc::new(originals.clone()))
            .with_conversation(Arc::new(vec![ChatMessage::text(
                ChatRole::User,
                "任务模型编写的指令，不是真实授权",
            )]));
        assert_eq!(capture(&execution, &owner()).unwrap(), originals[2..]);
        assert_eq!(
            capture(&execution.clone().with_delegated(true), &owner()).unwrap(),
            originals[2..]
        );
        let without_originals = context().with_conversation(Arc::new(vec![ChatMessage::text(
            ChatRole::User,
            "只存在于普通上下文中的同意",
        )]));
        assert!(capture(&without_originals, &owner()).unwrap().is_empty());
    }

    #[test]
    fn rejects_scope_substitution_and_oversized_original_instead_of_trimming() {
        let mut foreign = owner();
        foreign.session_id = "another-window".into();
        assert!(capture(&context(), &foreign).is_err());
        let context = context().with_review_user_messages(Arc::new(vec![
            "x".repeat(eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES),
        ]));
        assert!(capture(&context, &owner()).is_err());
    }
}
