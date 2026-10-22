//! 说法学习的组合：热配置与后台成批学习。
//!
//! 训练中的会话把通过表达统计筛选的用户原话放进待学缓冲；后台每攒够一批取出一次、请求一次模型，
//! 核对摘录出自原话后保存。取出即移出缓冲，失败、超时或中断不重试；结局写到标准错误的
//! `EVE_PHRASES` 行，不含原话。
use crate::AppError;
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use eve_training_api::{PhraseLearner, TrainingService};
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

fn report(outcome: &str, batch: &eve_training_api::PhraseBatch, detail: serde_json::Value) {
    eprintln!(
        "EVE_PHRASES {}",
        json!({"outcome": outcome, "batch": batch.id, "session_id": batch.scope.session_id,
            "user_id": batch.scope.user_id, "at_ms": now_ms(), "detail": detail})
    );
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
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(training, learner, policy, activated, stopped).await;
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
        if policy.learning() {
            match training.take_phrase_batch(now_ms()) {
                Ok(Some(batch)) => {
                    let request = learner.learn(batch.clone());
                    tokio::pin!(request);
                    let result = tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => {
                            report("cancelled", &batch, json!({}));
                            let _ = training.finish_phrase_batch(&batch, vec![], now_ms());
                            return;
                        }
                        result = tokio::time::timeout(LEARN_TIMEOUT, &mut request) => result,
                    };
                    match result {
                        Ok(Ok(drafts)) => {
                            let proposed = drafts.len();
                            match training.finish_phrase_batch(&batch, drafts, now_ms()) {
                                Ok(changed) => report(
                                    "learned",
                                    &batch,
                                    json!({"proposed": proposed, "saved": changed}),
                                ),
                                Err(error) => report(
                                    "rejected",
                                    &batch,
                                    json!({"proposed": proposed, "error": error.to_string()}),
                                ),
                            }
                        }
                        failed => {
                            let error = match failed {
                                Ok(Err(error)) => error.to_string(),
                                _ => "timeout".into(),
                            };
                            let _ = training.finish_phrase_batch(&batch, vec![], now_ms());
                            report("failed", &batch, json!({"error": error}));
                            pause = PROVIDER_COOLDOWN;
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!(
                        "EVE_PHRASES {}",
                        json!({"outcome": "unavailable", "at_ms": now_ms(), "detail": {"error": error.to_string()}})
                    );
                    pause = PROVIDER_COOLDOWN;
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
