//! 主/辅模型的实现选择与共同构造；目录查询是独立的只读 HTTP 能力。
use eve_llm_api::{LlmError, LlmProvider, ModelUsageObserver};
use eve_llm_codex::{CodexConfig, CodexProvider};
use eve_llm_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider, StreamTimeoutPolicy};
use std::{sync::Arc, time::Duration};

/// 普通配置不含凭据；角色 provider 决定实现，协议或模型名不能替换这个选择。
#[derive(Clone, Debug)]
pub(crate) enum ProviderConfig {
    OpenAi(OpenAiConfig),
    Codex(CodexConfig),
}

impl ProviderConfig {
    pub(crate) fn adapter(&self) -> &'static str {
        match self {
            Self::OpenAi(_) => "openai",
            Self::Codex(_) => "codex",
        }
    }

    pub(crate) fn model(&self) -> &str {
        match self {
            Self::OpenAi(config) => &config.model,
            Self::Codex(config) => &config.model,
        }
    }

    pub(crate) fn set_model(&mut self, model: String) {
        match self {
            Self::OpenAi(config) => config.model = model,
            Self::Codex(config) => config.model = model,
        }
    }

    pub(crate) fn endpoint(&self) -> &str {
        match self {
            Self::OpenAi(config) => &config.responses_url,
            Self::Codex(config) => &config.responses_url,
        }
    }

    pub(crate) fn protocol(&self) -> &'static str {
        match self {
            Self::OpenAi(config) if config.protocol == OpenAiProtocol::ChatCompletions => "chat",
            Self::OpenAi(_) | Self::Codex(_) => "responses",
        }
    }

    pub(crate) fn uses_sse(&self, streamed_completions: bool) -> bool {
        matches!(self, Self::Codex(_)) || streamed_completions
    }

    pub(crate) fn request_timeout(&self) -> Duration {
        match self {
            Self::OpenAi(config) => config.request_timeout,
            Self::Codex(config) => config.request_timeout,
        }
    }

    pub(crate) fn cap_timeout(&mut self, cap: Duration) {
        match self {
            Self::OpenAi(config) => config.request_timeout = config.request_timeout.min(cap),
            Self::Codex(config) => config.request_timeout = config.request_timeout.min(cap),
        }
    }

    pub(crate) fn max_output_tokens(&self) -> Option<u32> {
        match self {
            Self::OpenAi(config) => config.max_output_tokens,
            Self::Codex(config) => config.max_output_tokens,
        }
    }

    pub(crate) fn set_max_output_tokens(&mut self, limit: Option<u32>) {
        match self {
            Self::OpenAi(config) => config.max_output_tokens = limit,
            Self::Codex(config) => config.max_output_tokens = limit,
        }
    }

    #[cfg(test)]
    pub(crate) fn reasoning_effort(&self) -> Option<&str> {
        match self {
            Self::OpenAi(config) => config.reasoning_effort.as_deref(),
            Self::Codex(config) => config.reasoning_effort.as_deref(),
        }
    }

    pub(crate) fn build(
        self,
        credential: Option<&str>,
        options: &ProviderOptions,
    ) -> Result<Arc<dyn LlmProvider>, LlmError> {
        match self {
            Self::OpenAi(config) => {
                let mut provider = match credential {
                    Some(key) => OpenAiProvider::new(config, key)?,
                    None => OpenAiProvider::anonymous(config)?,
                };
                if let Some(policy) = options.stream_timeouts {
                    provider = provider.with_stream_timeout_policy(policy)?;
                }
                if options.streamed_completions {
                    provider = provider.with_streamed_completions();
                }
                if options.transient_retries {
                    provider = provider.with_transient_retries();
                }
                if options.recover_tool_arguments {
                    provider = provider.with_tool_argument_recovery();
                }
                if options.timing_diagnostics {
                    provider = provider.with_timing_diagnostics();
                }
                if let Some((observer, role)) = &options.usage {
                    provider = provider.with_usage_observer(observer.clone(), role);
                }
                Ok(Arc::new(provider))
            }
            Self::Codex(config) => {
                let mut provider = match credential {
                    Some(key) => CodexProvider::new(config, key)?,
                    None => CodexProvider::anonymous(config)?,
                };
                if let Some(policy) = options.stream_timeouts {
                    provider =
                        provider.with_stream_timeouts(eve_llm_codex::StreamTimeoutPolicy {
                            first_response: policy.first_response,
                            idle: policy.idle,
                            total: policy.total,
                        })?;
                }
                // 原 Codex 客户端始终使用 SSE，布尔值不伪造一个非流式 HTTP 路径。
                if options.transient_retries {
                    provider = provider.with_transient_retries();
                }
                if options.timing_diagnostics {
                    provider = provider.with_timing_diagnostics();
                }
                if let Some((observer, role)) = &options.usage {
                    provider = provider.with_usage_observer(observer.clone(), role);
                }
                Ok(Arc::new(provider))
            }
        }
    }

    pub(crate) fn catalog(&self, credential: Option<&str>) -> Result<ModelCatalog, LlmError> {
        let maximum = match self {
            Self::OpenAi(config) => config.max_response_bytes,
            Self::Codex(config) => config.max_response_bytes,
        };
        ModelCatalog::new(self.endpoint(), credential, self.request_timeout(), maximum)
    }
}

