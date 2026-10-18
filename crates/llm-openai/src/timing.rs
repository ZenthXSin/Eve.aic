//! 可选传输计时；只包含本进程序号、阶段、时间与 HTTP 状态，不含请求或响应正文。
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
static NEXT: AtomicU64 = AtomicU64::new(1);
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
}
impl Drop for RequestTiming {
    fn drop(&mut self) {
        if !self.finished {
            self.record("closed_without_decoded_response", None);
        }
    }
}
