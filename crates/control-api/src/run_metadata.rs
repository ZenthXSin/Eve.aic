//! 可信宿主单独提交的运行证据；故意不实现 Serialize / Deserialize。
use crate::{ControlError, ControlInput, ControlResult};
use eve_llm_api::{ContextScope, MAX_REVIEW_USER_MESSAGES_BYTES};
use eve_session_api::SessionKey;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlReviewEvidence {
    /// 通道验证并持久化后的具体消息标识；正文必须由可信原文源按标识读取。
    ChannelAnchor(String),
    /// 创建委派时冻结的完整原话；任务说明和当前会话新消息不能补作授权。
    FrozenDelegation(Vec<String>),
}

/// 不从模型参数或用户文字创建，只由已认证通道、持久任务宿主装配。
/// 随具体请求移动到执行器，没有按会话覆盖的共享槽位。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlRunMetadata {
    pub scope: ContextScope,
    pub task_id: String,
    pub review: ControlReviewEvidence,
}
impl ControlRunMetadata {
    pub fn validate_scope(&self, session: &SessionKey) -> ControlResult<()> {
        session.validate().map_err(|_| ControlError::InvalidInput)?;
        if self.scope.session_id != session.session_id || self.scope.user_id != session.user_id {
            return Err(ControlError::OwnerMismatch);
        }
        if self.task_id.is_empty()
            || self.task_id.len() > 256
            || self.task_id.trim() != self.task_id
            || self.task_id.chars().any(char::is_control)
        {
            return Err(ControlError::InvalidInput);
        }
        match &self.review {
            ControlReviewEvidence::ChannelAnchor(anchor) => {
                if anchor.is_empty()
                    || anchor.len() > 256
                    || anchor.trim() != anchor
                    || anchor.chars().any(char::is_control)
                {
                    return Err(ControlError::InvalidInput);
                }
            }
            ControlReviewEvidence::FrozenDelegation(messages) => {
                if messages.is_empty()
                    || messages.len() > 6
                    || messages
                        .iter()
                        .try_fold(0usize, |bytes, message| bytes.checked_add(message.len()))
                        .is_none_or(|bytes| bytes > MAX_REVIEW_USER_MESSAGES_BYTES)
                {
                    return Err(ControlError::InvalidInput);
                }
                // JSON 编码总上限仍由最终 LlmHost 校验；这里不裁剪任何正文。
            }
        }
        Ok(())
    }

    pub fn validate_for(&self, input: &ControlInput) -> ControlResult<()> {
        input.validate()?;
        self.validate_scope(&input.session.key)?;
        if self.task_id != input.task_id {
            return Err(ControlError::InvalidInput);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ControlInput {
        ControlInput {
            session: eve_session_api::SessionInput {
                key: SessionKey::new("session", "user").unwrap(),
                text: "宿主任务说明".into(),
            },
            task_id: "task".into(),
        }
    }
    fn metadata(review: ControlReviewEvidence) -> ControlRunMetadata {
        ControlRunMetadata {
            scope: ContextScope {
                session_id: "session".into(),
                user_id: "user".into(),
            },
            task_id: "task".into(),
            review,
        }
    }

    #[test]
    fn complete_originals_include_empty_attachment_bodies_without_trimming() {
        let originals = vec![String::new(), " \t授权范围\n保留尾部 ".into()];
        let value = metadata(ControlReviewEvidence::FrozenDelegation(originals.clone()));
        assert_eq!(value.validate_for(&input()), Ok(()));
        assert_eq!(
            value.review,
            ControlReviewEvidence::FrozenDelegation(originals)
        );
    }

    #[test]
    fn invalid_metadata_never_becomes_an_implicit_channel_lookup() {
        for review in [
            ControlReviewEvidence::ChannelAnchor(String::new()),
            ControlReviewEvidence::ChannelAnchor(" message-id".into()),
            ControlReviewEvidence::ChannelAnchor("message\nidentity".into()),
            ControlReviewEvidence::ChannelAnchor("x".repeat(257)),
            ControlReviewEvidence::FrozenDelegation(vec![]),
            ControlReviewEvidence::FrozenDelegation(vec!["原文".into(); 7]),
            ControlReviewEvidence::FrozenDelegation(vec![
                "x".repeat(MAX_REVIEW_USER_MESSAGES_BYTES + 1),
            ]),
        ] {
            assert_eq!(
                metadata(review).validate_for(&input()),
                Err(ControlError::InvalidInput)
            );
        }
    }
}