#[derive(Default)]
pub(crate) struct ProviderOptions {
    pub(crate) streamed_completions: bool,
    pub(crate) transient_retries: bool,
    pub(crate) recover_tool_arguments: bool,
    pub(crate) stream_timeouts: Option<StreamTimeoutPolicy>,
    pub(crate) timing_diagnostics: bool,
    pub(crate) usage: Option<(Arc<dyn ModelUsageObserver>, String)>,
}

/// 兼容 GET /models 的目录读取器；无 LlmProvider，也不发送推理请求或会话信息。
pub(crate) struct ModelCatalog {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    authorization: Option<reqwest::header::HeaderValue>,
    timeout: Duration,
    maximum: usize,
}

impl ModelCatalog {
    fn new(
        endpoint: &str,
        credential: Option<&str>,
        timeout: Duration,
        maximum: usize,
    ) -> Result<Self, LlmError> {
        let invalid = || LlmError::Configuration("模型目录配置无效".into());
        let mut endpoint = reqwest::Url::parse(endpoint).map_err(|_| invalid())?;
        let loopback = endpoint.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !(endpoint.scheme() == "https"
            || endpoint.scheme() == "http" && (loopback || credential.is_none()))
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || timeout.is_zero()
            || maximum == 0
        {
            return Err(invalid());
        }
        let path = endpoint.path().trim_end_matches('/');
        let root = path
            .strip_suffix("/chat/completions")
            .or_else(|| path.strip_suffix("/responses"))
            .ok_or_else(invalid)?;
        let path = format!("{root}/models");
        endpoint.set_path(&path);
        let authorization = credential
            .map(|key| {
                if key.is_empty() || key.chars().any(char::is_whitespace) {
                    return Err(invalid());
                }
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| invalid())?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never());
        if loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| invalid())?;
        Ok(Self {
            client,
            endpoint,
            authorization,
            timeout,
            maximum,
        })
    }

    pub(crate) async fn list(&self, limit: usize) -> Result<Vec<String>, LlmError> {
        let mut request = self.client.get(self.endpoint.clone()).timeout(self.timeout);
        if let Some(authorization) = &self.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        let transport = |_| LlmError::Backend("模型目录请求失败".into());
        let mut response = request.send().await.map_err(transport)?;
        if !response.status().is_success() {
            return Err(LlmError::Backend(format!(
                "模型目录返回 HTTP {}",
                response.status().as_u16()
            )));
        }
        let oversized = || LlmError::Protocol("模型目录超过响应上限".into());
        if response
            .content_length()
            .is_some_and(|bytes| bytes > self.maximum as u64)
        {
            return Err(oversized());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if chunk.len() > self.maximum.saturating_sub(bytes.len()) {
                return Err(oversized());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| LlmError::Protocol("模型目录不是 JSON".into()))?;
        let entries = body["data"]
            .as_array()
            .ok_or_else(|| LlmError::Protocol("模型目录缺少 data 数组".into()))?;
        let mut models = Vec::new();
        for id in entries.iter().filter_map(|entry| entry["id"].as_str()) {
            if models.len() == limit {
                break;
            }
            if !id.is_empty()
                && id.len() <= 200
                && !id
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
                && !models.iter().any(|known| known == id)
            {
                models.push(id.to_owned());
            }
        }
        Ok(models)
    }
}
