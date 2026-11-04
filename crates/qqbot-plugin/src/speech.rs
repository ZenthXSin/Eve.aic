//! Eve 对宿主事实的发言决定。事实、决定和平台投递分别记录，内部结束不是消息。
use eve_llm_api::LlmFuture;
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 来自可信适配器的事实版本；正文没有发送含义，模型不能改变来源或收件人。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqSpeechEvent {
    pub id: String,
    pub session: SessionKey,
    pub anchor: String,
    pub facts: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqSpeechDecision {
    Speak { text: String, after_ms: u64 },
    Wait { after_ms: u64 },
    Silence {},
}

/// `consider` 必须先保存决定，不能 claim 来源或执行发送。
/// `None` 表示已处理或还没到复审时点；绝不表示发送成功。
pub trait QqSpeech: Send + Sync {
    fn enqueue(&self, event: QqSpeechEvent) -> PluginResult<()>;
    fn pending(&self) -> PluginResult<Vec<QqSpeechEvent>>;
    /// 来源暂不可用或路由尚未就绪；让出队列，不消耗发送尝试。
    fn postpone(&self, event: &QqSpeechEvent, after_ms: u64) -> PluginResult<()>;
    /// 事实版本已经失效；原决定保留在账本中，不再进入待发队列。
    fn retire(&self, event: &QqSpeechEvent) -> PluginResult<()>;
    fn silenced(&self, event: &QqSpeechEvent) -> PluginResult<bool>;
    fn consider(&self, event: QqSpeechEvent) -> LlmFuture<'_, Option<QqSpeechDecision>>;
    fn dispatched(&self, event: &QqSpeechEvent) -> PluginResult<()>;
    fn delivered(&self, event: &QqSpeechEvent, result: super::QqOutreachResult)
    -> PluginResult<()>;
    /// 导入前台明确表达的实际回执。重复导入必须幂等，不能触发发送。
    fn record_expression(
        &self,
        event: QqSpeechEvent,
        text: &str,
        result: super::QqOutreachResult,
    ) -> PluginResult<()>;
}
