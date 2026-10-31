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
