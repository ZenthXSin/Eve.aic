//! 直接使用 Codex 0.162.0 原 HTTP、重试和 Responses SSE 实现的独立 Provider。
//!
//! vendor 原文件保留 Apache-2.0 与 SHA-256 清单；此 crate 只做 Eve 类型和资源边界的组合。
mod auth;
mod config;
mod error;
mod event_guard;
mod mapping;
mod session;
mod stream;
mod strict_json;
mod transport;

use codex_api::{Compression, ResponsesClient};
use codex_client::{CodexHttpClient, Provider, ReqwestTransport, RetryConfig};
pub use config::{CodexConfig, StreamTimeoutPolicy};
use eve_llm_api::{
    LlmError, LlmFuture, LlmProvider, ModelRequest, ModelResponse, ModelTextSink,
    ModelUsageObserver,
};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

/// 不实现 Debug，不持有全局请求会话；会话由每个 ModelRequest 明确携带。
pub struct CodexProvider {
    config: CodexConfig,
    base_url: String,
    auth: codex_api::SharedAuthProvider,
    transport: ReqwestTransport,
    turns: session::TurnStates,
    transient_retries: bool,
    timing_diagnostics: bool,
    stream_timeouts: Option<StreamTimeoutPolicy>,
    usage: Option<(Arc<dyn ModelUsageObserver>, String)>,
}

impl CodexProvider {
    pub fn new(config: CodexConfig, api_key: impl AsRef<str>) -> Result<Self, LlmError> {
        Self::build(config, Some(api_key.as_ref()))
    }

    pub fn anonymous(config: CodexConfig) -> Result<Self, LlmError> {
        Self::build(config, None)
    }

    fn build(config: CodexConfig, key: Option<&str>) -> Result<Self, LlmError> {
        let endpoint = config.endpoint(key.is_some())?;
        let auth = Arc::new(auth::FixedAuth::new(key)?);
        codex_utils_rustls_provider::ensure_rustls_crypto_provider();
        let mut builder = reqwest::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never());
        if endpoint.loopback {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|_| LlmError::Configuration("无法创建 Codex HTTP 客户端".into()))?;
        let transport = ReqwestTransport::from_http_client(
            CodexHttpClient::new_without_request_logging(client),
        );
        Ok(Self {
            config,
            base_url: endpoint.base_url,
            auth,
            transport,
            turns: session::TurnStates::default(),
            transient_retries: false,
            timing_diagnostics: false,
            stream_timeouts: None,
            usage: None,
        })
    }

    pub fn with_usage_observer(
        mut self,
        observer: Arc<dyn ModelUsageObserver>,
        role: impl Into<String>,
    ) -> Self {
        self.usage = Some((observer, role.into()));
        self
    }

    pub fn with_transient_retries(mut self) -> Self {
        self.transient_retries = true;
        self
    }

    pub fn with_timing_diagnostics(mut self) -> Self {
        self.timing_diagnostics = true;
        self
    }

    /// 原 ResponsesClient 始终使用 SSE；保留统一组合调用形式。
    pub fn with_streamed_completions(self) -> Self {
        self
    }

    pub fn with_stream_timeouts(mut self, policy: StreamTimeoutPolicy) -> Result<Self, LlmError> {
        policy.validate()?;
        self.stream_timeouts = Some(policy);
        Ok(self)
    }

    async fn request(
        &self,
        request: ModelRequest,
        json_mode: bool,
        sink: Option<&dyn ModelTextSink>,
    ) -> Result<ModelResponse, LlmError> {
        let session = request.session.as_ref().ok_or_else(|| {
            LlmError::Configuration(
                "Codex 原适配器需要宿主提供独立 ModelSession；不能使用随机或全局会话".into(),
            )
        })?;
        let state = self.turns.get(session)?;
        let policy = self.stream_timeouts.unwrap_or(StreamTimeoutPolicy {
            first_response: self.config.request_timeout,
            idle: self.config.request_timeout,
            total: self.config.request_timeout,
        });
        let started = Instant::now();
        let deadline = started + policy.total;
        let wire = Arc::new(transport::WireState::default());
        let guard = Arc::new(event_guard::EventGuard::default());
        let result = timeout_at(deadline, async {
            // 同一轮的多个模型调用串行读写原服务器 token；不同任务/角色不共用此锁。
            let _turn_guard = state.gate.lock().await;
            let body = mapping::encode(&self.config, &request, session, json_mode)?;
            let headers = session::headers(session, &state)?;
            let transport = transport::BoundedTransport {
                inner: self.transport.clone(),
                policy,
                started,
                max_bytes: self.config.max_response_bytes,
                state: wire.clone(),
                timing: self.timing_diagnostics,
            };
            let provider = Provider {
                name: "eve-codex".into(),
                base_url: self.base_url.clone(),
                query_params: None,
                headers: http::HeaderMap::new(),
                retry: RetryConfig {
                    max_attempts: if self.transient_retries { 3 } else { 0 },
                    base_delay: Duration::from_millis(200),
                    retry_429: true,
                    retry_5xx: true,
                    retry_transport: true,
                },
                // transport 已按非空字节实行 idle；原 parser 的事件停顿等待不比总期限早。
                stream_idle_timeout: policy.total,
            };
            let client = ResponsesClient::new(transport, provider, self.auth.clone())
                .with_telemetry(None, Some(guard.clone()));
            if self.timing_diagnostics {
                transport::timing("request_start", started, 0);
            }
            let native_stream = client
                .stream(body, headers, Compression::None, Some(state.token.clone()))
                .await
                .map_err(error::api)?;
            let completed = stream::collect(
                native_stream,
                sink,
                self.config.max_response_bytes,
                &wire,
                &guard,
            )
            .await?;
            if let Some((observer, role)) = &self.usage {
                observer.reported(role, completed.usage);
            }
            Ok(completed.response)
        })
        .await
        .unwrap_or(Err(LlmError::ProviderTimeout));
        if self.timing_diagnostics {
            transport::timing(
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
                started,
                wire.received(),
            );
        }
        result
    }
}

impl LlmProvider for CodexProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(self.request(request, false, None))
    }

    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(self.request(request, true, None))
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(self.request(request, false, Some(sink)))
    }
}
