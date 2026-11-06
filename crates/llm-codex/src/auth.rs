use codex_api::AuthProvider;
use eve_llm_api::LlmError;
use http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

// 不实现 Debug；原客户端的每次传输重试只重用这份宿主提供的凭据。
pub(crate) struct FixedAuth(Option<HeaderValue>);

impl FixedAuth {
    pub(crate) fn new(key: Option<&str>) -> Result<Self, LlmError> {
        let authorization = key
            .map(|key| {
                if key.trim().is_empty() || key.chars().any(char::is_whitespace) {
                    return Err(LlmError::Configuration("API 凭据不能为空或含空白".into()));
                }
                let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| LlmError::Configuration("API 凭据格式无效".into()))?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        Ok(Self(authorization))
    }
}

impl AuthProvider for FixedAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        if let Some(value) = &self.0 {
            headers.insert(AUTHORIZATION, value.clone());
        }
    }
}
