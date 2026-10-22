//! 人物印象的后台学习：此人谈及自己的原话攒够一批后学习一次。
//!
//! 取出即移出缓冲，失败、超时或中断不重试；结局写到标准错误的 `EVE_IMPRESSIONS` 行，不含原话。
//! 热配置 `runtime.people.impressions` 只控制是否发起学习请求。
use crate::AppError;
use eve_config_api::ConfigService;
use eve_people_api::{ImpressionBatch, ImpressionLearner, PeopleDirectory};
use serde_json::json;
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) const MODEL_USE: &str = "impression";
const LEARN_TIMEOUT: Duration = Duration::from_secs(60);
/// 模型请求失败后暂停新请求的时间。
const PROVIDER_COOLDOWN: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(500);

/// 配置服务在核心安装后才可用；之前一律视为关闭。
#[derive(Default)]
pub(crate) struct Policy {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}
impl Policy {
    fn learning(&self) -> bool {
        self.settings
            .get()
            .and_then(|s| s.snapshot(crate::qq_people::NAMESPACE, 1).ok())
            .and_then(|s| s.get::<bool>("impressions").ok())
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

fn report(outcome: &str, batch: Option<&ImpressionBatch>, detail: serde_json::Value) {
    eprintln!(
        "EVE_IMPRESSIONS {}",
        json!({"outcome": outcome, "batch": batch.map(|b| &b.id),
            "participant": batch.map(|b| &b.participant), "at_ms": now_ms(), "detail": detail})
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
        people: Arc<dyn PeopleDirectory>,
        learner: Arc<dyn ImpressionLearner>,
        policy: Arc<Policy>,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(people, learner, policy, activated, stopped).await;
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
            .map_err(|_| "印象学习后台异常；已有印象不受影响".into())
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
    people: Arc<dyn PeopleDirectory>,
    learner: Arc<dyn ImpressionLearner>,
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
            match people.take_impression_batch() {
                Ok(Some(batch)) => {
                    let request = learner.learn(batch.clone());
                    tokio::pin!(request);
                    let result = tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => {
                            let _ = people.finish_impression_batch(&batch, vec![], now_ms());
                            report("cancelled", Some(&batch), json!({}));
                            return;
                        }
                        result = tokio::time::timeout(LEARN_TIMEOUT, &mut request) => result,
                    };
                    match result {
                        Ok(Ok(drafts)) => {
                            let proposed = drafts.len();
                            match people.finish_impression_batch(&batch, drafts, now_ms()) {
                                Ok(saved) => report(
                                    "learned",
                                    Some(&batch),
                                    json!({"proposed": proposed, "saved": saved}),
                                ),
                                Err(error) => report(
                                    "rejected",
                                    Some(&batch),
                                    json!({"proposed": proposed, "error": error.to_string()}),
                                ),
                            }
                        }
                        failed => {
                            let error = match failed {
                                Ok(Err(error)) => error.to_string(),
                                _ => "timeout".into(),
                            };
                            let _ = people.finish_impression_batch(&batch, vec![], now_ms());
                            report("failed", Some(&batch), json!({"error": error}));
                            pause = PROVIDER_COOLDOWN;
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    report("unavailable", None, json!({"error": error.to_string()}));
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
