use crate::{ChatMessage, ChatRole, ContextScope, LlmError};

/// 单次请求的历史窗口；字节数按各完整轮次的 JSON UTF-8 长度累计，不是模型 token 数。
/// 不限制当前用户输入、系统资料、工具定义或本轮新产生的工具结果，不修改持久历史。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryWindow {
    pub max_turns: usize,
    pub max_bytes: usize,
}

impl HistoryWindow {
    pub fn validate(self) -> Result<(), LlmError> {
        if self.max_turns == 0 || self.max_bytes == 0 {
            return Err(LlmError::Configuration(
                "历史窗口轮数和字节数必须大于零".into(),
            ));
        }
        Ok(())
    }
}

/// 每轮开始前读取一次；同一轮的多次模型调用使用同一快照。
pub trait HistoryWindowResolver: Send + Sync {
    fn resolve(&self) -> Result<HistoryWindow, LlmError>;
}

/// 单个历史摘要正文的上限（UTF-8 字节）。
pub const MAX_HISTORY_DIGEST_BYTES: usize = 8192;

/// 被历史窗口省略的较早轮次的摘要。`covered_turns` 是它覆盖的、从最早一轮起连续的完整轮次数；
/// 摘要是模型归纳，不是原文，原文仍完整保存在会话中。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryDigest {
    pub covered_turns: usize,
    pub text: String,
}

/// 历史窗口省略较早轮次时提供摘要，取代直接省略。
///
/// `omitted` 是本轮被省略的最早若干完整轮次，按原顺序。实现返回覆盖其开头若干轮的摘要（如有），
/// 并可据此安排后台为尚未覆盖的轮次生成摘要；不得在此发起模型请求或长时间阻塞。
/// 返回错误时宿主照常省略，不让本轮失败。
pub trait HistoryDigests: Send + Sync {
    fn digest(
        &self,
        scope: Option<&ContextScope>,
        omitted: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError>;
}

/// 把历史按完整轮次分组：每组从一条用户消息开始，包含其后的助手回复与工具往返。
pub fn history_turns(messages: &[ChatMessage]) -> Vec<&[ChatMessage]> {
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == ChatRole::User).then_some(index))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = starts.get(position + 1).copied().unwrap_or(messages.len());
            &messages[start..end]
        })
        .collect()
}
