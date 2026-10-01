//! OpenAI 兼容 Responses/Chat Completions 适配器；厂商格式不进入通用协议或 Kernel。
//!
//! 网络和转换均在返回的 Future 中执行。取消等待会丢弃本地请求，不保证远端停止。
use eve_llm_api::{LlmError, LlmFuture, LlmProvider, ModelRequest, ModelResponse, ModelTextSink};
use reqwest::{Client, Url, header::HeaderValue, redirect::Policy};
use std::time::Duration;

mod chat;
mod stream;
mod strict_json;
mod wire;

/// 显式选择线协议；不根据模型名猜测，也不在失败时切换协议。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenAiProtocol {
    #[default]
    Responses,
    ChatCompletions,
}

/// 普通配置。凭据独立交给构造函数，不进入此对象、消息或诊断。
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub model: String,
    pub protocol: OpenAiProtocol,
    /// 完整所选协议端点；保留历史字段名，Chat 模式使用 chat/completions URL。
    pub responses_url: String,
    /// 独立传输期限；LlmHost 的期限仍可更早终止等待。
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    /// None 保持服务默认；窄协议验收可显式选择支持 none 的模型。
    pub reasoning_effort: Option<String>,
    pub max_output_tokens: Option<u32>,
}

impl OpenAiConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            protocol: OpenAiProtocol::Responses,
            responses_url: "https://api.openai.com/v1/responses".into(),
            request_timeout: Duration::from_secs(60),
            max_response_bytes: 8 * 1024 * 1024,
            reasoning_effort: None,
            max_output_tokens: None,
        }
    }

    pub fn chat(model: impl Into<String>) -> Self {
        Self {
            protocol: OpenAiProtocol::ChatCompletions,
            responses_url: "https://api.openai.com/v1/chat/completions".into(),
            ..Self::new(model)
        }
    }

    /// 宿主可提供 API 根地址、版本路径或完整所选协议 URL。
    pub fn with_base_url(mut self, base: &str) -> Result<Self, LlmError> {
        let mut url =
            Url::parse(base).map_err(|_| LlmError::Configuration("OpenAI base URL 无效".into()))?;
        let (suffix, other) = match self.protocol {
            OpenAiProtocol::Responses => ("/responses", "/chat/completions"),
            OpenAiProtocol::ChatCompletions => ("/chat/completions", "/responses"),
        };
        let path = url.path().trim_end_matches('/');
        let path = if path.is_empty() {
            format!("/v1{suffix}")
        } else if path.ends_with(suffix) {
            path.to_owned()
        } else if path.ends_with(other) {
            return Err(LlmError::Configuration("API 协议与完整端点不匹配".into()));
        } else {
            format!("{path}{suffix}")
        };
        url.set_path(&path);
        self.responses_url = url.to_string();
        Ok(self)
    }
}

