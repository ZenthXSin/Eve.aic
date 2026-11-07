use super::*;
use eve_llm_api::{ContextScope, ProviderFailure};
use eve_plugin_api::{PluginError, PluginResult};
use eve_training_api::{PHRASE_BATCH, PHRASE_LEARNER_VERSION, PhraseDraft};
use std::sync::Mutex;

#[derive(Default)]
struct Recorder {
    failed: Mutex<Vec<PhraseFailure>>,
    finished: Mutex<usize>,
    write_error: bool,
    finish_outcome: Option<PhraseFinish>,
}
impl TrainingService for Recorder {
    fn enabled(&self, _: &ContextScope) -> PluginResult<bool> {
        Ok(true)
    }
    fn set_enabled(&self, _: &ContextScope, _: bool) -> PluginResult<()> {
        Ok(())
    }
    fn finish_phrase_batch(
        &self,
        _: &PhraseBatch,
        _: Vec<PhraseDraft>,
        _: u64,
    ) -> PluginResult<PhraseFinish> {
        *self.finished.lock().unwrap() += 1;
        if self.write_error {
            Err(PluginError::State("提交结果未知".into()))
        } else {
            Ok(self.finish_outcome.unwrap_or(PhraseFinish::Saved(0)))
        }
    }
    fn fail_phrase_batch(
        &self,
        _: &PhraseBatch,
        failure: PhraseFailure,
        _: u64,
    ) -> PluginResult<PhraseFailureOutcome> {
        self.failed.lock().unwrap().push(failure);
        if self.write_error {
            Err(PluginError::State("提交结果未知".into()))
        } else {
            Ok(PhraseFailureOutcome::Finished)
        }
    }
}
fn batch() -> PhraseBatch {
    PhraseBatch {
        id: "b1".into(),
        scope: ContextScope {
            session_id: "s".into(),
            user_id: "u".into(),
        },
        messages: vec!["原话".into(); PHRASE_BATCH],
        attempt: 1,
        learner_version: PHRASE_LEARNER_VERSION,
    }
}

#[test]
fn only_explicit_transient_categories_schedule_retries() {
    for (error, expected) in [
        (LlmError::ProviderTimeout, PhraseFailure::Timeout),
        (
            LlmError::ProviderFailed(ProviderFailure::new(ProviderFailureKind::Network)),
            PhraseFailure::Network,
        ),
        (
            LlmError::ProviderFailed(ProviderFailure::http(429).unwrap()),
            PhraseFailure::RateLimited,
        ),
        (
            LlmError::ProviderFailed(ProviderFailure::http(503).unwrap()),
            PhraseFailure::ServerError,
        ),
        (LlmError::Cancelled, PhraseFailure::Cancelled),
        (
            LlmError::Provider("Service temporarily unavailable".into()),
            PhraseFailure::Rejected,
        ),
        (
            LlmError::Provider("说法学习输出不是约定的 JSON".into()),
            PhraseFailure::Rejected,
        ),
        (
            LlmError::Protocol("无效结构".into()),
            PhraseFailure::Rejected,
        ),
        (
            LlmError::Configuration("尚未配置".into()),
            PhraseFailure::Rejected,
        ),
        (
            LlmError::ProviderFailed(ProviderFailure {
                kind: ProviderFailureKind::ServerError,
                http_status: Some(400),
            }),
            PhraseFailure::Rejected,
        ),
    ] {
        assert_eq!(classify_failure(&error), expected);
    }
    for status in [400, 401, 402, 403, 404] {
        assert_eq!(
            classify_failure(&LlmError::ProviderFailed(
                ProviderFailure::http(status).unwrap()
            )),
            PhraseFailure::Rejected
        );
    }
    for kind in [
        ProviderFailureKind::Incomplete,
        ProviderFailureKind::Truncated,
        ProviderFailureKind::ResponseTooLarge,
        ProviderFailureKind::ContentFiltered,
        ProviderFailureKind::Other,
    ] {
        assert_eq!(
            classify_failure(&LlmError::ProviderFailed(ProviderFailure::new(kind))),
            PhraseFailure::Rejected
        );
    }
}

#[test]
fn saving_error_never_becomes_a_model_retry_and_failure_write_error_stops() {
    let recorder = Recorder {
        write_error: true,
        ..Default::default()
    };
    assert!(!finish_result(&recorder, &batch(), Ok(vec![]), 1));
    assert_eq!(*recorder.finished.lock().unwrap(), 1);
    assert!(recorder.failed.lock().unwrap().is_empty());
    assert!(!finish_result(
        &recorder,
        &batch(),
        Err(LlmError::ProviderTimeout),
        2
    ));
    assert_eq!(*recorder.failed.lock().unwrap(), [PhraseFailure::Timeout]);
}

#[test]
fn cancellation_uses_explicit_terminal_failure_instead_of_empty_success() {
    let recorder = Recorder::default();
    assert!(finish_result(
        &recorder,
        &batch(),
        Err(LlmError::Cancelled),
        1
    ));
    assert_eq!(*recorder.finished.lock().unwrap(), 0);
    assert_eq!(*recorder.failed.lock().unwrap(), [PhraseFailure::Cancelled]);
}

#[test]
fn confirmed_rejection_keeps_the_worker_running_without_a_retry() {
    let recorder = Recorder {
        finish_outcome: Some(PhraseFinish::Rejected),
        ..Default::default()
    };
    assert!(finish_result(&recorder, &batch(), Ok(vec![]), 1));
    assert_eq!(*recorder.finished.lock().unwrap(), 1);
    assert!(recorder.failed.lock().unwrap().is_empty());
}
