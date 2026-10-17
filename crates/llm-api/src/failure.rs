//! Provider 失败的有限分类与模型健康观察契约。
//!
//! 只保存固定类别与 HTTP 状态码，不保存供应商错误正文、URL、请求文本或凭据。
use crate::LlmError;
use serde::{Deserialize, Serialize};

/// Provider 能确定的失败类别；不能确定时用 `Other`，不猜测原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    /// HTTP 402：额度不足，或请求按当前上下文与单次输出上限不可负担。
    PaymentRequired,
    /// HTTP 429：限流。
    RateLimited,
    /// HTTP 401/403：凭据或权限被拒绝。
    Unauthorized,
    /// 其他 HTTP 4xx：请求被拒绝，例如上下文过长或参数不被接受。
    Rejected,
    /// HTTP 5xx：服务端失败。
    ServerError,
    /// 其他非成功 HTTP 状态，例如重定向（不跟随）。
    UnexpectedStatus,
    /// 输出达到单次输出上限，回复不完整。
    Truncated,
    /// 服务端因内容策略中止输出。
    ContentFiltered,
    /// 服务端返回了失败或未完成的响应，但没有说明原因。
    Incomplete,
    /// 网络或传输失败，没有收到 HTTP 响应。
    Network,
    /// 响应超过本地字节上限，未读完。
    ResponseTooLarge,
    /// 其他 Provider 失败。
    Other,
}

impl ProviderFailureKind {
    /// 按 HTTP 状态码分类；成功状态不是失败，返回 None。
    pub fn from_http_status(status: u16) -> Option<Self> {
        Some(match status {
            200..=299 => return None,
            402 => Self::PaymentRequired,
            429 => Self::RateLimited,
            401 | 403 => Self::Unauthorized,
            400..=499 => Self::Rejected,
            500..=599 => Self::ServerError,
            100..=599 => Self::UnexpectedStatus,
            _ => return None,
        })
    }
}

/// Provider 失败的受限诊断。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFailure {
    pub kind: ProviderFailureKind,
    /// 收到了 HTTP 响应时的状态码。
    pub http_status: Option<u16>,
}

impl ProviderFailure {
    pub fn new(kind: ProviderFailureKind) -> Self {
        Self {
            kind,
            http_status: None,
        }
    }

    /// 非成功 HTTP 状态；成功状态或越界值返回 None。
    pub fn http(status: u16) -> Option<Self> {
        Some(Self {
            kind: ProviderFailureKind::from_http_status(status)?,
            http_status: Some(status),
        })
    }
}

/// 一次模型请求在健康记录中的结局。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ModelOutcome {
    Succeeded,
    /// Provider 给出了分类的失败。
    Failed {
        failure: ProviderFailure,
    },
    /// 本地期限内没有完成。
    TimedOut,
    /// 响应不符合协议或请求了不支持的能力。
    InvalidResponse,
    /// 模型配置无法解析，没有发出请求。
    Misconfigured,
}

impl ModelOutcome {
    /// 从一次请求的错误得到结局；取消以及宿主自身的失败不是模型的结局，返回 None。
    pub fn from_error(error: &LlmError) -> Option<Self> {
        Some(match error {
            LlmError::Cancelled => return None,
            LlmError::ProviderFailed(failure) => Self::Failed { failure: *failure },
            LlmError::ProviderTimeout => Self::TimedOut,
            LlmError::Protocol(_) | LlmError::Unsupported(_) => Self::InvalidResponse,
            LlmError::Configuration(_) => Self::Misconfigured,
            LlmError::Provider(_) => Self::Failed {
                failure: ProviderFailure::new(ProviderFailureKind::Other),
            },
            // 上下文、工具轮次与宿主后端的失败不是模型请求的结局。
            LlmError::Context(_)
            | LlmError::HistoryWindowExceeded
            | LlmError::RoundLimit
            | LlmError::Backend(_) => return None,
        })
    }
}

/// 可信宿主记录模型请求结局的入口；记录失败不影响请求本身。
pub trait ModelHealthObserver: Send + Sync {
    /// `role` 是宿主登记的用途名，例如 dialogue、research。
    fn record(&self, role: &str, outcome: ModelOutcome, at_ms: u64);
}