/// 不实现 Debug，避免凭据和请求内容通过调试输出外泄。
pub struct OpenAiProvider {
    config: OpenAiConfig,
    endpoint: Url,
    authorization: HeaderValue,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig, api_key: impl AsRef<str>) -> Result<Self, LlmError> {
        if config.model.trim().is_empty()
            || config.request_timeout.is_zero()
            || config.max_response_bytes == 0
            || config.max_output_tokens == Some(0)
        {
            return Err(LlmError::Configuration(
                "模型、期限和响应上限必须有效".into(),
            ));
        }
        if config.reasoning_effort.as_deref().is_some_and(|effort| {
            !matches!(
                effort,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        }) {
            return Err(LlmError::Configuration(
                "不支持的 reasoning effort 配置".into(),
            ));
        }
        let endpoint = Url::parse(&config.responses_url)
            .map_err(|_| LlmError::Configuration("OpenAI URL 无效".into()))?;
        let loopback = endpoint.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(LlmError::Configuration(
                "OpenAI URL 必须是无用户信息、查询或片段的 HTTPS，或 loopback HTTP".into(),
            ));
        }
        let key = api_key.as_ref();
        if key.trim().is_empty() || key.chars().any(char::is_whitespace) {
            return Err(LlmError::Configuration("API 凭据不能为空或含空白".into()));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| LlmError::Configuration("API 凭据格式无效".into()))?;
        authorization.set_sensitive(true);
        let mut builder = Client::builder()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(config.request_timeout);
        // 本地 HTTP 验收不经代理，生产 HTTPS 沿用 reqwest 的代理配置。
        if loopback {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|_| LlmError::Configuration("无法创建 OpenAI HTTP 客户端".into()))?;
        Ok(Self {
            config,
            endpoint,
            authorization,
            client,
        })
    }

    async fn request(
        &self,
        request: ModelRequest,
        sink: Option<&dyn ModelTextSink>,
    ) -> Result<ModelResponse, LlmError> {
        if sink.is_some() && self.config.protocol == OpenAiProtocol::ChatCompletions {
            return Err(LlmError::Unsupported(
                "Chat Completions 流式尚未接入".into(),
            ));
        }
        let mut body = match self.config.protocol {
            OpenAiProtocol::Responses => wire::encode_request(&self.config.model, request)?,
            OpenAiProtocol::ChatCompletions => chat::encode_request(&self.config.model, request)?,
        };
        body["stream"] = serde_json::json!(sink.is_some());
        if let Some(effort) = &self.config.reasoning_effort {
            match self.config.protocol {
                OpenAiProtocol::Responses => {
                    body["reasoning"] = serde_json::json!({"effort": effort});
                }
                OpenAiProtocol::ChatCompletions => {
                    body["reasoning_effort"] = serde_json::json!(effort);
                }
            }
        }
        if let Some(limit) = self.config.max_output_tokens {
            let field = match self.config.protocol {
                OpenAiProtocol::Responses => "max_output_tokens",
                OpenAiProtocol::ChatCompletions => "max_tokens",
            };
            body[field] = serde_json::json!(limit);
        }
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            // 原始响应体、URL 和后端错误可能包含凭据或用户输入，不进入通用诊断。
            return Err(LlmError::Provider(format!(
                "OpenAI HTTP {}",
                status.as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|n| n > self.config.max_response_bytes as u64)
        {
            return Err(LlmError::Provider("OpenAI 响应超过字节上限".into()));
        }
        if let Some(sink) = sink {
            if response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| {
                    !v.split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                })
            {
                return Err(LlmError::Protocol(
                    "Responses 流必须使用 text/event-stream".into(),
                ));
            }
            let mut parser = stream::Sse::new();
            let mut state = stream::ResponseStream::default();
            let mut received = 0;
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                if chunk.len() > self.config.max_response_bytes - received {
                    return Err(LlmError::Provider("OpenAI 响应超过字节上限".into()));
                }
                received += chunk.len();
                for byte in chunk {
                    if let Some((name, data)) = parser.byte(byte)? {
                        match state.event(&name, &data)? {
                            stream::Update::None => {}
                            stream::Update::Text(text) => sink.text_delta(text).await?,
                            // 正常终态立即关闭本地流，不等 EOF，不重连、不重复执行。
                            stream::Update::Completed(response) => return Ok(response),
                        }
                    }
                }
            }
            return Err(LlmError::Protocol("Responses 流在正常终态前结束".into()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > self.config.max_response_bytes - bytes.len() {
                return Err(LlmError::Provider("OpenAI 响应超过字节上限".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        match self.config.protocol {
            OpenAiProtocol::Responses => wire::decode_response(&bytes),
            OpenAiProtocol::ChatCompletions => chat::decode_response(&bytes),
        }
    }
}

impl LlmProvider for OpenAiProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(self.request(request, None))
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            tokio::time::timeout(
                self.config.request_timeout,
                self.request(request, Some(sink)),
            )
            .await
            .map_err(|_| LlmError::ProviderTimeout)?
        })
    }
}

fn transport_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::ProviderTimeout
    } else {
        LlmError::Provider("OpenAI 网络请求失败".into())
    }
}
