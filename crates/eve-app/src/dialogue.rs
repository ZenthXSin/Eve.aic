//! 对话用途的当前表达约束，在历史之后提供；不修改持久会话、不重试或增加模型请求。
use eve_llm_api::{
    ChatMessage, ChatRole, LlmFuture, LlmProvider, ModelRequest, ModelResponse, ModelTextSink,
};
use std::sync::Arc;

pub(crate) const NAMESPACE: &str = "runtime.dialogue";
const POLICY: &str = include_str!("dialogue-policy.txt");
pub(crate) struct NaturalDialogue(pub(crate) Arc<dyn LlmProvider>);

fn apply(mut request: ModelRequest) -> ModelRequest {
    if let Some(user) = request
        .messages
        .iter()
        .rposition(|m| m.role == ChatRole::User)
    {
        // 保留 Runtime 当前工具清单与当前用户输入的相邻关系。
        let index = if user > 0
            && request.messages[user - 1].role == ChatRole::System
            && request.messages[user - 1]
                .text
                .as_deref()
                .is_some_and(|s| s.contains("eve.runtime.tool-capabilities"))
        {
            user - 1
        } else {
            user
        };
        request
            .messages
            .insert(index, ChatMessage::text(ChatRole::System, POLICY.trim()));
    }
    request
}
impl LlmProvider for NaturalDialogue {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.0.complete(apply(request))
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        self.0.stream(apply(request), sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_policy_follows_old_answers_and_preserves_current_capability_frame() {
        let request = ModelRequest {
            messages: vec![
                ChatMessage::text(ChatRole::User, "旧问题"),
                ChatMessage::text(ChatRole::Assistant, "旧助手连续追问"),
                ChatMessage::text(ChatRole::System, "eve.runtime.tool-capabilities"),
                ChatMessage::text(ChatRole::User, "当前消息"),
            ],
            tools: vec![],
        };
        let updated = apply(request.clone());
        updated.validate().unwrap();
        assert_eq!(updated.messages.len(), request.messages.len() + 1);
        assert!(
            updated.messages[2]
                .text
                .as_deref()
                .unwrap()
                .starts_with("本轮自然接话约束")
        );
        assert_eq!(updated.messages[3], request.messages[2]);
        assert_eq!(updated.messages[4], request.messages[3]);
        assert_eq!(request.messages.len(), 4);
    }
}
