//! 已校验路由的参与者资料；昵称和正文仍为用户数据，不授予权限。
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;
pub trait QqParticipantObserver: Send + Sync {
    /// conversation 是按 AppID、私聊/群与目标计算的摘要，不跨范围合并人名。
    fn observe(
        &self,
        session: &SessionKey,
        conversation: &str,
        text: &str,
        sender_name: Option<&str>,
    ) -> PluginResult<()>;
}
