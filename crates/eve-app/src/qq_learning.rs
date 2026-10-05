//! 宿主低频扫描已保存经历，持久化准入后才调用受限提炼器。
use crate::AppError;
use eve_learning_api::{
    LearningAdmin, LearningError, LearningFailure, LearningOptions, LearningOutcome,
    PreferenceExtractor,
};
use eve_memory_api::MemoryAdmin;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(
        memory: Arc<dyn MemoryAdmin>,
        learning: Arc<dyn LearningAdmin>,
        extractor: Arc<dyn PreferenceExtractor>,
        options: LearningOptions,
    ) -> Result<Self, AppError> {
        options.validate()?;
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放，宿主将关闭通道，避免假装后台仍正常。
            let result = run(memory, learning, extractor, options, activated, stopped).await;
            let _ = finished_sender.send(true);
            result
        });
        Ok(Self {
            active,
            stop,
            finished,
            task,
        })
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "偏好提炼后台异常；已停止准入")?
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
    memory: Arc<dyn MemoryAdmin>,
    learning: Arc<dyn LearningAdmin>,
    extractor: Arc<dyn PreferenceExtractor>,
    options: LearningOptions,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), AppError> {
    loop {
        if *active.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            changed = active.changed() => if changed.is_err() { return Ok(()); },
        }
    }
    let mut remaining = options.max_executions;
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        if remaining > 0 {
            let mut scopes = memory.scopes()?;
            if scopes.len() > eve_memory_api::MAX_EVIDENCE {
                return Err("提炼范围超过上限".into());
            }
            scopes.sort();
            scopes.dedup();
            for scope in scopes {
                if *stopped.borrow() {
                    return Ok(());
                }
                // 此组合入口只绑定 QQ。其他受信宿主需自行装配跨通道读取权限。
                if scope.channel != "qq" {
                    continue;
                }
                let snapshot = memory.reader(scope.clone())?.snapshot()?;
                if snapshot.scope != scope {
                    return Err("提炼来源作用域不匹配".into());
                }
                let reserved =
                    learning.reserve(&snapshot, now_ms()?, extractor.version(), &options);
                let batch = match reserved {
                    Ok(Some(batch)) => batch,
                    Ok(None) => continue,
                    Err(LearningError::LimitReached) => {
                        remaining = 0;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                remaining -= 1;
                let began = batch.started_at_ms;
                let outcome = {
                    let request = extractor.extract(batch.clone());
                    tokio::pin!(request);
                    tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => LearningOutcome::Failed(LearningFailure::Cancelled),
                        result = tokio::time::timeout(Duration::from_secs(30), &mut request) => match result {
                            Ok(Ok(candidates)) => LearningOutcome::Completed(candidates),
                            Ok(Err(LearningError::Extraction(failure))) => LearningOutcome::Failed(failure),
                            Ok(Err(_)) => LearningOutcome::Failed(LearningFailure::Provider),
                            Err(_) => LearningOutcome::Failed(LearningFailure::Timeout),
                        },
                    }
                };
                // 远端请求可能已经发生。取消/超时也消费这批证据，恢复不自动重试。
                learning.finish(&batch, now_ms()?.max(began), outcome)?;
                if remaining == 0 {
                    break;
                }
            }
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
}
