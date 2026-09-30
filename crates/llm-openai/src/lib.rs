//! OpenAI Responses 的文本与函数调用适配器；厂商格式不进入通用协议或 Kernel。
//!
//! 网络和转换均在返回的 Future 中执行。取消等待会丢弃本地请求，不保证远端停止。
use eve_llm_api::{LlmError, LlmFuture, LlmProvider, ModelRequest, ModelResponse};
use reqwest::{Client, Url, header::HeaderValue, redirect::Policy};
use std::time::Duration;

mod strict_json;
mod wire;

/// 普通配置。凭据独立交给构造函数，不进入此对象、消息或诊断。
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub model: String,
    /// 完整 Responses URL；默认官方 HTTPS，允许宿主显式指定 HTTPS 或 loopback HTTP。
    pub responses_url: String,
    /// 独立传输期限；LlmHost 的期限仍可更早终止等待。
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
}

impl OpenAiConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            responses_url: "https://api.openai.com/v1/responses".into(),
            request_timeout: Duration::from_secs(60),
            max_response_bytes: 8 * 1024 * 1024,
        }
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
        {
            return Err(LlmError::Configuration(
                "模型、期限和响应上限必须有效".into(),
            ));
        }
        let endpoint = Url::parse(&config.responses_url)
            .map_err(|_| LlmError::Configuration("Responses URL 无效".into()))?;
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
                "Responses URL 必须是无用户信息、查询或片段的 HTTPS，或 loopback HTTP".into(),
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

    async fn request(&self, request: ModelRequest) -> Result<ModelResponse, LlmError> {
        let body = wire::encode_request(&self.config.model, request)?;
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
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > self.config.max_response_bytes - bytes.len() {
                return Err(LlmError::Provider("OpenAI 响应超过字节上限".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        wire::decode_response(&bytes)
    }
}

impl LlmProvider for OpenAiProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(self.request(request))
    }
}

fn transport_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::ProviderTimeout
    } else {
        LlmError::Provider("OpenAI 网络请求失败".into())
    }
}
