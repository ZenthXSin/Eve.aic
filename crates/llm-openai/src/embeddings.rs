//! OpenAI 兼容的 `/embeddings` 适配：一次调用一次请求，严格核对返回条数、维度与数值。
use crate::strict_json;
use eve_semantic_api::{
    EmbedFuture, EmbedResult, EmbeddingError, EmbeddingProfile, EmbeddingProvider, validate_inputs,
    validate_vectors,
};
use reqwest::{Client, Url, header::HeaderValue, redirect::Policy};
use serde::Deserialize;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// 不实现 Debug，避免凭据外泄。
pub struct OpenAiEmbeddings {
    profile: EmbeddingProfile,
    endpoint: Url,
    authorization: HeaderValue,
    client: Client,
    timeout: Duration,
}

impl OpenAiEmbeddings {
    /// `base` 可以是 API 根地址、版本路径、完整 embeddings URL，或同一服务的对话端点；
    /// 对话端点会换成同级的 `/embeddings`。请求带上 `dimensions` 以固定维度。
    pub fn new(
        base: &str,
        profile: EmbeddingProfile,
        api_key: impl AsRef<str>,
        timeout: Duration,
    ) -> EmbedResult<Self> {
        profile.validate()?;
        if timeout.is_zero() {
            return Err(EmbeddingError::InvalidInput);
        }
        let mut endpoint = Url::parse(base).map_err(|_| EmbeddingError::InvalidInput)?;
        let path = endpoint.path().trim_end_matches('/');
        let path = path
            .strip_suffix("/chat/completions")
            .or_else(|| path.strip_suffix("/responses"))
            .unwrap_or(path);
        let path = if path.is_empty() {
            "/v1/embeddings".to_owned()
        } else if path.ends_with("/embeddings") {
            path.to_owned()
        } else {
            format!("{path}/embeddings")
        };
        endpoint.set_path(&path);
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
            return Err(EmbeddingError::InvalidInput);
        }
        let key = api_key.as_ref();
        if key.trim().is_empty() || key.chars().any(char::is_whitespace) {
            return Err(EmbeddingError::InvalidInput);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| EmbeddingError::InvalidInput)?;
        authorization.set_sensitive(true);
        let mut builder = Client::builder()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout);
        if loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| EmbeddingError::InvalidInput)?;
        Ok(Self {
            profile,
            endpoint,
            authorization,
            client,
            timeout,
        })
    }

    async fn request(&self, inputs: &[String]) -> EmbedResult<Vec<Vec<f32>>> {
        validate_inputs(inputs)?;
        let body = serde_json::json!({
            "model": self.profile.model,
            "input": inputs,
            "dimensions": self.profile.dimensions,
            "encoding_format": "float",
        });
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        // 响应体、URL 与后端错误可能含凭据或用户正文，不进入诊断。
        if !response.status().is_success() {
            return Err(EmbeddingError::Provider);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(EmbeddingError::InvalidOutput);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(EmbeddingError::InvalidOutput);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = strict_json::from_slice(&bytes).map_err(|_| EmbeddingError::InvalidOutput)?;
        let decoded: Response =
            serde_json::from_value(value).map_err(|_| EmbeddingError::InvalidOutput)?;
        let mut data = decoded.data;
        data.sort_by_key(|item| item.index);
        if data
            .iter()
            .enumerate()
            .any(|(position, item)| item.index != position)
        {
            return Err(EmbeddingError::InvalidOutput);
        }
        let vectors: Vec<Vec<f32>> = data.into_iter().map(|item| item.embedding).collect();
        validate_vectors(&self.profile, inputs.len(), &vectors)?;
        Ok(vectors)
    }
}

impl EmbeddingProvider for OpenAiEmbeddings {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(self.timeout, self.request(inputs))
                .await
                .map_err(|_| EmbeddingError::Timeout)?
        })
    }
}

/// 只取用到的字段；服务可以附带 usage、model 等其他字段。
#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    index: usize,
    embedding: Vec<f32>,
}

fn transport_error(error: reqwest::Error) -> EmbeddingError {
    if error.is_timeout() {
        EmbeddingError::Timeout
    } else {
        EmbeddingError::Provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> EmbeddingProfile {
        EmbeddingProfile {
            model: "embed-small".into(),
            dimensions: 3,
        }
    }

    #[test]
    fn endpoints_resolve_to_sibling_embeddings_path_and_reject_unsafe_urls() {
        for (base, expected) in [
            ("http://127.0.0.1:9", "http://127.0.0.1:9/v1/embeddings"),
            (
                "https://api.example/v1",
                "https://api.example/v1/embeddings",
            ),
            (
                "https://api.example/v1/chat/completions",
                "https://api.example/v1/embeddings",
            ),
            (
                "https://api.example/v1/responses",
                "https://api.example/v1/embeddings",
            ),
            (
                "https://api.example/v1/embeddings",
                "https://api.example/v1/embeddings",
            ),
        ] {
            let provider =
                OpenAiEmbeddings::new(base, profile(), "key", Duration::from_secs(1)).unwrap();
            assert_eq!(provider.endpoint.as_str(), expected);
        }
        for base in [
            "http://api.example/v1",
            "https://user:pass@api.example/v1",
            "https://api.example/v1?x=1",
            "not a url",
        ] {
            assert!(OpenAiEmbeddings::new(base, profile(), "key", Duration::from_secs(1)).is_err());
        }
        assert!(
            OpenAiEmbeddings::new(
                "https://a.example",
                profile(),
                "a b",
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
