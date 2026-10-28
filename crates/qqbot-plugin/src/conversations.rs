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
    turn_anchors: HashMap<String, QqTurnAnchor>,
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

impl QqConversationIdentity {
    /// 可信平台账号标识：AppID 与群/私聊命名空间进入摘要，群号不进入。
    pub fn account_id(&self) -> String {
        format!("qq-account:{}", self.subject)
    }
    pub fn valid_account_id(value: &str) -> bool {
        value.strip_prefix("qq-account:").is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }
}

/// 当前已准入轮次的可信消息集合；后续收到的消息只更新路由，不改变本轮锚点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QqTurnAnchor {
    pub session: SessionKey,
    pub conversation: String,
    /// 按本轮合并顺序，最后一条是本轮真实主消息；不是合并文本生成的伪ID。
    pub message_ids: Vec<String>,
    pub speaker_id: String,
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

    /// 仅通道准入时写入；读取不依赖正文相等、附件描述或最新收到的消息。
    pub fn turn_anchor(&self, session: &SessionKey) -> PluginResult<Option<QqTurnAnchor>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("QQ轮次锚点目录不可用".into()))?;
        Ok(inner
            .turn_anchors
            .get(&session.session_id)
            .filter(|a| a.session == *session)
            .cloned())
    }
    pub(crate) fn bind_turn_messages(
        &self,
        app: &str,
        messages: &[crate::state::Message],
    ) -> PluginResult<()> {
        let invalid = || PluginError::Task("QQ轮次消息锚点无效".into());
        let last = messages.last().ok_or_else(invalid)?;
        if messages.len() > 32 || messages.iter().any(|m| !m.valid()) {
            return Err(invalid());
        }
        let session = last.session_key(app)?;
        let conversation = last.conversation_key(app)?;
        let mut ids = std::collections::BTreeSet::new();
        for message in messages {
            if message.session_key(app)? != session
                || message.conversation_key(app)? != conversation
                || !ids.insert(message.id.clone())
            {
                return Err(invalid());
            }
        }
        let anchor = QqTurnAnchor {
            session: session.clone(),
            conversation,
            message_ids: messages.iter().map(|m| m.id.clone()).collect(),
            speaker_id: last.user_id.clone(),
        };
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("QQ轮次锚点目录不可用".into()))?;
        if inner.turn_anchors.len() >= MAX_SESSIONS
            && !inner.turn_anchors.contains_key(&session.session_id)
        {
            return Err(PluginError::State("QQ轮次锚点目录已满".into()));
        }
        // Busy后重新准入同一主ID时保留原合并批次，不能因合并正文已写回队列而丢掉早期ID。
        if messages.len() == 1
            && inner
                .turn_anchors
                .get(&session.session_id)
                .is_some_and(|a| a.session == session && a.message_ids.last() == Some(&last.id))
        {
            return Ok(());
        }
        inner.turn_anchors.insert(session.session_id, anchor);
        Ok(())
    }

    pub(crate) fn clear_turn_anchor(
        &self,
        session: &SessionKey,
        primary_message_id: &str,
    ) -> PluginResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("QQ轮次锚点目录不可用".into()))?;
        if inner
            .turn_anchors
            .get(&session.session_id)
            .is_some_and(|a| {
                a.session == *session
                    && a.message_ids
                        .last()
                        .is_some_and(|id| id == primary_message_id)
            })
        {
            inner.turn_anchors.remove(&session.session_id);
        }
        Ok(())
    }

    /// 可信宿主为已有会话选择通知路由；新近登记的会话在前，不接受模型自报路由。
    pub fn recent_identities(&self) -> PluginResult<Vec<QqConversationIdentity>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("QQ 身份目录不可用".into()))?;
        Ok(inner
            .order
            .iter()
            .rev()
            .filter_map(|key| inner.identities.get(key).cloned())
            .collect())
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
                    inner.turn_anchors.remove(&oldest);
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

#[cfg(test)]
mod turn_anchor_tests {
    use super::*;
    fn message(id: &str, speaker: &str) -> crate::Message {
        crate::Message {
            id: id.into(),
            scope: "group".into(),
            target_id: "group-1".into(),
            user_id: speaker.into(),
            text: "同一句话".into(),
            attachments: vec![],
            quoted: None,
        }
    }
    #[test]
    fn admitted_ids_survive_later_same_text_other_members_and_busy_batch_retry() {
        let conversations = QqConversations::default();
        let first = message("first", "member-a");
        let primary = message("primary", "member-a");
        conversations.record_message("app", &first).unwrap();
        conversations.record_message("app", &primary).unwrap();
        conversations
            .bind_turn_messages("app", &[first.clone(), primary.clone()])
            .unwrap();
        let session = primary.session_key("app").unwrap();
        for n in 0..25 {
            conversations
                .record_message("app", &message(&format!("later-{n}"), "member-a"))
                .unwrap();
        }
        conversations
            .record_message("app", &message("other-member", "member-b"))
            .unwrap();
        let anchor = conversations.turn_anchor(&session).unwrap().unwrap();
        assert_eq!(anchor.message_ids, vec!["first", "primary"]);
        conversations
            .bind_turn_messages("app", std::slice::from_ref(&primary))
            .unwrap();
        assert_eq!(
            conversations
                .turn_anchor(&session)
                .unwrap()
                .unwrap()
                .message_ids,
            vec!["first", "primary"]
        );
        assert!(
            conversations
                .bind_turn_messages("app", &[first, message("wrong-member", "member-b")])
                .is_err()
        );
        assert!(
            conversations
                .turn_anchor(&message("x", "member-b").session_key("app").unwrap())
                .unwrap()
                .is_none()
        );
        conversations
            .clear_turn_anchor(&session, "later-1")
            .unwrap();
        assert!(conversations.turn_anchor(&session).unwrap().is_some());
        conversations
            .clear_turn_anchor(&session, "primary")
            .unwrap();
        assert!(conversations.turn_anchor(&session).unwrap().is_none());
    }
    #[test]
    fn attachment_only_message_anchors_by_real_id_without_text_guessing() {
        let conversations = QqConversations::default();
        let mut attachment = message("attachment-id", "member-a");
        attachment.text.clear();
        attachment.attachments.push(crate::ImageAttachment {
            content_type: "image/png".into(),
            url: "https://example.org/image.png".into(),
            filename: None,
        });
        assert!(attachment.valid());
        conversations.record_message("app", &attachment).unwrap();
        conversations
            .bind_turn_messages("app", &[attachment.clone()])
            .unwrap();
        let anchor = conversations
            .turn_anchor(&attachment.session_key("app").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(anchor.message_ids, vec!["attachment-id"]);
    }
}
