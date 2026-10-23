//! 已校验路由的参与者资料；昵称和正文仍为用户数据，不授予权限。
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;
pub trait QqParticipantObserver: Send + Sync {
    /// conversation 是按 AppID、私聊/群与目标计算的摘要，不跨范围合并人名；
    /// private 取自已校验的平台路由（私聊为 true），不来自正文。
    fn observe(
        &self,
        session: &SessionKey,
        conversation: &str,
        private: bool,
        text: &str,
        sender_name: Option<&str>,
    ) -> PluginResult<()>;
}
