//! 说法学习的组合：热配置与后台成批学习。
//!
//! 训练中的会话把通过表达统计筛选的用户原话放进待学缓冲；后台持久领取固定批次，
//! 明确暂态失败在原预算内有限补做。取消、不合格和未知保存不重放；结局写到标准错误的
//! `EVE_PHRASES` 行，不含原话。
use crate::AppError;
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use eve_llm_api::{LlmError, ProviderFailureKind};
use eve_training_api::{
    MAX_PHRASE_ATTEMPTS, PhraseBatch, PhraseFailure, PhraseFailureOutcome, PhraseFinish,
    PhraseLearner, TrainingService,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) const NAMESPACE: &str = "runtime.phrases";
pub(crate) const MODEL_USE: &str = "phrase";
const LEARN_TIMEOUT: Duration = Duration::from_secs(60);
/// 模型请求失败后暂停新请求的时间。
const PROVIDER_COOLDOWN: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(500);

#[cfg(test)]
#[path = "qq_phrases_tests.rs"]
mod tests;

pub(crate) fn schema() -> ConfigSchema {
    let mut learning = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    learning.environment = Some("EVE_PHRASE_LEARNING".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("learning".into(), learning)]),
    }
}

/// 配置服务在核心安装后才可用；之前一律视为关闭。
#[derive(Default)]
pub(crate) struct Policy {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}
impl Policy {
    fn learning(&self) -> bool {
        self.settings
            .get()
            .and_then(|s| s.snapshot(NAMESPACE, 1).ok())
            .and_then(|s| s.get::<bool>("learning").ok())
            .unwrap_or(false)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| u64::try_from(v.as_millis()).ok())
        .unwrap_or(1)
}

fn report(outcome: &str, batch: &PhraseBatch, detail: serde_json::Value) {
    eprintln!(
        "EVE_PHRASES {}",
        json!({"outcome": outcome, "batch": batch.id, "attempt": batch.attempt,
            "max_attempts": MAX_PHRASE_ATTEMPTS, "session_id": batch.scope.session_id,
            "user_id": batch.scope.user_id, "at_ms": now_ms(), "detail": detail})
    );
}

fn classify_failure(error: &LlmError) -> PhraseFailure {
    match error {
        LlmError::ProviderTimeout => PhraseFailure::Timeout,
        LlmError::ProviderFailed(failure) => match (failure.kind, failure.http_status) {
            (ProviderFailureKind::Network, None) => PhraseFailure::Network,
            (ProviderFailureKind::RateLimited, None | Some(429)) => PhraseFailure::RateLimited,
            (ProviderFailureKind::ServerError, None | Some(500..=599)) => {
                PhraseFailure::ServerError
            }
            _ => PhraseFailure::Rejected,
        },
        LlmError::Cancelled => PhraseFailure::Cancelled,
        _ => PhraseFailure::Rejected,
    }
}

/// true 表示结局已确认；false 让后台结束，不拿未知保存结果继续写。
fn finish_result(
    training: &dyn TrainingService,
    batch: &PhraseBatch,
    result: Result<Vec<eve_training_api::PhraseDraft>, LlmError>,
    now: u64,
) -> bool {
    finish_result_with_report(training, batch, result, now, report)
}

