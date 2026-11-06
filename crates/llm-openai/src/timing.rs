//! 可选传输计时与错误分类；不包含端点、凭据、请求或响应正文。
use std::{
    error::Error,
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
static NEXT_CALL: AtomicU64 = AtomicU64::new(1);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

// 只保留宿主公开用途中的固定字符串；未登记用途不输出调用方原始文字。
const ROLES: &[&str] = &[
    "dialogue",
    "speech",
    "plan",
    "interest",
    "research",
    "practice",
    "skill",
    "forge",
    "outreach",
    "clarify",
    "preference",
    "reflection",
    "summary",
    "task",
    "digest",
    "phrase",
    "conversation_learning",
    "impression",
    "chime",
    "group_relation_learning",
    "group_digest",
    "sticker",
    "review",
    "progress",
    "message_relation",
    "exploration",
];

/// 一次完整模型调用，含它在原总期限内的全部 HTTP 尝试。
/// 编号即使关闭计时也会分配，让原有的重试与超时日志仍能关联。
pub(crate) struct CallDiagnostics {
    identity: RequestIdentity,
}
impl CallDiagnostics {
    pub(crate) fn start(role: Option<&str>) -> Self {
        Self {
            identity: RequestIdentity {
                call: NEXT_CALL.fetch_add(1, Ordering::Relaxed),
                request: None,
                attempt: None,
                role: ROLES.iter().copied().find(|name| Some(*name) == role),
            },
        }
    }

    pub(crate) fn next_attempt(&mut self) -> RequestIdentity {
        self.identity.request = Some(NEXT_REQUEST.fetch_add(1, Ordering::Relaxed));
        self.identity.attempt = Some(self.identity.attempt.unwrap_or(0) + 1);
        self.identity
    }

    // 总期限取消内层 future 后，仍可读取最后一次尝试，不靠日志时间猜测。
    pub(crate) fn identity(&self) -> RequestIdentity {
        self.identity
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RequestIdentity {
    call: u64,
    request: Option<u64>,
    attempt: Option<u32>,
    role: Option<&'static str>,
}
impl RequestIdentity {
    pub(crate) fn fields(self) -> serde_json::Value {
        serde_json::json!({
            "call":self.call,"request":self.request,"attempt":self.attempt,"role":self.role,
        })
    }
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransportPhase {
    BeforeHeaders,
    ResponseBody,
    StreamBody,
}

pub(crate) struct RequestTiming {
    identity: RequestIdentity,
    started: Instant,
    finished: bool,
}
impl RequestTiming {
    pub(crate) fn start(enabled: bool, identity: RequestIdentity) -> Option<Self> {
        enabled.then(|| {
            let timing = Self {
                identity,
                started: Instant::now(),
                finished: false,
            };
            timing.record("dispatch", None);
            timing
        })
    }
    pub(crate) fn record(&self, phase: &str, status: Option<u16>) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let mut fields = self.identity.fields();
        fields["phase"] = serde_json::json!(phase);
        fields["at_ms"] = serde_json::json!(at);
        fields["elapsed_ms"] = serde_json::json!(self.started.elapsed().as_millis());
        fields["http_status"] = serde_json::json!(status);
        eprintln!("EVE_HTTP_TIMING {fields}");
    }
    pub(crate) fn finish(&mut self) {
        self.finished = true;
        self.record("decoded", None);
    }

    pub(crate) fn transport_failed(&self, phase: TransportPhase, error: &reqwest::Error) {
        eprintln!("EVE_HTTP_TRANSPORT {}", self.transport_fields(phase, error));
    }

    fn transport_fields(&self, phase: TransportPhase, error: &reqwest::Error) -> serde_json::Value {
        let (io_kind, io_os_error) = io_details(error);
        let mut fields = self.identity.fields();
        fields["phase"] = serde_json::json!(phase);
        fields["at_ms"] = serde_json::json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        );
        fields["elapsed_ms"] = serde_json::json!(self.started.elapsed().as_millis());
        fields["is_timeout"] = serde_json::json!(error.is_timeout());
        fields["is_connect"] = serde_json::json!(error.is_connect());
        fields["io_kind"] = serde_json::json!(io_kind);
        fields["io_os_error"] = serde_json::json!(io_os_error);
        fields
    }
}

fn io_details(error: &(dyn Error + 'static)) -> (Option<String>, Option<i32>) {
    let mut source = Some(error);
    let mut details = (None, None);
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            // 只格式化标准库枚举，绝不格式化可能带 URL、凭据或正文的错误消息。
            details = (Some(format!("{:?}", io.kind())), io.raw_os_error());
        }
        source = error.source();
    }
    details
}

impl Drop for RequestTiming {
    fn drop(&mut self) {
        if !self.finished {
            self.record("closed_without_decoded_response", None);
        }
    }
}

#[cfg(test)]
mod tests;
