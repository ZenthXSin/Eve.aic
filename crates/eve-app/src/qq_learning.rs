//! 宿主低频扫描已保存经历，持久化准入后才调用受限提炼器。
use crate::AppError;
use eve_learning_api::{
    AutoConfirmationPolicy, LearningAdmin, LearningChangeToken, LearningError, LearningFailure,
    LearningOptions, LearningOutcome, PreferenceExtractor,
};
use eve_memory_api::{MemoryAdmin, MemoryScope};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

#[derive(Default)]
struct ScanSchedule {
    previous: BTreeMap<MemoryScope, (u64, LearningChangeToken, tokio::time::Instant)>,
}
impl ScanSchedule {
    fn admit(
        &mut self,
        scope: &MemoryScope,
        revision: u64,
        token: Option<LearningChangeToken>,
        now: tokio::time::Instant,
        recheck: Duration,
    ) -> bool {
        let Some(token) = token else {
            return true;
        };
        if self
            .previous
            .get(scope)
            .is_some_and(|(old_revision, old_token, at)| {
                *old_revision == revision
                    && *old_token == token
                    && now
                        .checked_duration_since(*at)
                        .is_some_and(|age| age < recheck)
            })
        {
            return false;
        }
        // 保存准入前的标识。提炼或确认随后改变账本时，下一次轮询必须再次准入。
        self.previous.insert(scope.clone(), (revision, token, now));
        true
    }
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
        gate: crate::qq_budget::Gate,
        confirmation: Option<Arc<dyn AutoConfirmationPolicy>>,
    ) -> Result<Self, AppError> {
        options.validate()?;
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放，宿主将关闭通道，避免假装后台仍正常。
            let result = run(
                memory,
                learning,
                extractor,
                (options, gate),
                confirmation,
                activated,
                stopped,
            )
            .await;
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
    settings: (LearningOptions, crate::qq_budget::Gate),
    confirmation: Option<Arc<dyn AutoConfirmationPolicy>>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), AppError> {
    let (options, gate) = settings;
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
    let mut schedule = ScanSchedule::default();
    // 零冷却只表示新证据无需等待；空闲时仍按有限周期复核，不把它变成磁盘忙轮询。
    let recheck = Duration::from_millis(match options.cooldown_ms {
        0 => 30_000,
        value => value.clamp(250, 30_000),
    });
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        let mut ready = Vec::new();
        if confirmation.is_some() || remaining > 0 {
            let mut scopes = memory.revisions("qq")?;
            if scopes.len() > eve_memory_api::MAX_EVIDENCE {
                return Err("学习范围超过上限".into());
            }
            scopes.sort_by(|a, b| a.0.cmp(&b.0));
            if scopes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err("学习范围修订重复".into());
            }
            schedule
                .previous
                .retain(|scope, _| scopes.binary_search_by(|(item, _)| item.cmp(scope)).is_ok());
            let now = tokio::time::Instant::now();
            for (scope, revision) in scopes {
                scope.validate()?;
                if scope.channel != "qq" {
                    continue;
                }
                let token = learning.change_token(&scope)?;
                if schedule.admit(&scope, revision, token, now, recheck) {
                    let reader = memory.reader(scope.clone())?;
                    ready.push((scope, reader));
                }
            }
        }
        // 自主模式按持久化冷却持续接收新证据；总容量仍由 Learning 插件限制。
        if let Some(policy) = &confirmation {
            remaining = options.max_executions;
            for (scope, _) in &ready {
                if *stopped.borrow() {
                    return Ok(());
                }
                if scope.channel == "qq" {
                    crate::qq_learning_commands::auto_confirm(
                        memory.as_ref(),
                        learning.as_ref(),
                        policy.as_ref(),
                        scope,
                        now_ms()?,
                    )?;
                }
            }
        }
        if remaining > 0 && gate.admits_category("memory-maintenance", 1)? {
            for (position, (scope, reader)) in ready.iter().enumerate() {
                if *stopped.borrow() {
                    return Ok(());
                }
                // 此组合入口只绑定 QQ。其他受信宿主需自行装配跨通道读取权限。
                if scope.channel != "qq" {
                    continue;
                }
                if !gate.admits_category("memory-maintenance", 1)? {
                    break;
                }
                // 完整校验固定版本来源，保留确切批次，避免恢复整份原始记忆。
                let reserved = learning.reserve_reader(
                    scope,
                    reader.as_ref(),
                    now_ms()?,
                    extractor.version(),
                    &options,
                );
                let batch = match reserved {
                    Ok(Some(batch)) => batch,
                    Ok(None) => {
                        match learning.reserve_repair_reader(
                            scope,
                            reader.as_ref(),
                            now_ms()?,
                            extractor.version(),
                        )? {
                            Some(batch) => batch,
                            None => continue,
                        }
                    }
                    Err(LearningError::LimitReached) => {
                        remaining = 0;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                remaining -= 1;
                let began = batch.started_at_ms;
                let outcome = {
                    let request = extractor.extract_maintenance(batch.clone());
                    tokio::pin!(request);
                    tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => LearningOutcome::Failed(LearningFailure::Cancelled),
                        result = tokio::time::timeout(Duration::from_secs(30), &mut request) => match result {
                            Ok(Ok(value)) => LearningOutcome::Maintained(value),
                            Ok(Err(LearningError::Extraction(failure))) => LearningOutcome::Failed(failure),
                            Ok(Err(_)) => LearningOutcome::Failed(LearningFailure::Provider),
                            Err(_) => LearningOutcome::Failed(LearningFailure::Timeout),
                        },
                    }
                };
                // 当前尝试先持久收尾。只读提炼的补做由独立准入负责，原失败保留且次数受限。
                learning.finish(&batch, now_ms()?.max(began), outcome)?;
                if remaining == 0 {
                    // 本轮额度用尽不代表后续范围已完成提炼检查。自主模式下一轮
                    // 会续领原有批次额度，不能让预先计算的 ready 将它们负缓存。
                    for (pending, _) in ready.iter().skip(position + 1) {
                        schedule.previous.remove(pending);
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use eve_learning_api::*;
    use eve_memory_api::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    fn scope() -> MemoryScope {
        MemoryScope {
            channel: "qq".into(),
            session_id: "idle".into(),
            user_id: "alice".into(),
        }
    }
    #[test]
    fn unchanged_tokens_have_a_finite_recheck_and_new_evidence_or_service_wakes_immediately() {
        let scope = scope();
        let now = tokio::time::Instant::now();
        let interval = Duration::from_secs(30);
        let token = LearningChangeToken {
            instance: 1,
            revision: 0,
        };
        let mut schedule = ScanSchedule::default();
        assert!(schedule.admit(&scope, 10, Some(token), now, interval));
        assert!(!schedule.admit(
            &scope,
            10,
            Some(token),
            now + Duration::from_secs(29),
            interval
        ));
        assert!(schedule.admit(&scope, 10, Some(token), now + interval, interval));
        assert!(schedule.admit(&scope, 11, Some(token), now + interval, interval));
        assert!(schedule.admit(
            &scope,
            11,
            Some(LearningChangeToken {
                revision: 1,
                ..token
            }),
            now + interval,
            interval
        ));
        assert!(schedule.admit(
            &scope,
            11,
            Some(LearningChangeToken {
                instance: 2,
                revision: 0
            }),
            now + interval,
            interval
        ));
    }
    #[test]
    fn legacy_implementations_never_enter_negative_cache_and_clock_reversal_forces_recheck() {
        let scope = scope();
        let now = tokio::time::Instant::now();
        let interval = Duration::from_secs(30);
        let mut schedule = ScanSchedule::default();
        for _ in 0..3 {
            assert!(schedule.admit(&scope, 0, None, now, interval));
        }
        let token = Some(LearningChangeToken {
            instance: 1,
            revision: 0,
        });
        assert!(schedule.admit(&scope, 0, token, now + interval, interval));
        assert!(schedule.admit(&scope, 0, token, now, interval));
    }
    #[derive(Clone)]
    struct RevisionMemory(Arc<AtomicU64>);
    impl MemoryService for RevisionMemory {
        fn revision(&self) -> MemoryResult<u64> {
            Ok(self.0.load(Ordering::SeqCst))
        }
        fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
            panic!("空闲调度不应加载正文")
        }
    }
    impl MemoryAdmin for RevisionMemory {
        fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
            Ok(vec![scope()])
        }
        fn reader(&self, _: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
            Ok(Arc::new(self.clone()))
        }
        fn import_completed(
            &self,
            _: &MemoryScope,
            _: u64,
            _: CompletedInteraction,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("只读调度")
        }
        fn update_preference(
            &self,
            _: &MemoryScope,
            _: u64,
            _: PreferenceChange,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("只读调度")
        }
    }
    struct CountLearning {
        scans: AtomicUsize,
        revision: AtomicU64,
    }
    impl LearningAdmin for CountLearning {
        fn change_token(&self, _: &MemoryScope) -> LearningResult<Option<LearningChangeToken>> {
            Ok(Some(LearningChangeToken {
                instance: 1,
                revision: self.revision.load(Ordering::SeqCst),
            }))
        }
        fn snapshot(&self, _: &MemoryScope) -> LearningResult<LearningSnapshot> {
            panic!("本测试没有自动确认候选")
        }
        fn reserve(
            &self,
            _: &MemorySnapshot,
            _: u64,
            _: &str,
            _: &LearningOptions,
        ) -> LearningResult<Option<LearningBatch>> {
            panic!("应使用分页准入")
        }
        fn reserve_reader(
            &self,
            _: &MemoryScope,
            _: &dyn MemoryService,
            _: u64,
            _: &str,
            _: &LearningOptions,
        ) -> LearningResult<Option<LearningBatch>> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
        fn reserve_repair_reader(
            &self,
            _: &MemoryScope,
            _: &dyn MemoryService,
            _: u64,
            _: &str,
        ) -> LearningResult<Option<LearningBatch>> {
            Ok(None)
        }
        fn finish(&self, _: &LearningBatch, _: u64, _: LearningOutcome) -> LearningResult<()> {
            panic!("没有批次，不得伪造收尾")
        }
    }
    struct NoModel;
    impl PreferenceExtractor for NoModel {
        fn version(&self) -> &str {
            "no-model"
        }
        fn extract(&self, _: LearningBatch) -> LearningFuture<'_, Vec<CandidateDraft>> {
            Box::pin(async { panic!("空闲或不足证据不应请求模型") })
        }
    }
    async fn scans(learning: &CountLearning, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while learning.scans.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("新变更应在正常轮询中唤醒");
    }
    #[tokio::test]
    async fn actual_background_keeps_polling_metadata_without_rescanning_idle_history() {
        for cooldown_ms in [0, 300_000] {
            let memory = Arc::new(RevisionMemory(Arc::new(AtomicU64::new(0))));
            let learning = Arc::new(CountLearning {
                scans: 0.into(),
                revision: 0.into(),
            });
            let background = Background::start(
                memory.clone(),
                learning.clone(),
                Arc::new(NoModel),
                LearningOptions {
                    cooldown_ms,
                    ..LearningOptions::default()
                },
                crate::qq_budget::Gate::default(),
                None,
            )
            .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                learning.scans.load(Ordering::SeqCst),
                0,
                "未激活通道不能扫描"
            );
            background.activate();
            scans(&learning, 1).await;
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(
                learning.scans.load(Ordering::SeqCst),
                1,
                "多个空闲轮询不应重复扫描正文"
            );
            memory.0.store(1, Ordering::SeqCst);
            scans(&learning, 2).await;
            learning.revision.store(1, Ordering::SeqCst);
            scans(&learning, 3).await;
            background.stop().await.unwrap();
            assert_eq!(learning.scans.load(Ordering::SeqCst), 3);
        }
    }
}
