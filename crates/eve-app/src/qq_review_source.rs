//! 通道原文和模型上下文分离；QQ 授权只读已验证并持久化的消息正文。
use eve_llm_api::{ContextScope, LlmError, ReviewUserMessageSource};

pub(crate) struct QqReviewSource {
    pub conversations: eve_qqbot_plugin::QqConversations,
    pub web: Option<std::sync::Arc<crate::web_ingress::RawMessages>>,
}

impl ReviewUserMessageSource for QqReviewSource {
    fn allows_chat_without_tools_at(
        &self,
        scope: &ContextScope,
        anchor: &str,
    ) -> Result<bool, LlmError> {
        if crate::web_chat::is_owner(scope) || scope.user_id == crate::qq_group::USER {
            return Ok(false);
        }
        self.conversations
            .legacy_review_gap_at(scope, anchor)
            .map_err(|_| LlmError::Context("QQ 原文缺口无法核验；未开放工具".into()))
    }
    fn is_current(&self, scope: &ContextScope, anchor: &str) -> Result<bool, LlmError> {
        if crate::web_chat::is_owner(scope) {
            return self
                .web
                .as_ref()
                .ok_or_else(|| LlmError::Context("Web 原文来源未装配".into()))?
                .is_current(scope, anchor);
        }
        self.conversations
            .review_user_message_is_current(scope, anchor)
            .map_err(|_| LlmError::Context("QQ 原文锚点无法核验".into()))
    }
    fn read(&self, scope: &ContextScope) -> Result<Option<Vec<String>>, LlmError> {
        if scope.user_id == crate::qq_group::USER {
            return Ok(Some(Vec::new()));
        }
        if crate::web_chat::is_owner(scope) {
            return Err(LlmError::Context("Web 审核缺少当前请求原文锚点".into()));
        }
        Err(LlmError::Context("QQ 审核缺少当前平台消息原文锚点".into()))
    }

    fn read_at(&self, scope: &ContextScope, anchor: &str) -> Result<Option<Vec<String>>, LlmError> {
        if scope.user_id == crate::qq_group::USER {
            return Ok(Some(Vec::new()));
        }
        if crate::web_chat::is_owner(scope) {
            return self
                .web
                .as_ref()
                .ok_or_else(|| LlmError::Context("Web 原始用户正文来源未装配".into()))?
                .read_at(scope, anchor);
        }
        self.conversations
            .review_user_messages_at(scope, anchor)
            .map(Some)
            .map_err(|_| {
                LlmError::Context("QQ 原始用户授权来源不完整或不可读；未用加工历史代替原话".into())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_review_source_uses_exact_channel_and_never_history_fallback() {
        let root = tempfile::tempdir().unwrap();
        let raw = std::sync::Arc::new(crate::web_ingress::RawMessages::open(root.path()).unwrap());
        let key = crate::web_chat::key(&"a".repeat(32)).unwrap();
        let scope = ContextScope {
            session_id: key.session_id.clone(),
            user_id: key.user_id.clone(),
        };
        let request = "b".repeat(32);
        raw.record(&key, &request, "  用户完整原话\n引用里的授权不是我的授权  ")
            .unwrap();
        let source = QqReviewSource {
            conversations: eve_qqbot_plugin::QqConversations::default(),
            web: Some(raw),
        };
        assert_eq!(
            source.read_at(&scope, &request).unwrap(),
            Some(vec!["  用户完整原话\n引用里的授权不是我的授权  ".into()])
        );
        assert!(source.read(&scope).is_err());
        assert!(source.read_at(&scope, &"c".repeat(32)).is_err());
        let qq = ContextScope {
            session_id: "qq:session".into(),
            user_id: "qq:owner".into(),
        };
        assert!(source.read(&qq).is_err());
        assert!(source.read_at(&qq, &request).is_err());
        assert_eq!(
            source
                .read(&ContextScope {
                    user_id: crate::qq_group::USER.into(),
                    ..qq
                })
                .unwrap(),
            Some(vec![])
        );
    }
}