fn finish_result_with_report(
    training: &dyn TrainingService,
    batch: &PhraseBatch,
    result: Result<Vec<eve_training_api::PhraseDraft>, LlmError>,
    now: u64,
    mut report: impl FnMut(&str, &PhraseBatch, serde_json::Value),
) -> bool {
    match result {
        Ok(drafts) => {
            let proposed = drafts.len();
            match training.finish_phrase_batch(batch, drafts, now) {
                Ok(PhraseFinish::Saved(saved)) => report(
                    if proposed == 0 {
                        "empty"
                    } else if saved == 0 {
                        "unchanged"
                    } else {
                        "learned"
                    },
                    batch,
                    json!({"proposed": proposed, "saved": saved, "skipped": 0}),
                ),
                Ok(PhraseFinish::CapacityLimited { saved, skipped }) => report(
                    if saved == 0 {
                        "capacity_limited"
                    } else {
                        "learned_partial"
                    },
                    batch,
                    json!({"proposed": proposed, "saved": saved, "skipped": skipped}),
                ),
                Ok(PhraseFinish::Rejected) => {
                    report("rejected", batch, json!({"proposed": proposed}))
                }
                Ok(PhraseFinish::Stale) => report("stale", batch, json!({})),
                Err(_) => {
                    report(
                        "storage_unavailable",
                        batch,
                        json!({"committed": "unknown"}),
                    );
                    return false;
                }
            }
        }
        Err(error) => {
            let failure = classify_failure(&error);
            match training.fail_phrase_batch(batch, failure, now) {
                Ok(PhraseFailureOutcome::Retry { ready_at_ms }) => report(
                    "retry_scheduled",
                    batch,
                    json!({"failure": failure, "retry_at_ms": ready_at_ms}),
                ),
                Ok(PhraseFailureOutcome::Exhausted) => {
                    report("exhausted", batch, json!({"failure": failure}))
                }
                Ok(PhraseFailureOutcome::Finished) => report(
                    if failure == PhraseFailure::Cancelled {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    batch,
                    json!({"failure": failure}),
                ),
                Ok(PhraseFailureOutcome::Stale) => {
                    report("stale", batch, json!({"failure": failure}))
                }
                Err(_) => {
                    report(
                        "storage_unavailable",
                        batch,
                        json!({"failure": failure, "committed": "unknown"}),
                    );
                    return false;
                }
            }
        }
    }
    true
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<()>,
}
impl Background {
    pub(crate) fn start(
        training: Arc<dyn TrainingService>,
        learner: Arc<dyn PhraseLearner>,
        policy: Arc<Policy>,
        budget: crate::qq_budget::Gate,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(training, learner, policy, budget, activated, stopped).await;
            let _ = finished_sender.send(true);
        });
        Self {
            active,
            stop,
            finished,
            task,
        }
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "说法学习后台异常；已学说法不受影响".into())
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn run(
    training: Arc<dyn TrainingService>,
    learner: Arc<dyn PhraseLearner>,
    policy: Arc<Policy>,
    budget: crate::qq_budget::Gate,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) {
    loop {
        if *active.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return,
            changed = active.changed() => if changed.is_err() { return; },
        }
    }
    loop {
        let mut pause = POLL;
        if policy.learning()
            && match budget.admits_category(MODEL_USE, 1) {
                Ok(admitted) => admitted,
                Err(error) => {
                    eprintln!(
                        "EVE_BACKGROUND_BUDGET {}",
                        json!({"category": MODEL_USE, "outcome": "unavailable", "error": error.to_string()})
                    );
                    return;
                }
            }
        {
            match training.take_phrase_batch(now_ms()) {
                Ok(Some(batch)) => {
                    let request = learner.learn(batch.clone());
                    tokio::pin!(request);
                    let result = tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => {
                            finish_result(training.as_ref(), &batch, Err(LlmError::Cancelled), now_ms());
                            return;
                        }
                        result = tokio::time::timeout(LEARN_TIMEOUT, &mut request) => result,
                    };
                    let result = result.unwrap_or(Err(LlmError::ProviderTimeout));
                    if result.is_err() {
                        pause = PROVIDER_COOLDOWN;
                    }
                    if !finish_result(training.as_ref(), &batch, result, now_ms()) {
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!(
                        "EVE_PHRASES {}",
                        json!({"outcome": "unavailable", "at_ms": now_ms(), "detail": {"error": error.to_string()}})
                    );
                    return;
                }
            }
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return,
            _ = tokio::time::sleep(pause) => {},
        }
    }
}
