use eve_kernel::backends::TokioTaskManager;
use eve_plugin_api::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep};

fn owner() -> PluginId {
    PluginId::new("custom-tasks").unwrap()
}

async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

struct CountTask {
    count: Arc<AtomicUsize>,
}

impl Task for CountTask {
    fn task_type(&self) -> TaskTypeId {
        TaskTypeId::new("test.count").unwrap()
    }

    fn execute(&self, _signal: Arc<dyn TaskSignal>) -> PluginFuture<'_, ()> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn spec(mode: TaskMode, schedule: TaskSchedule, count: Arc<AtomicUsize>) -> TaskSpec {
    TaskSpec::from_task("计数任务", mode, schedule, Arc::new(CountTask { count })).unwrap()
}

#[tokio::test(start_paused = true)]
async fn custom_task_type_works_with_builtin_schedules_and_is_reported() {
    let manager = Arc::new(TokioTaskManager::default());
    let count = Arc::new(AtomicUsize::new(0));
    let foreground = manager
        .clone()
        .run_foreground(
            owner(),
            spec(
                TaskMode::Foreground,
                TaskSchedule::Every {
                    interval: Duration::from_secs(1),
                    runs: Some(3),
                },
                count.clone(),
            ),
        )
        .unwrap()
        .await
        .unwrap();
    assert_eq!(foreground.task_type.as_str(), "test.count");
    assert_eq!(foreground.runs, 3);
    assert_eq!(foreground.state, TaskState::Finished);
    manager
        .spawn(
            owner(),
            spec(TaskMode::Background, TaskSchedule::Immediate, count.clone()),
        )
        .unwrap();
    settle().await;
    let info = manager.list(&owner()).unwrap().pop().unwrap();
    assert_eq!(info.task_type, foreground.task_type);
    assert_eq!(info.schedule, TaskScheduleInfo::Immediate);
    assert!(info.exited);
    assert_eq!(count.load(Ordering::SeqCst), 4);
    assert!(
        manager
            .shutdown(&owner(), Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap()
            .is_clean()
    );
}

struct IncreasingDelay {
    next: u64,
}

impl TaskScheduler for IncreasingDelay {
    fn next(&mut self, completed: u32) -> PluginFuture<'_, Option<Duration>> {
        Box::pin(async move {
            // 克隆的 TaskSpec 不应共享此可变状态。
            assert_eq!(self.next, u64::from(completed) + 1);
            let delay = Duration::from_secs(self.next);
            self.next += 1;
            Ok(Some(delay))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn custom_schedule_instances_are_independent_and_run_limit_is_enforced() {
    let manager = Arc::new(TokioTaskManager::default());
    let factories = Arc::new(AtomicUsize::new(0));
    let created = factories.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let job = spec(
        TaskMode::Foreground,
        TaskSchedule::Custom {
            name: "test.increasing".into(),
            max_runs: Some(3),
            factory: Arc::new(move || {
                created.fetch_add(1, Ordering::SeqCst);
                Box::new(IncreasingDelay { next: 1 })
            }),
        },
        count.clone(),
    );
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        let start = Instant::now();
        let result = manager
            .clone()
            .run_foreground(owner(), job.clone())
            .unwrap()
            .await
            .unwrap();
        assert_eq!(result.state, TaskState::Finished);
        assert_eq!(result.runs, 3);
        assert_eq!(start.elapsed(), Duration::from_secs(6));
    }
    assert_eq!(factories.load(Ordering::SeqCst), 2);
    assert_eq!(count.load(Ordering::SeqCst), 6);
}

struct EventSchedule {
    receiver: broadcast::Receiver<()>,
    dropped: Arc<AtomicUsize>,
}

impl TaskScheduler for EventSchedule {
    fn next(&mut self, _completed: u32) -> PluginFuture<'_, Option<Duration>> {
        Box::pin(async move {
            self.receiver
                .recv()
                .await
                .map_err(|error| PluginError::Task(error.to_string()))?;
            Ok(Some(Duration::ZERO))
        })
    }
}

impl Drop for EventSchedule {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn event_triggered_tasks_stop_while_waiting_and_release_scheduler_state() {
    let manager = Arc::new(TokioTaskManager::default());
    let (events, _) = broadcast::channel(4);
    let source = events.clone();
    let dropped = Arc::new(AtomicUsize::new(0));
    let observed = dropped.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let mut job = spec(
        TaskMode::Background,
        TaskSchedule::Custom {
            name: "test.event".into(),
            max_runs: Some(2),
            factory: Arc::new(move || {
                Box::new(EventSchedule {
                    receiver: source.subscribe(),
                    dropped: observed.clone(),
                })
            }),
        },
        count.clone(),
    );
    manager.spawn(owner(), job.clone()).unwrap();
    job.mode = TaskMode::Foreground;
    let waiting = manager.clone().run_foreground(owner(), job).unwrap();
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    events.send(()).unwrap();
    // 等待零延迟计时器唤醒两个动作，然后再次进入事件等待。
    sleep(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(manager.list(&owner()).unwrap().iter().all(|info| {
        info.runs == 1
            && info.state == TaskState::Scheduled
            && info.schedule
                == TaskScheduleInfo::Custom {
                    name: "test.event".into(),
                    max_runs: Some(2),
                }
    }));
    let report = manager
        .clone()
        .shutdown(&owner(), Duration::from_secs(1), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(report.is_clean());
    assert_eq!(report.stopped.len(), 2);
    let foreground = waiting.await.unwrap();
    assert_eq!(foreground.state, TaskState::Cancelled);
    assert_eq!(foreground.runs, 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert_eq!(events.receiver_count(), 0);
    assert!(manager.list(&owner()).unwrap().is_empty());
}

struct EndAfter(u32);

impl TaskScheduler for EndAfter {
    fn next(&mut self, completed: u32) -> PluginFuture<'_, Option<Duration>> {
        Box::pin(async move { Ok((completed < self.0).then_some(Duration::from_secs(1))) })
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_can_end_before_the_limit_including_without_running() {
    for runs in [0, 2] {
        let manager = Arc::new(TokioTaskManager::default());
        let count = Arc::new(AtomicUsize::new(0));
        let report = manager
            .clone()
            .run_foreground(
                owner(),
                spec(
                    TaskMode::Foreground,
                    TaskSchedule::Custom {
                        name: "test.finite".into(),
                        max_runs: Some(5),
                        factory: Arc::new(move || Box::new(EndAfter(runs))),
                    },
                    count.clone(),
                ),
            )
            .unwrap()
            .await
            .unwrap();
        assert_eq!(report.runs, runs);
        assert_eq!(count.load(Ordering::SeqCst), runs as usize);
        assert_eq!(report.state, TaskState::Finished);
    }
}

#[tokio::test(start_paused = true)]
async fn completed_task_metadata_does_not_retain_custom_factory_captures() {
    let manager = Arc::new(TokioTaskManager::default());
    let captured = Arc::new(());
    let weak = Arc::downgrade(&captured);
    manager
        .spawn(
            owner(),
            spec(
                TaskMode::Background,
                TaskSchedule::Custom {
                    name: "test.empty".into(),
                    max_runs: None,
                    factory: Arc::new(move || {
                        let _keep = captured.clone();
                        Box::new(EndAfter(0))
                    }),
                },
                Arc::new(AtomicUsize::new(0)),
            ),
        )
        .unwrap();
    settle().await;
    let records = manager.list(&owner()).unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].exited);
    assert!(weak.upgrade().is_none());
}

struct FaultySchedule(u8);

impl TaskScheduler for FaultySchedule {
    fn next(&mut self, _completed: u32) -> PluginFuture<'_, Option<Duration>> {
        assert_ne!(self.0, 1, "同步调度 panic");
        Box::pin(async move {
            match self.0 {
                0 => Err(PluginError::Task("调度错误".into())),
                2 => panic!("异步调度 panic"),
                _ => Ok(Some(Duration::MAX)),
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_errors_panics_and_invalid_delays_are_contained() {
    let manager = Arc::new(TokioTaskManager::default());
    let count = Arc::new(AtomicUsize::new(0));
    for failure in 0..5 {
        let report = manager
            .clone()
            .run_foreground(
                owner(),
                spec(
                    TaskMode::Foreground,
                    TaskSchedule::Custom {
                        name: "test.fault".into(),
                        max_runs: Some(2),
                        factory: Arc::new(move || {
                            assert_ne!(failure, 4, "工厂 panic");
                            Box::new(FaultySchedule(failure))
                        }),
                    },
                    count.clone(),
                ),
            )
            .unwrap()
            .await
            .unwrap();
        assert_eq!(report.state, TaskState::Failed);
        assert_eq!(report.runs, 0);
        assert_eq!(report.errors.len(), 1);
        assert!(manager.list(&owner()).unwrap().is_empty());
    }
    let report = manager
        .clone()
        .run_foreground(
            owner(),
            spec(TaskMode::Foreground, TaskSchedule::Immediate, count.clone()),
        )
        .unwrap()
        .await
        .unwrap();
    assert_eq!(report.state, TaskState::Finished);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn custom_schedule_validation_cannot_be_bypassed_by_public_fields() {
    let manager = Arc::new(TokioTaskManager::default());
    assert!(TaskTypeId::new("  ").is_err());
    let calls = Arc::new(AtomicUsize::new(0));
    for (name, max_runs, mode) in [
        ("  ", Some(1), TaskMode::Background),
        ("test.invalid", Some(0), TaskMode::Background),
        ("test.invalid", None, TaskMode::Foreground),
    ] {
        let mut job = spec(mode, TaskSchedule::Immediate, Arc::new(AtomicUsize::new(0)));
        let calls = calls.clone();
        job.schedule = TaskSchedule::Custom {
            name: name.into(),
            max_runs,
            factory: Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::new(EndAfter(1))
            }),
        };
        match mode {
            TaskMode::Foreground => assert!(manager.clone().run_foreground(owner(), job).is_err()),
            TaskMode::Background => assert!(manager.spawn(owner(), job).is_err()),
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(manager.list(&owner()).unwrap().is_empty());
}
