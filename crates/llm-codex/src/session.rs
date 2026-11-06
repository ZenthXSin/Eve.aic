use codex_api::build_session_headers;
use eve_llm_api::{LlmError, ModelSession};
use http::{HeaderMap, HeaderValue};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

const MAX_CACHED_TURNS: usize = 1024;

#[derive(Default)]
pub(crate) struct TurnState {
    pub token: Arc<OnceLock<String>>,
    pub gate: tokio::sync::Mutex<()>,
}

struct Entry {
    state: Arc<TurnState>,
    touched: Instant,
}

#[derive(Default)]
pub(crate) struct TurnStates(Mutex<BTreeMap<(String, String), Entry>>);

impl TurnStates {
    pub(crate) fn get(&self, session: &ModelSession) -> Result<Arc<TurnState>, LlmError> {
        session.validate()?;
        // 没有可信轮次时不复用服务器 token；长期身份仍来自同一 ModelSession。
        let Some(turn) = session.turn_id() else {
            return Ok(Arc::new(TurnState::default()));
        };
        let key = (session.session_id().to_owned(), turn.to_owned());
        let mut entries = self
            .0
            .lock()
            .map_err(|_| LlmError::Backend("Codex 会话状态不可用".into()))?;
        if let Some(entry) = entries.get_mut(&key) {
            entry.touched = Instant::now();
            return Ok(entry.state.clone());
        }
        if entries.len() >= MAX_CACHED_TURNS {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.state) == 1)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                return Err(LlmError::Backend("Codex 在途轮次状态已达容量上限".into()));
            }
        }
        let state = Arc::new(TurnState::default());
        entries.insert(
            key,
            Entry {
                state: state.clone(),
                touched: Instant::now(),
            },
        );
        Ok(state)
    }
}

pub(crate) fn headers(session: &ModelSession, state: &TurnState) -> Result<HeaderMap, LlmError> {
    session.validate()?;
    let mut headers = build_session_headers(
        Some(session.session_id().to_owned()),
        Some(session.thread_id().to_owned()),
    );
    headers.insert("x-client-request-id", header(session.client_request_id())?);
    if let Some(token) = state.token.get() {
        if token.len() > 16 * 1024 {
            return Err(LlmError::Protocol("Codex 服务器轮次状态过大".into()));
        }
        let mut value = header(token)?;
        value.set_sensitive(true);
        headers.insert("x-codex-turn-state", value);
    }
    Ok(headers)
}

fn header(value: &str) -> Result<HeaderValue, LlmError> {
    HeaderValue::from_str(value).map_err(|_| LlmError::Protocol("Codex 会话头无效".into()))
}
