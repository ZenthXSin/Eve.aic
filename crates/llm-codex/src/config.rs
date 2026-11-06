use eve_llm_api::LlmError;
use reqwest::Url;
use std::time::Duration;

/// Eve 的组合配置；凭据由构造函数单独接收。
#[derive(Clone, Debug)]
pub struct CodexConfig {
    pub model: String,
    /// 完整 Responses 端点。Codex 0.162.0 的原客户端不支持 Chat Completions。
    pub responses_url: String,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    /// None 不发 reasoning 字段。永远不添加 temperature。
    pub reasoning_effort: Option<String>,
    pub max_output_tokens: Option<u32>,
}

impl CodexConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            responses_url: "https://api.openai.com/v1/responses".into(),
            request_timeout: Duration::from_secs(60),
            max_response_bytes: 8 * 1024 * 1024,
            reasoning_effort: None,
            max_output_tokens: None,
        }
    }

    pub fn with_base_url(mut self, base: &str) -> Result<Self, LlmError> {
        let mut url = Url::parse(base).map_err(|_| invalid_url())?;
        let path = url.path().trim_end_matches('/');
        let path = if path.is_empty() {
            "/v1/responses".to_owned()
        } else if path.ends_with("/chat/completions") {
            return Err(LlmError::Configuration(
                "Codex 原适配器只支持 Responses，不能使用 Chat Completions 端点".into(),
            ));
        } else if path.ends_with("/responses") {
            path.to_owned()
        } else {
            format!("{path}/responses")
        };
        url.set_path(&path);
        self.responses_url = url.to_string();
        Ok(self)
    }

    pub(crate) fn endpoint(&self, authenticated: bool) -> Result<Endpoint, LlmError> {
        if self.model.trim().is_empty()
            || self.model.len() > 512
            || self.model.chars().any(char::is_control)
            || self.request_timeout.is_zero()
            || self.max_response_bytes == 0
            || self.max_output_tokens == Some(0)
        {
            return Err(LlmError::Configuration(
                "模型、期限和响应上限必须有效".into(),
            ));
        }
        if self.reasoning_effort.as_deref().is_some_and(|effort| {
            effort.is_empty()
                || effort.len() > 64
                || effort.chars().any(char::is_whitespace)
                || effort.chars().any(char::is_control)
        }) {
            return Err(LlmError::Configuration(
                "reasoning effort 必须是非空的单个配置值".into(),
            ));
        }
        let mut endpoint = Url::parse(&self.responses_url).map_err(|_| invalid_url())?;
        let loopback = endpoint.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !(endpoint.scheme() == "https"
            || endpoint.scheme() == "http" && (loopback || !authenticated))
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(invalid_url());
        }
        let path = endpoint.path().trim_end_matches('/');
        let Some(root) = path.strip_suffix("/responses") else {
            return Err(LlmError::Configuration(
                "Codex 原适配器要求完整 Responses 端点".into(),
            ));
        };
        let root = root.to_owned();
        endpoint.set_path(&root);
        Ok(Endpoint {
            base_url: endpoint.as_str().trim_end_matches('/').to_owned(),
            loopback,
        })
    }
}

pub(crate) struct Endpoint {
    pub base_url: String,
    pub loopback: bool,
}

fn invalid_url() -> LlmError {
    LlmError::Configuration(
        "Codex URL 必须是无用户信息、查询或片段的 HTTPS，或受支持的本地 HTTP".into(),
    )
}

/// Eve 的分阶段期限，传给原 HTTP/SSE 客户端；所有重试共用 total。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamTimeoutPolicy {
    pub first_response: Duration,
    pub idle: Duration,
    pub total: Duration,
}

impl StreamTimeoutPolicy {
    pub fn validate(self) -> Result<(), LlmError> {
        if self.first_response.is_zero()
            || self.idle.is_zero()
            || self.total.is_zero()
            || self.first_response > self.total
            || self.idle > self.total
        {
            return Err(LlmError::Configuration(
                "流式首包、停顿期限须大于零且不超过总期限".into(),
            ));
        }
        Ok(())
    }
}
