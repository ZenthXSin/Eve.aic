//! 可选传输计时与错误分类；不包含端点、凭据、请求或响应正文。
use std::{
    error::Error,
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransportPhase {
    BeforeHeaders,
    ResponseBody,
    StreamBody,
}

pub(crate) struct RequestTiming {
    id: u64,
    started: Instant,
    finished: bool,
}
impl RequestTiming {
    pub(crate) fn start(enabled: bool) -> Option<Self> {
        enabled.then(|| {
            let timing = Self {
                id: NEXT.fetch_add(1, Ordering::Relaxed),
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
        eprintln!(
            "EVE_HTTP_TIMING {}",
            serde_json::json!({"request":self.id,"phase":phase,"at_ms":at,"elapsed_ms":self.started.elapsed().as_millis(),"http_status":status})
        );
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
        serde_json::json!({
            "request":self.id,"phase":phase,
            "at_ms":SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis()),
            "elapsed_ms":self.started.elapsed().as_millis(),
            "is_timeout":error.is_timeout(),"is_connect":error.is_connect(),
            "io_kind":io_kind,"io_os_error":io_os_error,
        })
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
