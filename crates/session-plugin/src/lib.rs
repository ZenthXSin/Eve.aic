//! 内置会话插件，通过 StateStore 保存版本化历史；不调用模型或依赖文件实现。
mod strict_json;
use eve_llm_api::ChatMessage;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    ServiceId, cleanup,
};
use eve_session_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

pub const SESSION_STATE_KEY: &str = "sessions.v1";
const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    sessions: BTreeMap<String, SessionSnapshot>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            sessions: BTreeMap::new(),
        }
    }
}
struct Inner {
    document: Document,
    active: bool,
}
struct StoredSessions {
    context: PluginContext,
    inner: Mutex<Inner>,
}
impl StoredSessions {
    fn open(context: PluginContext) -> SessionResult<Self> {
        let mut document: Document = match context
            .state_get(SESSION_STATE_KEY)
            .map_err(|_| SessionError::Storage)?
        {
            None => Document::default(),
            Some(bytes) => {
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| SessionError::CorruptState)?;
                serde_json::from_value(value).map_err(|_| SessionError::CorruptState)?
            }
        };
        if document.format_version != FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion);
        }
        let mut recovered = false;
        for (id, snapshot) in &mut document.sessions {
            snapshot
                .validate()
                .map_err(|_| SessionError::CorruptState)?;
            if id != &snapshot.key.session_id {
                return Err(SessionError::CorruptState);
            }
            if let Some(turn) = snapshot.turns.last_mut()
                && turn.status == SessionTurnStatus::Pending
            {
                turn.status = SessionTurnStatus::Interrupted;
                snapshot.revision = snapshot
                    .revision
                    .checked_add(1)
                    .ok_or(SessionError::LimitReached)?;
                recovered = true;
            }
        }
        if recovered {
            let bytes = serde_json::to_vec(&document).map_err(|_| SessionError::CorruptState)?;
            context
                .state_set(SESSION_STATE_KEY, bytes)
                .map_err(|_| SessionError::Storage)?;
        }
        Ok(Self {
            context,
            inner: Mutex::new(Inner {
                document,
                active: true,
            }),
        })
    }
    fn lock(&self) -> SessionResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| SessionError::Unavailable)?;
        if !inner.active {
            return Err(SessionError::Unavailable);
        }
        Ok(inner)
    }
    fn save(&self, inner: &mut Inner, document: Document) -> SessionResult<()> {
        let bytes = serde_json::to_vec(&document).map_err(|_| SessionError::CorruptState)?;
        self.context
            .state_set(SESSION_STATE_KEY, bytes)
            .map_err(|_| SessionError::Storage)?;
        inner.document = document;
        Ok(())
    }
    fn finish(&self, lease: &TurnLease, status: SessionTurnStatus) -> SessionResult<()> {
        lease.key.validate()?;
        let mut inner = self.lock()?;
        let mut next = inner.document.clone();
        let snapshot = next
            .sessions
            .get_mut(&lease.key.session_id)
            .ok_or(SessionError::StaleTurn)?;
        if snapshot.key != lease.key {
            return Err(SessionError::OwnerMismatch);
        }
        let turn = snapshot.turns.last_mut().ok_or(SessionError::StaleTurn)?;
        if turn.id != lease.turn_id || turn.status != SessionTurnStatus::Pending {
            return Err(SessionError::StaleTurn);
        }
        if let SessionTurnStatus::Completed { messages } = &status {
            validate_completed_turn(&turn.input, messages)?;
        }
        turn.status = status;
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        self.save(&mut inner, next)
    }
    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("会话锁不可用".into()))?
            .active = false;
        Ok(())
    }
}
impl SessionService for StoredSessions {
    fn snapshot(&self, key: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
        key.validate()?;
        let inner = self.lock()?;
        let snapshot = inner.document.sessions.get(&key.session_id);
        if snapshot.is_some_and(|s| s.key != *key) {
            return Err(SessionError::OwnerMismatch);
        }
        Ok(snapshot.cloned())
    }
    fn begin(&self, input: SessionInput) -> SessionResult<StartedTurn> {
        input.key.validate()?;
        if input.text.trim().is_empty() {
            return Err(SessionError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.document.clone();
        let snapshot = next
            .sessions
            .entry(input.key.session_id.clone())
            .or_insert_with(|| SessionSnapshot {
                key: input.key.clone(),
                revision: 0,
                turns: vec![],
            });
        if snapshot.key != input.key {
            return Err(SessionError::OwnerMismatch);
        }
        if snapshot
            .turns
            .last()
            .is_some_and(|turn| turn.status == SessionTurnStatus::Pending)
        {
            return Err(SessionError::Busy);
        }
        let id = snapshot.turns.last().map_or(Ok(1), |turn| {
            turn.id.checked_add(1).ok_or(SessionError::LimitReached)
        })?;
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or(SessionError::LimitReached)?;
        snapshot.turns.push(SessionTurn {
            id,
            input: input.text,
            status: SessionTurnStatus::Pending,
        });
        let result = StartedTurn {
            lease: TurnLease {
                key: input.key,
                turn_id: id,
            },
            revision: snapshot.revision,
            history: snapshot.history(),
        };
        self.save(&mut inner, next)?;
        Ok(result)
    }
    fn complete(&self, lease: &TurnLease, messages: Vec<ChatMessage>) -> SessionResult<()> {
        self.finish(lease, SessionTurnStatus::Completed { messages })
    }
    fn fail(&self, lease: &TurnLease, failure: SessionFailure) -> SessionResult<()> {
        self.finish(lease, SessionTurnStatus::Failed { failure })
    }
}

pub struct SessionPlugin {
    manifest: PluginManifest,
}
impl SessionPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(SESSION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
        })
    }
}
impl Plugin for SessionPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let service = Arc::new(
                StoredSessions::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = service.clone();
            context.cleanup(cleanup(move || async move { to_close.close() }))?;
            context.provide_service(
                ServiceId::new(SESSION_SERVICE_ID)?,
                SessionServiceHandle(service),
            )?;
            Ok(None)
        })
    }
}
