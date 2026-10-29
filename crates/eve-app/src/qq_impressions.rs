//! 人物印象的后台学习：此人谈及自己的原话攒够一批后学习一次；同一循环里整理跨会话画像。
//!
//! 取出即移出缓冲，失败、超时或中断不重试；结局写到标准错误的 `EVE_IMPRESSIONS` 与
//! `EVE_PROFILES` 行，不含原话。热配置 `runtime.people.impressions` 同时控制两者是否发起模型请求。
use crate::AppError;
use eve_config_api::ConfigService;
use eve_people_api::{
    ImpressionBatch, ImpressionLearner, PeopleDirectory, ProfileConsolidator, ProfileRequest,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
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

/// 至多排队的画像请求；满了丢弃最早的，依据再变化时会重新排上。
const MAX_PENDING_PROFILES: usize = 16;
/// 记住至多这么多人的已尝试指纹；超出时清空，至多多整理一次。
const MAX_ATTEMPTED_PROFILES: usize = 1024;

/// 待整理的画像请求，只在内存中。同一人只留最新一份；某份依据取出整理过（不论成败）就不再重试，
/// 依据变化得到新指纹后才再整理。重启后清空，下次对话时按需重新排队。
#[derive(Default)]
pub(crate) struct ProfileQueue {
    inner: Mutex<(VecDeque<ProfileRequest>, BTreeMap<String, String>)>,
}
impl ProfileQueue {
    pub(crate) fn offer(&self, request: ProfileRequest) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let (pending, attempted) = &mut *inner;
        if attempted.get(&request.participant) == Some(&request.fingerprint) {
            return;
        }
        pending.retain(|r| r.participant != request.participant);
        if pending.len() == MAX_PENDING_PROFILES {
            pending.pop_front();
        }
        pending.push_back(request);
    }
    fn take(&self) -> Option<ProfileRequest> {
        let mut inner = self.inner.lock().ok()?;
        let (pending, attempted) = &mut *inner;
        let request = pending.pop_front()?;
        if attempted.len() == MAX_ATTEMPTED_PROFILES {
            attempted.clear();
        }
        attempted.insert(request.participant.clone(), request.fingerprint.clone());
        Some(request)
    }
}

fn report_profile(outcome: &str, request: &ProfileRequest, detail: serde_json::Value) {
    eprintln!(
        "EVE_PROFILES {}",
        json!({"outcome": outcome, "participant": request.participant,
            "sources": request.sources.len(), "at_ms": now_ms(), "detail": detail})
    );
}

/// 画像整理器与队列；不配置时只学印象。
pub(crate) struct Profiles {
    pub(crate) consolidator: Arc<dyn ProfileConsolidator>,
    pub(crate) queue: Arc<ProfileQueue>,
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
        budget: crate::qq_budget::Gate,
        profiles: Profiles,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                people, learner, policy, budget, profiles, activated, stopped,
            )
            .await;
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
    budget: crate::qq_budget::Gate,
    profiles: Profiles,
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
        if pause == POLL
            && policy.learning()
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
            && let Some(request) = profiles.queue.take()
        {
            let consolidation = profiles.consolidator.consolidate(request.clone());
            tokio::pin!(consolidation);
            let result = tokio::select! {
                biased;
                _ = stop_requested(&mut stopped) => {
                    report_profile("cancelled", &request, json!({}));
                    return;
                }
                result = tokio::time::timeout(LEARN_TIMEOUT, &mut consolidation) => result,
            };
            match result {
                Ok(Ok(facts)) => {
                    let proposed = facts.len();
                    match people.save_profile(&request, facts, now_ms()) {
                        Ok(()) => report_profile("saved", &request, json!({"facts": proposed})),
                        Err(error) => report_profile(
                            "rejected",
                            &request,
                            json!({"facts": proposed, "error": error.to_string()}),
                        ),
                    }
                }
                failed => {
                    let error = match failed {
                        Ok(Err(error)) => error.to_string(),
                        _ => "timeout".into(),
                    };
                    report_profile("failed", &request, json!({"error": error}));
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
