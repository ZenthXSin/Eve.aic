//! 通道已提交过轮次的会话是私聊还是群聊。只在内存中按会话标识记录，宿主只能读取；
//! 用于只在私聊中成立的能力（例如到时私聊提醒），不从用户文字或模型输出推断。
use crate::observer::QqInteractionSource;
use eve_plugin_api::{PluginError, PluginResult};
use eve_session_api::SessionKey;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

/// 记录上限；超过时淘汰最早记录的会话。被淘汰的会话下一条消息提交时重新记录。
const MAX_SESSIONS: usize = 4096;

#[derive(Default)]
struct Inner {
    group_routes: HashMap<String, String>,
    sources: HashMap<String, QqInteractionSource>,
    identities: HashMap<String, QqConversationIdentity>,
    order: VecDeque<String>,
    idle: bool,
    wakeup_claimed: bool,
}

/// 仅由已校验的平台路由构造；不会改变已有 SessionKey 或根据昵称合并身份。
#[derive(Clone)]
pub struct QqConversationIdentity {
    pub session: SessionKey,
    pub app_id: String,
    pub subject: String,
    pub conversation: String,
    pub source: QqInteractionSource,
}

#[derive(Clone, Default)]
pub struct QqConversations {
    inner: Arc<Mutex<Inner>>,
}
pub struct QqWakeupLease(QqConversations);
impl Drop for QqWakeupLease {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.0.inner.lock() {
            inner.wakeup_claimed = false;
        }
    }
}
impl QqConversations {
    pub(crate) fn set_idle(&self, idle: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.idle = idle;
        }
    }
    pub(crate) fn wakeup_claimed(&self) -> bool {
        self.inner.lock().map_or(true, |inner| inner.wakeup_claimed)
    }
    /// Reserve the idle channel before submitting a host wakeup. Incoming messages queue until release.
    pub fn try_claim_wakeup(&self) -> Option<QqWakeupLease> {
        let mut inner = self.inner.lock().ok()?;
        if !inner.idle || inner.wakeup_claimed {
            return None;
        }
        inner.wakeup_claimed = true;
        Some(QqWakeupLease(self.clone()))
    }

    pub fn identity(&self, session: &SessionKey) -> PluginResult<Option<QqConversationIdentity>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("QQ 身份目录不可用".into()))?;
        Ok(inner
            .identities
            .get(&session.session_id)
            .filter(|v| v.session == *session)
            .cloned())
    }

    pub(crate) fn group_route(&self, conversation: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()?
            .group_routes
            .get(conversation)
            .cloned()
    }

    pub(crate) fn record_message(
        &self,
        app: &str,
        message: &crate::state::Message,
    ) -> PluginResult<()> {
        let session = message.session_key(app)?;
        if message.scope == "group" {
            let conversation = message.conversation_key(app)?;
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| PluginError::State("QQ 群路由目录不可用".into()))?;
            if inner.group_routes.len() < MAX_SESSIONS
                || inner.group_routes.contains_key(&conversation)
            {
                inner
                    .group_routes
                    .insert(conversation, message.target_id.clone());
            }
        }
        self.record(&session, &message.scope);
        // 群与私聊的 OpenID 属于不同命名空间，绝不仅凭字符串相同自动关联。
        let subject = serde_json::to_vec(&(app, &message.scope, &message.user_id))
            .map_err(|_| PluginError::State("QQ 身份摘要编码失败".into()))?;
        let hash = ring::digest::digest(&ring::digest::SHA256, &subject);
        let subject = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let identity = QqConversationIdentity {
            session: session.clone(),
            app_id: app.into(),
            subject,
            conversation: message.conversation_key(app)?,
            source: match message.scope.as_str() {
                "c2c" => QqInteractionSource::DirectMessage,
                "group" => QqInteractionSource::GroupMessage,
                _ => return Err(PluginError::Task("QQ 身份范围无效".into())),
            },
        };
        self.inner
            .lock()
            .map_err(|_| PluginError::State("QQ 身份目录不可用".into()))?
            .identities
            .insert(session.session_id, identity);
        Ok(())
    }
    /// 这个会话是私聊还是群聊；本进程还没有为它提交过轮次时为 None。
    pub fn source(&self, session_id: &str) -> Option<QqInteractionSource> {
        self.inner.lock().ok()?.sources.get(session_id).copied()
    }

    pub(crate) fn record(&self, session: &SessionKey, scope: &str) {
        let source = match scope {
            "c2c" => QqInteractionSource::DirectMessage,
            "group" => QqInteractionSource::GroupMessage,
            _ => return,
        };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner
            .sources
            .insert(session.session_id.clone(), source)
            .is_none()
        {
            inner.order.push_back(session.session_id.clone());
            while inner.order.len() > MAX_SESSIONS {
                if let Some(oldest) = inner.order.pop_front() {
                    inner.sources.remove(&oldest);
                    inner.identities.remove(&oldest);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_recorded_by_scope_and_bounded() {
        let conversations = QqConversations::default();
        let private = SessionKey::new("qq:a", "qq:a").unwrap();
        let group = SessionKey::new("qq:b", "qq:b").unwrap();
        conversations.record(&private, "c2c");
        conversations.record(&group, "group");
        conversations.record(&SessionKey::new("qq:c", "qq:c").unwrap(), "other");
        assert_eq!(
            conversations.source("qq:a"),
            Some(QqInteractionSource::DirectMessage)
        );
        assert_eq!(
            conversations.source("qq:b"),
            Some(QqInteractionSource::GroupMessage)
        );
        assert_eq!(conversations.source("qq:c"), None);
        for index in 0..MAX_SESSIONS {
            conversations.record(&SessionKey::new(format!("qq:{index}"), "u").unwrap(), "c2c");
        }
        assert_eq!(conversations.source("qq:a"), None, "最早记录的会话被淘汰");
        assert!(
            conversations
                .source(&format!("qq:{}", MAX_SESSIONS - 1))
                .is_some()
        );
    }
}
