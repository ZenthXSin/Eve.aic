//! 通道已提交过轮次的会话是私聊还是群聊。只在内存中按会话标识记录，宿主只能读取；
//! 用于只在私聊中成立的能力（例如到时私聊提醒），不从用户文字或模型输出推断。
use crate::observer::QqInteractionSource;
use eve_session_api::SessionKey;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

/// 记录上限；超过时淘汰最早记录的会话。被淘汰的会话下一条消息提交时重新记录。
const MAX_SESSIONS: usize = 4096;

#[derive(Default)]
struct Inner {
    sources: HashMap<String, QqInteractionSource>,
    order: VecDeque<String>,
}

#[derive(Clone, Default)]
pub struct QqConversations {
    inner: Arc<Mutex<Inner>>,
}
impl QqConversations {
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
