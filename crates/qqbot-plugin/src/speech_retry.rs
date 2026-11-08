//! 交流决定失败后的本地调度；不决定是否说话，也不保存或认领发送。
use crate::QqSpeechEvent;
use eve_llm_api::LlmError;
use eve_plugin_api::{LogEntry, LogLevel};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

const RETRY_START: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
const KEEP_FOR: Duration = Duration::from_secs(60 * 60);
const MAX_FAILURES: usize = 512;

/// 日志和轮转都只使用固定长度指纹；不记录会话编号、正文或供应商错误文本。
fn fingerprint(event: &QqSpeechEvent) -> String {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for value in [
        &event.session.session_id,
        &event.session.user_id,
        &event.id,
        &event.anchor,
    ] {
        digest.update(&(value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    // 通常来源版本已在 event.id 中；额外覆盖替换实现的事实更新，日志仍只出指纹。
    let facts = event.facts.to_string();
    digest.update(&(facts.len() as u64).to_be_bytes());
    digest.update(facts.as_bytes());
    digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Failed {
    count: u32,
    until: Instant,
    updated: Instant,
}

#[derive(Default)]
pub(super) struct SpeechRetry {
    failed: BTreeMap<String, Failed>,
    cursor: Option<String>,
}

impl SpeechRetry {
    /// 替换实现即使返回相同的队首，也不能让一个事件独占交流决定入口。
    pub(super) fn order(&self, events: Vec<QqSpeechEvent>) -> Vec<QqSpeechEvent> {
        let mut events: Vec<_> = events
            .into_iter()
            .map(|event| (fingerprint(&event), event))
            .collect();
        events.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(cursor) = &self.cursor {
            let pivot = events.partition_point(|(key, _)| key <= cursor);
            events.rotate_left(pivot);
        }
        events.into_iter().map(|(_, event)| event).collect()
    }

    pub(super) fn eligible(&self, event: &QqSpeechEvent, now: Instant) -> bool {
        self.failed
            .get(&fingerprint(event))
            .is_none_or(|failure| now >= failure.until)
    }

    pub(super) fn started(&mut self, event: &QqSpeechEvent) {
        self.cursor = Some(fingerprint(event));
    }

    pub(super) fn succeeded(&mut self, event: &QqSpeechEvent) {
        self.failed.remove(&fingerprint(event));
    }

    pub(super) fn failed(&mut self, event: &QqSpeechEvent, now: Instant) -> Duration {
        self.failed
            .retain(|_, failure| now.saturating_duration_since(failure.updated) < KEEP_FOR);
        let key = fingerprint(event);
        if self.failed.len() >= MAX_FAILURES
            && !self.failed.contains_key(&key)
            && let Some(oldest) = self
                .failed
                .iter()
                .min_by_key(|(_, failure)| failure.updated)
                .map(|(key, _)| key.clone())
        {
            self.failed.remove(&oldest);
        }
        let failure = self.failed.entry(key).or_insert(Failed {
            count: 0,
            until: now,
            updated: now,
        });
        failure.count = failure.count.saturating_add(1);
        let delay = (RETRY_START * (1 << failure.count.saturating_sub(1).min(4))).min(RETRY_MAX);
        failure.until = now + delay;
        failure.updated = now;
        delay
    }
}

pub(super) fn failure_log(
    event: &QqSpeechEvent,
    error: &LlmError,
    elapsed: Duration,
    retry: Duration,
    postponed: bool,
) -> LogEntry {
    let kind = match error {
        LlmError::HistoryWindowExceeded => "history_window_exceeded",
        LlmError::Configuration(_) => "configuration",
        LlmError::Context(_) => "context",
        LlmError::ContextCompactionFailed(_) => "context_compaction",
        LlmError::Provider(_) | LlmError::ProviderFailed(_) => "provider",
        LlmError::ProviderTimeout => "provider_timeout",
        LlmError::Protocol(_) => "protocol",
        LlmError::Unsupported(_) => "unsupported",
        LlmError::RoundLimit => "round_limit",
        LlmError::Backend(_) => "backend",
        LlmError::Cancelled => "cancelled",
    };
    let source = if event.id.starts_with("outreach:passive:") {
        "passive_outreach"
    } else if event.id.starts_with("outreach:") {
        "outreach"
    } else if event.id.starts_with("foreground-progress:") {
        "foreground_progress"
    } else if event.id.starts_with("foreground-input:") {
        "foreground_input"
    } else {
        "host"
    };
    let mut entry = LogEntry::new(LogLevel::Warn, "eve.qqbot", "speech_decision_unavailable")
        .expect("固定日志目标")
        .with_field("failure_kind", kind)
        .with_field("source_kind", source)
        .with_field("event_fingerprint", fingerprint(event))
        .with_field("elapsed_ms", elapsed.as_millis().to_string())
        .with_field("retry_after_ms", retry.as_millis().to_string())
        .with_field("postponed", postponed.to_string());
    if let Some(failure) = error.provider_failure() {
        // 枚举序列化只有本地定义的固定值，不能带入供应商正文。
        if let Ok(serde_json::Value::String(kind)) = serde_json::to_value(failure.kind) {
            entry = entry.with_field("provider_failure_kind", kind);
        }
    }
    if let Some(status) = error.provider_http_status() {
        entry = entry.with_field("http_status", status.to_string());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_session_api::SessionKey;
    use serde_json::json;

    fn event(id: &str) -> QqSpeechEvent {
        QqSpeechEvent {
            id: id.into(),
            session: SessionKey::new("private-session", "private-user").unwrap(),
            anchor: "private-input".into(),
            facts: json!({"text":"private-body"}),
        }
    }

    #[test]
    fn repeated_failure_yields_to_other_events_and_new_input() {
        let mut retries = SpeechRetry::default();
        let now = Instant::now();
        let bad = event("bad");
        let good = event("good");
        retries.started(&bad);
        assert_eq!(retries.failed(&bad, now), Duration::from_secs(60));
        assert!(!retries.eligible(&bad, now + Duration::from_secs(59)));
        assert!(retries.eligible(&good, now));
        assert_eq!(retries.order(vec![bad.clone(), good.clone()])[0], good);
        assert!(retries.eligible(&bad, now + Duration::from_secs(60)));
        let mut newer = bad.clone();
        newer.anchor = "new-input".into();
        assert!(retries.eligible(&newer, now));
        let mut updated = bad.clone();
        updated.facts = json!({"text":"a new fact version"});
        assert!(retries.eligible(&updated, now));
        let mut foreign = bad.clone();
        foreign.session.user_id = "another-user".into();
        assert!(retries.eligible(&foreign, now));
        let next = now + Duration::from_secs(60);
        assert_eq!(retries.failed(&bad, next), Duration::from_secs(120));
        retries.succeeded(&bad);
        assert!(retries.eligible(&bad, next));
        assert_eq!(retries.failed(&bad, next), Duration::from_secs(60));
    }

    #[test]
    fn retries_are_bounded_and_expire_without_permanently_silencing_events() {
        let mut retries = SpeechRetry::default();
        let mut now = Instant::now();
        let failed = event("persistent");
        for seconds in [60, 120, 240, 480, 900, 900] {
            let delay = retries.failed(&failed, now);
            assert_eq!(delay, Duration::from_secs(seconds));
            now += delay;
        }
        assert!(retries.eligible(&failed, now));
        now += KEEP_FOR;
        assert_eq!(retries.failed(&failed, now), RETRY_START);
        for index in 0..MAX_FAILURES + 20 {
            retries.failed(&event(&format!("event-{index}")), now);
        }
        assert_eq!(retries.failed.len(), MAX_FAILURES);
    }

    #[test]
    fn diagnostics_keep_failure_class_and_status_without_raw_error_or_identity() {
        let source = event("foreground-input:private-id");
        for error in [
            LlmError::Context("private-token private-body".into()),
            LlmError::Provider("HTTP 400 private-token private-body".into()),
            LlmError::ProviderFailed(eve_llm_api::ProviderFailure::http(400).unwrap()),
        ] {
            let entry = failure_log(
                &source,
                &error,
                Duration::from_millis(12),
                RETRY_START,
                false,
            );
            let printed = format!("{entry:?}");
            assert!(!printed.contains("private-"));
            assert_eq!(entry.fields["event_fingerprint"].len(), 64);
            assert_eq!(entry.fields["source_kind"], "foreground_input");
            assert_eq!(entry.fields["elapsed_ms"], "12");
            assert_eq!(entry.fields["retry_after_ms"], "60000");
            assert_eq!(entry.fields["postponed"], "false");
            if matches!(error, LlmError::ProviderFailed(_)) {
                assert_eq!(entry.fields["http_status"], "400");
                assert_eq!(entry.fields["provider_failure_kind"], "rejected");
            } else {
                assert!(!entry.fields.contains_key("http_status"));
            }
        }
    }
}
