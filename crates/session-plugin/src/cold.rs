//! 正文不可变，索引提交才使新轮次可见；失败从不重放工具。
use super::*;
use crate::archive::{key as hash, valid};

const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LEGACY_INDEX_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    hash: String,
    bytes: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    key: SessionKey,
    revision: u64,
    turns: Vec<Record>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format_version: u32,
    sessions: BTreeMap<String, History>,
}
struct ColdInner {
    index: Index,
    resets: ContextResets,
    active: bool,
}
fn deactivate(inner: &mut ColdInner) {
    inner.active = false;
    inner.index.sessions.clear();
    inner.resets.boundaries.clear();
}
fn load(
    store: &dyn SessionArchiveStore,
    record: &Record,
    position: usize,
) -> SessionResult<SessionTurn> {
    if !valid(&record.hash) || record.bytes > MAX_SESSION_ARCHIVE_RECORD_BYTES {
        return Err(SessionError::CorruptState);
    }
    let bytes = store.get(&record.hash)?;
    if bytes.len() != record.bytes || hash(&bytes) != record.hash {
        return Err(SessionError::CorruptState);
    }
    let turn: SessionTurn = serde_json::from_value(
        strict_json::from_slice(&bytes).map_err(|_| SessionError::CorruptState)?,
    )
    .map_err(|_| SessionError::CorruptState)?;
    if turn.id != position as u64 + 1 || turn.input.trim().is_empty() {
        return Err(SessionError::CorruptState);
    }
    match &turn.status {
        SessionTurnStatus::Completed { messages } => validate_completed_turn(&turn.input, messages),
        SessionTurnStatus::Failed { failure } => failure.validate(),
        _ => Ok(()),
    }
    .map_err(|_| SessionError::CorruptState)?;
    Ok(turn)
}
fn freeze(store: &dyn SessionArchiveStore, turn: &SessionTurn) -> SessionResult<Record> {
    let bytes = serde_json::to_vec(turn).map_err(|_| SessionError::CorruptState)?;
    if bytes.len() > eve_session_api::MAX_SESSION_ARCHIVE_WRITE_BYTES {
        return Err(SessionError::LimitReached);
    }
    let hash = hash(&bytes);
    store.put(&hash, &bytes)?;
    Ok(Record {
        hash,
        bytes: bytes.len(),
    })
}
fn encode(index: &Index) -> SessionResult<Vec<u8>> {
    let bytes = serde_json::to_vec(index).map_err(|_| SessionError::CorruptState)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(SessionError::LimitReached);
    }
    Ok(bytes)
}
/// 创建前完整校验，校验时最多保留一轮正文；不在校验途中恢复 Pending。
fn validate(
    store: &dyn SessionArchiveStore,
    index: &Index,
    resets: &ContextResets,
) -> SessionResult<()> {
    if index.format_version != 2 || resets.format_version != 1 {
        return Err(SessionError::UnsupportedVersion);
    }
    for (id, history) in &index.sessions {
        history
            .key
            .validate()
            .map_err(|_| SessionError::CorruptState)?;
        if id != &history.key.session_id {
            return Err(SessionError::CorruptState);
        }
        let mut revision = 0u64;
        for (position, record) in history.turns.iter().enumerate() {
            let turn = load(store, record, position)?;
            let pending = turn.status == SessionTurnStatus::Pending;
            if pending && position + 1 != history.turns.len() {
                return Err(SessionError::CorruptState);
            }
            revision = revision
                .checked_add(if pending { 1 } else { 2 })
                .ok_or(SessionError::LimitReached)?;
        }
        if history.revision != revision {
            return Err(SessionError::CorruptState);
        }
    }
    for (id, boundary) in &resets.boundaries {
        let history = index.sessions.get(id).ok_or(SessionError::CorruptState)?;
        if boundary.key != history.key
            || boundary.through == 0
            || boundary.through > history.turns.len() as u64
        {
            return Err(SessionError::CorruptState);
        }
    }
    Ok(())
}
fn checked<T>(inner: &mut ColdInner, result: SessionResult<T>) -> SessionResult<T> {
    if result.is_err() {
        deactivate(inner);
    }
    result
}
pub(crate) struct ColdSessions {
    context: PluginContext,
    archive: Arc<dyn SessionArchiveStore>,
    inner: Arc<Mutex<ColdInner>>,
}
impl ColdSessions {
    pub(crate) fn open(
        context: PluginContext,
        archive: Arc<dyn SessionArchiveStore>,
    ) -> SessionResult<Self> {
        let bytes = context
            .state_get(SESSION_STATE_KEY)
            .map_err(|_| SessionError::Storage)?;
        let resets: ContextResets = match context
            .state_get(CONTEXT_RESETS_STATE_KEY)
            .map_err(|_| SessionError::Storage)?
        {
            Some(bytes) => serde_json::from_value(
                strict_json::from_slice(&bytes).map_err(|_| SessionError::CorruptState)?,
            )
            .map_err(|_| SessionError::CorruptState)?,
            None => ContextResets::default(),
        };
        let mut migrated = false;
        let mut index = match bytes {
            None => Index {
                format_version: 2,
                sessions: BTreeMap::new(),
            },
            Some(bytes) => {
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| SessionError::CorruptState)?;
                match value
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64)
                {
                    Some(2) => {
                        if bytes.len() > MAX_LEGACY_INDEX_BYTES {
                            return Err(SessionError::LimitReached);
                        }
                        serde_json::from_value(value).map_err(|_| SessionError::CorruptState)?
                    }
                    Some(1) => {
                        let legacy: Document = serde_json::from_value(value)
                            .map_err(|_| SessionError::CorruptState)?;
                        // 迁移也先验证所有会话，不能先写入一部分再发现另一个损坏。
                        for (id, snapshot) in &legacy.sessions {
                            snapshot
                                .validate()
                                .map_err(|_| SessionError::CorruptState)?;
                            if id != &snapshot.key.session_id {
                                return Err(SessionError::CorruptState);
                            }
                        }
                        for (id, boundary) in &resets.boundaries {
                            let snapshot =
                                legacy.sessions.get(id).ok_or(SessionError::CorruptState)?;
                            if boundary.key != snapshot.key
                                || boundary.through == 0
                                || boundary.through > snapshot.turns.len() as u64
                            {
                                return Err(SessionError::CorruptState);
                            }
                        }
                        if resets.format_version != 1 {
                            return Err(SessionError::UnsupportedVersion);
                        }
                        let mut sessions = BTreeMap::new();
                        for (id, snapshot) in legacy.sessions {
                            let turns = snapshot
                                .turns
                                .iter()
                                .map(|turn| freeze(archive.as_ref(), turn))
                                .collect::<SessionResult<_>>()?;
                            sessions.insert(
                                id,
                                History {
                                    key: snapshot.key,
                                    revision: snapshot.revision,
                                    turns,
                                },
                            );
                        }
                        migrated = true;
                        Index {
                            format_version: 2,
                            sessions,
                        }
                    }
                    _ => return Err(SessionError::UnsupportedVersion),
                }
            }
        };
        validate(archive.as_ref(), &index, &resets)?;
        let mut recovered = false;
        for history in index.sessions.values_mut() {
            if let Some((position, record)) = history.turns.iter_mut().enumerate().next_back() {
                let mut turn = load(archive.as_ref(), record, position)?;
                if turn.status == SessionTurnStatus::Pending {
                    turn.status = SessionTurnStatus::Interrupted;
                    *record = freeze(archive.as_ref(), &turn)?;
                    history.revision = history
                        .revision
                        .checked_add(1)
                        .ok_or(SessionError::LimitReached)?;
                    recovered = true;
                }
            }
        }
        if migrated || recovered {
            context
                .state_set(SESSION_STATE_KEY, encode(&index)?)
                .map_err(|_| SessionError::Storage)?;
        }
        Ok(Self {
            context,
            archive,
            inner: Arc::new(Mutex::new(ColdInner {
                index,
                resets,
                active: true,
            })),
        })
    }
    fn lock(&self) -> SessionResult<MutexGuard<'_, ColdInner>> {
        let inner = self.inner.lock().map_err(|_| SessionError::Unavailable)?;
        if !inner.active {
            return Err(SessionError::Unavailable);
        }
        Ok(inner)
    }
    fn save(&self, inner: &mut ColdInner, next: Index) -> SessionResult<()> {
        let bytes = encode(&next)?;
        if self.context.state_set(SESSION_STATE_KEY, bytes).is_err() {
            deactivate(inner);
            return Err(SessionError::Storage);
        }
        inner.index = next;
        Ok(())
    }
    pub(crate) fn close(&self) -> PluginResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| PluginError::State("会话锁不可用".into()))?;
        deactivate(&mut inner);
        Ok(())
    }
    fn finish(&self, lease: &TurnLease, status: SessionTurnStatus) -> SessionResult<()> {
        lease.key.validate()?;
        let mut inner = self.lock()?;
        let mut next = inner.index.clone();
        let history = next
            .sessions
            .get_mut(&lease.key.session_id)
            .ok_or(SessionError::StaleTurn)?;
        if history.key != lease.key {
            return Err(SessionError::OwnerMismatch);
        }
        let position = history
            .turns
            .len()
            .checked_sub(1)
            .ok_or(SessionError::StaleTurn)?;
        let mut turn = checked(
            &mut inner,
            load(self.archive.as_ref(), &history.turns[position], position),
        )?;
        if turn.id != lease.turn_id || turn.status != SessionTurnStatus::Pending {
            return Err(SessionError::StaleTurn);
        }
        match &status {
            SessionTurnStatus::Completed { messages } => {
                validate_completed_turn(&turn.input, messages)?
            }
            SessionTurnStatus::Failed { failure } => failure.validate()?,
            _ => return Err(SessionError::InvalidInput),
        }
        turn.status = status;
        history.turns[position] = freeze(self.archive.as_ref(), &turn)?;
        history.revision = history
            .revision
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        self.save(&mut inner, next)
    }
}
struct ColdCursor {
    inner: Arc<Mutex<ColdInner>>,
    archive: Arc<dyn SessionArchiveStore>,
    summary: SessionHistorySummary,
    records: Vec<Record>,
    next: usize,
    failed: bool,
}
impl SessionTurnCursor for ColdCursor {
    fn summary(&self) -> &SessionHistorySummary {
        &self.summary
    }
    fn next_page(&mut self) -> SessionResult<Vec<SessionTurn>> {
        if self.failed {
            return Err(SessionError::Unavailable);
        }
        let result = (|| {
            let mut inner = self.inner.lock().map_err(|_| SessionError::Unavailable)?;
            if !inner.active {
                return Err(SessionError::Unavailable);
            }
            let mut end = self.next;
            let mut page_bytes = 0usize;
            while end < self.records.len() && end - self.next < MAX_SESSION_TURN_PAGE {
                let size = self.records[end].bytes;
                if end > self.next
                    && page_bytes.saturating_add(size) > MAX_SESSION_ARCHIVE_RECORD_BYTES
                {
                    break;
                }
                page_bytes = page_bytes.saturating_add(size);
                end += 1;
            }
            let page = (self.next..end)
                .map(|position| load(self.archive.as_ref(), &self.records[position], position))
                .collect();
            let page = checked(&mut inner, page)?;
            self.next = end;
            Ok(page)
        })();
        if result.is_err() {
            self.failed = true;
            self.records.clear();
        }
        result
    }
}
impl SessionService for ColdSessions {
    fn scan_turns(&self, key: &SessionKey) -> SessionResult<Option<Box<dyn SessionTurnCursor>>> {
        key.validate()?;
        let inner = self.lock()?;
        let Some(history) = inner.index.sessions.get(&key.session_id) else {
            return Ok(None);
        };
        if history.key != *key {
            return Err(SessionError::OwnerMismatch);
        }
        Ok(Some(Box::new(ColdCursor {
            inner: self.inner.clone(),
            archive: self.archive.clone(),
            summary: SessionHistorySummary {
                key: key.clone(),
                revision: history.revision,
                turn_count: history.turns.len() as u64,
            },
            records: history.turns.clone(),
            next: 0,
            failed: false,
        })))
    }
    fn snapshot(&self, key: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
        let Some(mut cursor) = self.scan_turns(key)? else {
            return Ok(None);
        };
        let mut snapshot = SessionSnapshot {
            key: key.clone(),
            revision: cursor.summary().revision,
            turns: vec![],
        };
        loop {
            let page = cursor.next_page()?;
            if page.is_empty() {
                break;
            }
            snapshot.turns.extend(page);
        }
        snapshot.validate()?;
        Ok(Some(snapshot))
    }
    fn list_keys(&self, after: Option<&str>, limit: usize) -> SessionResult<Vec<SessionKey>> {
        if !(1..=100).contains(&limit)
            || after
                .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(SessionError::InvalidInput);
        }
        let inner = self.lock()?;
        Ok(inner
            .index
            .sessions
            .range::<str, _>((after.map_or(Unbounded, Excluded), Unbounded))
            .take(limit)
            .map(|(_, h)| h.key.clone())
            .collect())
    }
    fn reset_context(&self, key: Option<&SessionKey>) -> SessionResult<usize> {
        let mut inner = self.lock()?;
        if let Some(key) = key {
            key.validate()?;
            if inner
                .index
                .sessions
                .get(&key.session_id)
                .is_some_and(|h| h.key != *key)
            {
                return Err(SessionError::OwnerMismatch);
            }
        }
        let mut next = inner.resets.clone();
        let mut affected = 0;
        for (id, history) in &inner.index.sessions {
            if key.is_none_or(|key| key == &history.key) && !history.turns.is_empty() {
                next.boundaries.insert(
                    id.clone(),
                    ContextBoundary {
                        key: history.key.clone(),
                        through: history.turns.len() as u64,
                    },
                );
                affected += 1;
            }
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| SessionError::CorruptState)?;
        if self
            .context
            .state_set(CONTEXT_RESETS_STATE_KEY, bytes)
            .is_err()
        {
            deactivate(&mut inner);
            return Err(SessionError::Storage);
        }
        inner.resets = next;
        Ok(affected)
    }
    fn begin(&self, input: SessionInput) -> SessionResult<StartedTurn> {
        input.key.validate()?;
        if input.text.trim().is_empty() {
            return Err(SessionError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let through = inner
            .resets
            .boundaries
            .get(&input.key.session_id)
            .map_or(0, |b| b.through);
        let mut next = inner.index.clone();
        let history = next
            .sessions
            .entry(input.key.session_id.clone())
            .or_insert_with(|| History {
                key: input.key.clone(),
                revision: 0,
                turns: vec![],
            });
        if history.key != input.key {
            return Err(SessionError::OwnerMismatch);
        }
        let mut messages = vec![];
        for (position, record) in history.turns.iter().enumerate() {
            // 被清空的正文不进入新请求；仍检查末轮是否在途。
            if (position as u64) < through && position + 1 != history.turns.len() {
                continue;
            }
            let turn = checked(&mut inner, load(self.archive.as_ref(), record, position))?;
            if turn.status == SessionTurnStatus::Pending {
                return Err(SessionError::Busy);
            }
            if turn.id > through {
                messages.extend(into_history(turn));
            }
        }
        let id = (history.turns.len() as u64)
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        let turn = SessionTurn {
            id,
            input: input.text,
            status: SessionTurnStatus::Pending,
        };
        history.turns.push(freeze(self.archive.as_ref(), &turn)?);
        history.revision = history
            .revision
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        let started = StartedTurn {
            lease: TurnLease {
                key: input.key,
                turn_id: id,
            },
            revision: history.revision,
            history: messages,
        };
        self.save(&mut inner, next)?;
        Ok(started)
    }
    fn complete(&self, lease: &TurnLease, messages: Vec<ChatMessage>) -> SessionResult<()> {
        self.finish(lease, SessionTurnStatus::Completed { messages })
    }
    fn fail(&self, lease: &TurnLease, failure: SessionFailure) -> SessionResult<()> {
        self.finish(lease, SessionTurnStatus::Failed { failure })
    }
}
