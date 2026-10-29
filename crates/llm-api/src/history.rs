use crate::{ChatMessage, ChatRole, ContextScope, LlmError, LlmFuture};

/// 主动上下文压缩的整次期限，包含所有分批摘要请求。
pub const CONTEXT_COMPACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// 输入上下文的估算 token 阈值；不代表模型厂商的精确分词结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextCompactionPolicy {
    pub trigger_tokens: usize,
    pub target_tokens: usize,
}

impl ContextCompactionPolicy {
    pub fn validate(self) -> Result<(), LlmError> {
        if self.target_tokens == 0 || self.target_tokens >= self.trigger_tokens {
            return Err(LlmError::Configuration(
                "压缩目标必须大于零且小于触发阈值".into(),
            ));
        }
        Ok(())
    }
}

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
    /// 自适应上下文只设总上限；None 继续使用旧的固定阈值。
    fn adaptive_limit(&self) -> Result<Option<usize>, LlmError> {
        Ok(None)
    }
    fn compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        Ok(None)
    }
    /// 本轮尚未调用工具时（闲聊）使用的较小压缩阈值；只在主动压缩开启、且触发阈值
    /// 小于 `compaction` 的阈值时生效。本轮一旦调用工具，改用 `compaction`。
    fn casual_compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        Ok(None)
    }
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

    /// 只读、指纹匹配的已存摘要；不得因此登记后台压缩。
    fn cached(
        &self,
        _scope: Option<&ContextScope>,
        _history: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        Ok(None)
    }

    /// 主动压缩，必须完整覆盖所给轮次才可替换原文。None 作用域只生成临时摘要，不写入历史存储。
    /// 宿主负责取消和总期限；实现不能重放工具。
    fn compact<'a>(
        &'a self,
        scope: Option<&'a ContextScope>,
        messages: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryDigest>> {
        Box::pin(async move { self.digest(scope, messages) })
    }
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

/// 工作集只作为非授权资料进入请求；不能替代用户原话、身份或工具权限。
pub const MAX_HISTORY_WORKING_SET_BYTES: usize = 16 * 1024;
/// 六条审核原话独立保留，超过此总量时拒绝本轮，不截断授权上下文。
pub const MAX_REVIEW_USER_MESSAGES_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryContextPlan {
    /// 原始 history_turns 的零基下标；严格递增、唯一，只选择完整轮次。
    pub retained_turns: Vec<usize>,
    /// 宿主核验范围后的工作集；模型摘要不是授权，原话仍由审核通道独立保留。
    pub working_set: Vec<String>,
}

/// 可选的分层历史加载器。scope 来自已授权宿主；相关性模型不能授予范围或权限。
/// None 明确沿用既有上下文路径（例如群观察锚点）；Err 停止本轮。
/// 实现可在内部对建议模型失败采用确定性退路，不能无声跨范围加载资料。
pub trait HistoryContextPlanner: Send + Sync {
    fn plan<'a>(
        &'a self,
        scope: Option<&'a ContextScope>,
        input: &'a crate::TurnInput,
        history: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryContextPlan>>;
}
