use crate::{SessionError, SessionKey, SessionResult, SessionSnapshot, SessionTurn};
use std::collections::VecDeque;

pub const MAX_SESSION_TURN_PAGE: usize = 16;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionHistorySummary {
    pub key: SessionKey,
    pub revision: u64,
    pub turn_count: u64,
}
/// 固定创建时的完整轮次版本，不删除原始数据。空页表示结束，错误后丢弃游标。
/// 内置实现停止后拒绝读取；兼容实现默认持有已经交付的快照副本。
pub trait SessionTurnCursor: Send {
    fn summary(&self) -> &SessionHistorySummary;
    fn next_page(&mut self) -> SessionResult<Vec<SessionTurn>>;
}
pub struct SnapshotTurnCursor {
    summary: SessionHistorySummary,
    turns: VecDeque<SessionTurn>,
}
impl SnapshotTurnCursor {
    pub fn new(snapshot: SessionSnapshot) -> SessionResult<Self> {
        snapshot.validate()?;
        Ok(Self {
            summary: SessionHistorySummary {
                key: snapshot.key,
                revision: snapshot.revision,
                turn_count: u64::try_from(snapshot.turns.len())
                    .map_err(|_| SessionError::LimitReached)?,
            },
            turns: snapshot.turns.into(),
        })
    }
}
impl SessionTurnCursor for SnapshotTurnCursor {
    fn summary(&self) -> &SessionHistorySummary {
        &self.summary
    }
    fn next_page(&mut self) -> SessionResult<Vec<SessionTurn>> {
        let count = self.turns.len().min(MAX_SESSION_TURN_PAGE);
        Ok(self.turns.drain(..count).collect())
    }
}

/// 兼容旧服务的有界收集；这里只保留所需尾页，不复制所有历史正文。
pub(crate) fn recent_from_cursor(
    mut cursor: Box<dyn SessionTurnCursor>,
    key: &SessionKey,
    before: Option<u64>,
    limit: usize,
) -> SessionResult<Vec<SessionTurn>> {
    if cursor.summary().key != *key {
        return Err(SessionError::OwnerMismatch);
    }
    if before.is_some_and(|id| id <= 1) {
        return Ok(vec![]);
    }
    let count = cursor.summary().turn_count;
    let mut last = 0u64;
    let mut retained = VecDeque::with_capacity(limit);
    loop {
        let page = cursor.next_page()?;
        if page.len() > MAX_SESSION_TURN_PAGE {
            return Err(SessionError::CorruptState);
        }
        if page.is_empty() {
            if last != count {
                return Err(SessionError::CorruptState);
            }
            break;
        }
        for turn in page {
            if last.checked_add(1) != Some(turn.id) || turn.id > count {
                return Err(SessionError::CorruptState);
            }
            if before.is_some_and(|before| turn.id >= before) {
                return Ok(retained.into_iter().rev().collect());
            }
            last = turn.id;
            if retained.len() == limit {
                retained.pop_front();
            }
            retained.push_back(turn);
        }
    }
    Ok(retained.into_iter().rev().collect())
}

#[cfg(test)]
mod recent_tests {
    use super::*;
    use crate::{
        SessionFailure, SessionInput, SessionService, SessionTurnStatus, StartedTurn, TurnLease,
    };
    use eve_llm_api::ChatMessage;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn key() -> SessionKey {
        SessionKey::new("session", "owner").unwrap()
    }
    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            key: key(),
            revision: 40,
            turns: (1..=20)
                .map(|id| SessionTurn {
                    id,
                    input: format!("original {id}"),
                    status: SessionTurnStatus::Interrupted,
                })
                .collect(),
        }
    }
    struct LegacySnapshot;
    impl SessionService for LegacySnapshot {
        fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            Ok(Some(snapshot()))
        }
        fn begin(&self, _: SessionInput) -> SessionResult<StartedTurn> {
            Err(SessionError::Unavailable)
        }
        fn complete(&self, _: &TurnLease, _: Vec<ChatMessage>) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
        fn fail(&self, _: &TurnLease, _: SessionFailure) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
    }
    struct CountingCursor {
        inner: SnapshotTurnCursor,
        pages: Arc<AtomicUsize>,
    }
    impl SessionTurnCursor for CountingCursor {
        fn summary(&self) -> &SessionHistorySummary {
            self.inner.summary()
        }
        fn next_page(&mut self) -> SessionResult<Vec<SessionTurn>> {
            self.pages.fetch_add(1, Ordering::SeqCst);
            self.inner.next_page()
        }
    }
    struct CursorOnly {
        pages: Arc<AtomicUsize>,
    }
    impl SessionService for CursorOnly {
        fn scan_turns(&self, _: &SessionKey) -> SessionResult<Option<Box<dyn SessionTurnCursor>>> {
            Ok(Some(Box::new(CountingCursor {
                inner: SnapshotTurnCursor::new(snapshot())?,
                pages: self.pages.clone(),
            })))
        }
        fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            panic!("recent fallback must use the provided cursor")
        }
        fn begin(&self, _: SessionInput) -> SessionResult<StartedTurn> {
            Err(SessionError::Unavailable)
        }
        fn complete(&self, _: &TurnLease, _: Vec<ChatMessage>) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
        fn fail(&self, _: &TurnLease, _: SessionFailure) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
    }

    #[test]
    fn old_snapshot_only_service_keeps_working_with_recent_pagination() {
        let service: &dyn SessionService = &LegacySnapshot;
        let page = service.recent_turns(&key(), None, 6).unwrap();
        assert_eq!(
            page.iter().map(|turn| turn.id).collect::<Vec<_>>(),
            [20, 19, 18, 17, 16, 15]
        );
        assert_eq!(
            service
                .recent_turns(&key(), Some(15), 6)
                .unwrap()
                .iter()
                .map(|turn| turn.id)
                .collect::<Vec<_>>(),
            [14, 13, 12, 11, 10, 9]
        );
        assert_eq!(
            service.recent_turns(&SessionKey::new("session", "other").unwrap(), None, 6),
            Err(SessionError::OwnerMismatch)
        );
        assert_eq!(
            service.recent_turns(&key(), None, 0),
            Err(SessionError::InvalidInput)
        );
    }

    #[test]
    fn cursor_fallback_checks_full_owner_and_ends_scan_at_before_without_using_snapshot() {
        let pages = Arc::new(AtomicUsize::new(0));
        let service = CursorOnly {
            pages: pages.clone(),
        };
        assert_eq!(
            service.recent_turns(&SessionKey::new("session", "other").unwrap(), None, 6),
            Err(SessionError::OwnerMismatch)
        );
        assert_eq!(pages.load(Ordering::SeqCst), 0);
        assert!(service.recent_turns(&key(), Some(0), 6).unwrap().is_empty());
        assert_eq!(pages.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .recent_turns(&key(), Some(8), 3)
                .unwrap()
                .iter()
                .map(|turn| turn.id)
                .collect::<Vec<_>>(),
            [7, 6, 5]
        );
        assert_eq!(pages.load(Ordering::SeqCst), 1);
        pages.store(0, Ordering::SeqCst);
        assert_eq!(
            service
                .recent_turns(&key(), None, 2)
                .unwrap()
                .iter()
                .map(|turn| turn.id)
                .collect::<Vec<_>>(),
            [20, 19]
        );
        assert_eq!(pages.load(Ordering::SeqCst), 3);
    }
}
