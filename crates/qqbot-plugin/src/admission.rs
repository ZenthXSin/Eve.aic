//! 可信宿主提供的短调度租约。此接口不接受正文，不能把历史文本变成授权。
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;

#[derive(Clone, Copy)]
pub enum QqAdmissionResult {
    Started,
    Retry,
    Rejected,
    Unknown,
}
/// begin只覆盖同步submit，不等待模型、工具或消息发送；长执行不占全局租约。
pub trait QqAdmission: Send + Sync {
    fn begin(&self, session: &SessionKey, source: &str) -> PluginResult<Option<String>>;
    fn begin_control(
        &self,
        session: &SessionKey,
        source: &str,
        stop: bool,
    ) -> PluginResult<Option<String>>;
    fn finish(&self, ticket: &str, result: QqAdmissionResult) -> PluginResult<()>;
    fn abandon(&self, session: &SessionKey, source: &str) -> PluginResult<()>;
    fn completed(&self, session: &SessionKey, source: &str, known: bool) -> PluginResult<()>;
}
