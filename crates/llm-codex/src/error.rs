use codex_api::ApiError;
use codex_client::TransportError;
use eve_llm_api::{LlmError, ProviderFailure, ProviderFailureKind as Kind};

pub(crate) fn failed(kind: Kind) -> LlmError {
    LlmError::ProviderFailed(ProviderFailure::new(kind))
}

pub(crate) fn transport(error: TransportError) -> LlmError {
    match error {
        TransportError::Timeout => LlmError::ProviderTimeout,
        TransportError::Http { status, .. } => http(status.as_u16()),
        TransportError::Connection(_) | TransportError::Network(_) => failed(Kind::Network),
        TransportError::ResponseTooLarge { .. } => failed(Kind::ResponseTooLarge),
        TransportError::Build(_) | TransportError::Policy(_) => failed(Kind::Rejected),
        TransportError::RetryLimit => failed(Kind::Other),
    }
}

pub(crate) fn api(error: ApiError) -> LlmError {
    match error {
        ApiError::Transport(error) => transport(error),
        ApiError::Api { status, .. } => http(status.as_u16()),
        ApiError::ContentFilter
        | ApiError::CyberPolicy { .. }
        | ApiError::BioPolicy { .. }
        | ApiError::MisalignmentPolicyViolation { .. } => failed(Kind::ContentFiltered),
        ApiError::ContextWindowExceeded
        | ApiError::InvalidRequest { .. }
        | ApiError::InvalidPrompt { .. } => failed(Kind::Rejected),
        ApiError::QuotaExceeded => failed(Kind::PaymentRequired),
        ApiError::RateLimitExceeded { .. } | ApiError::RateLimit(_) => failed(Kind::RateLimited),
        ApiError::FlexUnavailable | ApiError::ServerOverloaded { .. } => failed(Kind::ServerError),
        ApiError::UsageNotIncluded | ApiError::Retryable { .. } => failed(Kind::Other),
        ApiError::Stream(message) => match message.as_str() {
            // 仅匹配原源码的固定错误，绝不输出由响应正文构造的错误内容。
            "idle timeout waiting for SSE" => LlmError::ProviderTimeout,
            "Incomplete response returned, reason: max_output_tokens" => failed(Kind::Truncated),
            "stream closed before response.completed" => failed(Kind::Incomplete),
            _ => LlmError::Protocol("Codex 原响应流未能完成解码".into()),
        },
    }
}

fn http(status: u16) -> LlmError {
    ProviderFailure::http(status)
        .map(LlmError::ProviderFailed)
        .unwrap_or_else(|| failed(Kind::Other))
}
