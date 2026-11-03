//! 条件唤醒只继承可信宿主冻结的真实用户原话，不从触发说明或会话摘要补造授权。
use eve_llm_api::{ToolExecutionContext, ToolExecutionError};
use eve_session_api::SessionKey;
use eve_trigger_api::{Trigger, validate_review_messages};

/// 在创建时按完整会话范围冻结最近六条正文；正文的空白和长度保持原样。
pub(crate) fn capture(
    context: &ToolExecutionContext,
    owner: &SessionKey,
) -> Result<Vec<String>, ToolExecutionError> {
    owner.validate().map_err(|_| scope_rejected())?;
    if context
        .scope()
        .is_none_or(|scope| scope.session_id != owner.session_id || scope.user_id != owner.user_id)
    {
        return Err(scope_rejected());
    }
    let originals = context.review_user_messages().ok_or_else(|| {
        ToolExecutionError::Rejected("触发器缺少可信用户授权原话；未创建自动唤醒".into())
    })?;
    let messages = &originals[originals.len().saturating_sub(6)..];
    validate_review_messages(messages).map_err(|_| {
        ToolExecutionError::Rejected(
            "触发器授权原话为空或超过上限；未裁剪正文、未创建自动唤醒".into(),
        )
    })?;
    Ok(messages.to_vec())
}

/// 恢复只读取触发器自身的冻结证据；旧记录缺失原话时失败关闭，不读取当前历史。
pub(crate) fn restore(trigger: &Trigger) -> Result<Vec<String>, ToolExecutionError> {
    trigger.owner.validate().map_err(|_| scope_rejected())?;
    let messages = trigger.review_messages.as_deref().ok_or_else(|| {
        ToolExecutionError::Rejected("触发器没有持久化授权原话；未执行自动唤醒".into())
    })?;
    validate_review_messages(messages).map_err(|_| {
        ToolExecutionError::Rejected("触发器持久化授权原话无效；未执行自动唤醒".into())
    })?;
    Ok(messages.to_vec())
}

fn scope_rejected() -> ToolExecutionError {
    ToolExecutionError::Rejected("触发器授权证据与可信会话不匹配；未执行自动唤醒".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, ChatRole, ContextScope, MAX_REVIEW_USER_MESSAGES_BYTES};
    use eve_trigger_api::{Condition, MatchMode, Status, TriggerSpec};
    use std::sync::Arc;

    fn owner() -> SessionKey {
        SessionKey::new("trusted-window", "trusted-owner").unwrap()
    }

    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: owner().session_id,
            user_id: owner().user_id,
        }))
    }

    fn trigger(messages: Option<Vec<String>>) -> Trigger {
        Trigger {
            id: "trigger-test".into(),
            owner: owner(),
            source: "trusted-call".into(),
            spec: TriggerSpec {
                title: "条件唤醒".into(),
                instructions: "模型写的触发说明不能当作用户授权".into(),
                mode: MatchMode::All,
                conditions: vec![Condition::Time { at_ms: 2 }],
                expires_at_ms: 3,
                recurrence: None,
                resume_task_id: None,
            },
            created_at_ms: 1,
            status: Status::Armed,
            evidence: vec!["观察结果也不是用户授权".into()],
            result: None,
            delivery: None,
            runs: 0,
            review_messages: messages,
        }
    }

    #[test]
    fn captures_last_six_complete_originals_including_whitespace() {
        let originals: Vec<_> = (0..8)
            .map(|index| {
                format!(
                    " \t第 {index} 条原话\n{}\n保留尾部\t ",
                    "授权范围 ".repeat(200)
                )
            })
            .collect();
        let execution = context()
            .with_review_user_messages(Arc::new(originals.clone()))
            .with_conversation(Arc::new(vec![ChatMessage::text(
                ChatRole::User,
                "模型生成的任务说明，不能替代用户原话",
            )]));
        assert_eq!(capture(&execution, &owner()).unwrap(), originals[2..]);
        assert_eq!(
            capture(&execution.with_delegated(true), &owner()).unwrap(),
            originals[2..]
        );
    }

    #[test]
    fn rejects_missing_originals_even_when_conversation_claims_authorization() {
        let execution = context().with_conversation(Arc::new(vec![ChatMessage::text(
            ChatRole::User,
            "普通会话上下文中的同意，不能补造专用槽的用户原话",
        )]));
        assert!(capture(&execution, &owner()).is_err());
        let execution = context().with_review_user_messages(Arc::new(vec![]));
        assert!(capture(&execution, &owner()).is_err());
        // 已验证附件消息可能只有空正文；不能删除、裁剪或改写这一真实消息。
        for messages in [vec![String::new()], vec![" \t\n".into()]] {
            let execution = context().with_review_user_messages(Arc::new(messages.clone()));
            assert_eq!(capture(&execution, &owner()).unwrap(), messages);
        }
    }

    #[test]
    fn rejects_absent_foreign_and_invalid_scope() {
        let execution = context().with_review_user_messages(Arc::new(vec!["原始授权".into()]));
        assert!(capture(&execution.clone().with_scope(None), &owner()).is_err());
        for foreign in [
            SessionKey::new("other-window", "trusted-owner").unwrap(),
            SessionKey::new("trusted-window", "other-owner").unwrap(),
        ] {
            assert!(capture(&execution, &foreign).is_err());
        }
        let invalid = SessionKey {
            session_id: " trusted-window".into(),
            user_id: "trusted-owner".into(),
        };
        let execution = execution.with_scope(Some(ContextScope {
            session_id: invalid.session_id.clone(),
            user_id: invalid.user_id.clone(),
        }));
        assert!(capture(&execution, &invalid).is_err());
    }

    #[test]
    fn rejects_body_and_json_encoding_overflow_instead_of_trimming() {
        for oversized in [
            "x".repeat(MAX_REVIEW_USER_MESSAGES_BYTES),
            "\\".repeat(MAX_REVIEW_USER_MESSAGES_BYTES / 2),
        ] {
            let originals = vec![oversized];
            let execution = context().with_review_user_messages(Arc::new(originals.clone()));
            assert!(capture(&execution, &owner()).is_err());
            assert!(restore(&trigger(Some(originals))).is_err());
        }
    }

    #[test]
    fn restores_only_complete_frozen_originals_and_rejects_invalid_records() {
        let originals = vec![" \t第一条完整授权\n保留尾部 ".into(), "第二条原话\n".into()];
        let record = trigger(Some(originals.clone()));
        let persisted = serde_json::to_vec(&record).unwrap();
        let restored: Trigger = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(restore(&restored).unwrap(), originals);
        // 旧记录没有新字段；仍能读取，但不能从说明或匹配证据恢复授权。
        let old_record = serde_json::to_vec(&trigger(None)).unwrap();
        let old_record: Trigger = serde_json::from_slice(&old_record).unwrap();
        assert!(restore(&old_record).is_err());
        for messages in [vec![], vec!["授权".into(); 7]] {
            assert!(restore(&trigger(Some(messages))).is_err());
        }
        for messages in [vec![String::new()], vec![" \n".into()]] {
            assert_eq!(restore(&trigger(Some(messages.clone()))).unwrap(), messages);
        }
        let mut invalid = record;
        invalid.owner.user_id = "invalid\nowner".into();
        assert!(restore(&invalid).is_err());
    }
}
