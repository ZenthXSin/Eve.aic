use super::*;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ContextScope, ProviderFailure};
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, StateStore};
use eve_training_api::{
    MAX_PHRASES, PHRASE_BATCH, PHRASE_LEARNER_VERSION, PhraseDraft, PhraseKind, TRAINING_PLUGIN_ID,
    TRAINING_SERVICE_ID, TrainingServiceHandle,
};
use std::{path::Path, sync::Mutex};

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

fn expression(text: &str) -> PhraseDraft {
    PhraseDraft {
        kind: PhraseKind::Expression,
        text: text.into(),
        meaning: "实际更新的解释".into(),
        example: "原话".into(),
    }
}

fn old_phrases(count: usize, version: u8) -> Vec<u8> {
    let owner = batch().scope;
    let mut learned: Vec<_> = (1..=count)
        .map(|index| {
            json!({"scope":owner,"phrase":{
                "id":format!("p{index}"),"kind":"expression",
                "text":format!("旧说法{index}"),"meaning":"已有解释",
                "example":"原话","uses":1,"updated_at_ms":1
            }})
        })
        .collect();
    learned.push(json!({"scope":ContextScope {
        session_id: owner.session_id.clone(), user_id:"other-user".into()
    },"phrase":{
        "id":format!("p{}",count+1),"kind":"expression","text":"旧说法1",
        "meaning":"另一人的解释","example":"原话","uses":1,"updated_at_ms":1
    }}));
    let mut document = json!({"version":version,"next":count+2,"pending":[],"learned":learned});
    if version == 2 {
        document["inflight"] = json!([]);
    }
    serde_json::to_vec(&document).unwrap()
}

async fn file_training(
    directory: &Path,
    initial: Option<Vec<u8>>,
) -> (Kernel, Arc<dyn TrainingService>) {
    let state = Arc::new(FileStateStore::open(directory).unwrap());
    if let Some(bytes) = initial {
        state
            .set(
                &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
                "phrases.v1".into(),
                bytes,
            )
            .unwrap();
    }
    let services = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(
            eve_training_plugin::TrainingPlugin::new(true)
                .unwrap()
                .with_phrase_learning(),
        ))
        .unwrap();
    kernel.start_all().await.unwrap();
    let training = registry
        .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<TrainingServiceHandle>()
        .unwrap()
        .0
        .clone();
    (kernel, training)
}

fn fresh_batch(training: &dyn TrainingService, prefix: &str) -> PhraseBatch {
    for index in 0..PHRASE_BATCH {
        training
            .observe_user_message(
                &batch().scope,
                &format!("{prefix}-{index}"),
                &format!("这一句原话，{prefix} 第 {index} 次"),
            )
            .unwrap();
    }
    training.take_phrase_batch(2).unwrap().unwrap()
}

fn captured_finish(
    training: &dyn TrainingService,
    batch: &PhraseBatch,
    drafts: Vec<PhraseDraft>,
) -> (String, serde_json::Value) {
    let mut events = Vec::new();
    assert!(finish_result_with_report(
        training,
        batch,
        Ok(drafts),
        3,
        |outcome, actual, detail| {
            assert_eq!(actual, batch);
            events.push((outcome.to_owned(), detail));
        }
    ));
    assert_eq!(events.len(), 1);
    events.pop().unwrap()
}

#[tokio::test]
async fn a_65th_phrase_is_saved_after_loading_old_state_and_survives_file_reopen() {
    for version in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let (kernel, training) =
            file_training(directory.path(), Some(old_phrases(64, version))).await;
        let owner = batch().scope;
        let foreign = ContextScope {
            session_id: owner.session_id.clone(),
            user_id: "other-user".into(),
        };
        let before = training.phrases(&owner).unwrap();
        let other_before = training.phrases(&foreign).unwrap();
        assert_eq!(before.len(), 64);
        let claimed = fresh_batch(training.as_ref(), "65th");
        assert_eq!(
            captured_finish(training.as_ref(), &claimed, vec![expression("新说法65")]),
            (
                "learned".into(),
                json!({"proposed":1,"saved":1,"skipped":0})
            )
        );
        let saved = training.phrases(&owner).unwrap();
        assert_eq!(saved.len(), 65);
        for old in &before {
            assert!(saved.contains(old), "旧说法不能被扩容覆盖或淘汰");
        }
        kernel.stop_all().await.unwrap();
        drop(training);
        drop(kernel);

        let disk = std::fs::read(directory.path().join("state.json")).unwrap();
        let (kernel, training) = file_training(directory.path(), None).await;
        assert_eq!(training.phrases(&owner).unwrap(), saved);
        assert_eq!(training.phrases(&foreign).unwrap(), other_before);
        assert_eq!(
            training
                .finish_phrase_batch(&claimed, vec![expression("新说法65")], 4)
                .unwrap(),
            PhraseFinish::Stale
        );
        assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
        assert_eq!(
            std::fs::read(directory.path().join("state.json")).unwrap(),
            disk,
            "恢复与重复提交不改写旧批次"
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn final_scope_capacity_reports_partial_and_skipped_counts_without_replay() {
    assert_eq!(MAX_PHRASES, 256);
    let directory = tempfile::tempdir().unwrap();
    let (kernel, training) =
        file_training(directory.path(), Some(old_phrases(MAX_PHRASES - 1, 2))).await;
    let owner = batch().scope;
    let first = fresh_batch(training.as_ref(), "partial");
    assert_eq!(
        captured_finish(
            training.as_ref(),
            &first,
            vec![
                expression("新增256"),
                expression("跳过257"),
                expression("旧说法1")
            ],
        ),
        (
            "learned_partial".into(),
            json!({"proposed":3,"saved":2,"skipped":1})
        )
    );
    let saved = training.phrases(&owner).unwrap();
    assert_eq!(saved.len(), MAX_PHRASES);
    assert_eq!(saved.iter().find(|p| p.text == "旧说法1").unwrap().uses, 2);
    assert!(saved.iter().any(|p| p.text == "新增256"));
    assert!(!saved.iter().any(|p| p.text == "跳过257"));

    let full = fresh_batch(training.as_ref(), "full");
    assert_eq!(
        captured_finish(
            training.as_ref(),
            &full,
            vec![expression("跳过257"), expression("跳过258")],
        ),
        (
            "capacity_limited".into(),
            json!({"proposed":2,"saved":0,"skipped":2})
        )
    );
    let empty = fresh_batch(training.as_ref(), "empty");
    assert_eq!(
        captured_finish(training.as_ref(), &empty, vec![]),
        ("empty".into(), json!({"proposed":0,"saved":0,"skipped":0}))
    );
    let invalid = fresh_batch(training.as_ref(), "invalid");
    let mut ungrounded = expression("不合格");
    ungrounded.example = "这不是批次原话".into();
    assert_eq!(
        captured_finish(training.as_ref(), &invalid, vec![ungrounded]),
        ("rejected".into(), json!({"proposed":1}))
    );
    assert_eq!(training.phrases(&owner).unwrap(), saved);
    kernel.stop_all().await.unwrap();
    drop(training);
    drop(kernel);

    let (kernel, training) = file_training(directory.path(), None).await;
    assert_eq!(training.phrases(&owner).unwrap(), saved);
    for old in [first, full, empty, invalid] {
        assert_eq!(
            training
                .finish_phrase_batch(&old, vec![expression("重复结果")], 5)
                .unwrap(),
            PhraseFinish::Stale
        );
    }
    assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}
