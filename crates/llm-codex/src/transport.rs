//! 只绑定 Eve 的资源期限与原 transport 的字节上限；HTTP、重试和 SSE 解析来自 vendor。
use crate::StreamTimeoutPolicy;
use codex_client::{
    HttpTransport, Request, ReqwestTransport, Response, StreamResponse, TransportError,
};
use eve_llm_api::{LlmError, ProviderFailureKind};
use futures::StreamExt;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};
use tokio::time::{Instant, timeout_at};

#[derive(Default)]
pub(crate) struct WireState {
    failure: AtomicU8,
    received: AtomicUsize,
}

impl WireState {
    pub(crate) fn failure(&self) -> Option<LlmError> {
        match self.failure.load(Ordering::Acquire) {
            1 => Some(LlmError::ProviderTimeout),
            2 => Some(crate::error::failed(ProviderFailureKind::ResponseTooLarge)),
            3 => Some(crate::error::failed(ProviderFailureKind::Network)),
            _ => None,
        }
    }

    fn record(&self, error: &TransportError) {
        let kind = match error {
            TransportError::Timeout => 1,
            TransportError::ResponseTooLarge { .. } => 2,
            TransportError::Connection(_) | TransportError::Network(_) => 3,
            _ => return,
        };
        self.failure.store(kind, Ordering::Release);
    }

    pub(crate) fn received(&self) -> usize {
        self.received.load(Ordering::Relaxed)
    }
}

pub(crate) struct BoundedTransport {
    pub inner: ReqwestTransport,
    pub policy: StreamTimeoutPolicy,
    pub started: Instant,
    pub max_bytes: usize,
    pub state: Arc<WireState>,
    pub timing: bool,
}

impl BoundedTransport {
    fn prepare(&self, mut request: Request) -> Request {
        request.timeout =
            Some((self.started + self.policy.total).saturating_duration_since(Instant::now()));
        // 这是原 ReqwestTransport 的内置限额，同时覆盖正常流和 HTTP 错误正文。
        request.response_body_limit_bytes = Some(self.max_bytes);
        request
    }
}

impl HttpTransport for BoundedTransport {
    async fn execute(&self, request: Request) -> Result<Response, TransportError> {
        timeout_at(
            self.started + self.policy.total,
            self.inner.execute(self.prepare(request)),
        )
        .await
        .map_err(|_| TransportError::Timeout)?
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        let response = timeout_at(
            self.started + self.policy.first_response,
            self.inner.stream(self.prepare(request)),
        )
        .await
        .map_err(|_| TransportError::Timeout)??;
        let StreamResponse {
            status,
            headers,
            bytes,
        } = response;
        if self.timing {
            timing("response_headers", self.started, 0);
        }
        self.state.failure.store(0, Ordering::Release);
        let state = self.state.clone();
        let started = self.started;
        let policy = self.policy;
        let diagnostics = self.timing;
        let stream = futures::stream::try_unfold(
            (bytes, false, started + policy.first_response),
            move |(mut bytes, mut received, mut deadline)| {
                let state = state.clone();
                async move {
                    loop {
                        let chunk = match timeout_at(deadline, bytes.next()).await {
                            Ok(Some(Ok(chunk))) => chunk,
                            Ok(Some(Err(error))) => {
                                state.record(&error);
                                return Err(error);
                            }
                            Ok(None) => return Ok(None),
                            Err(_) => {
                                state.record(&TransportError::Timeout);
                                return Err(TransportError::Timeout);
                            }
                        };
                        if chunk.is_empty() {
                            continue;
                        }
                        state.received.fetch_add(chunk.len(), Ordering::Relaxed);
                        if !received && diagnostics {
                            timing("first_bytes", started, state.received());
                        }
                        received = true;
                        deadline = (Instant::now() + policy.idle).min(started + policy.total);
                        return Ok(Some((chunk, (bytes, received, deadline))));
                    }
                }
            },
        );
        Ok(StreamResponse {
            status,
            headers,
            bytes: Box::pin(stream),
        })
    }
}

pub(crate) fn timing(phase: &str, started: Instant, received: usize) {
    eprintln!(
        "EVE_MODEL_TIMING {}",
        serde_json::json!({
            "adapter": "codex", "phase": phase,
            "elapsed_ms": started.elapsed().as_millis(), "received_bytes": received,
        })
    );
}
