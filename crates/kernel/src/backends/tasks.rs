use eve_plugin_api::{
    PluginError, PluginFuture, PluginId, PluginResult, TaskAction, TaskId, TaskInfo, TaskManager,
    TaskMode, TaskRunReport, TaskSchedule, TaskShutdownReport, TaskSignal, TaskSpec, TaskState,
};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Default)]
pub struct TokioTaskManager {
    inner: Arc<TaskManagerInner>,
}

#[derive(Default)]
struct TaskManagerInner {
    next_id: AtomicU64,
    tasks: Mutex<HashMap<TaskId, Arc<TaskEntry>>>,
}

struct TaskEntry {
    owner: PluginId,
    name: String,
    mode: TaskMode,
    schedule: TaskSchedule,
    state: Mutex<TaskState>,
    signal: Arc<TokioTaskSignal>,
    handle: Mutex<Option<JoinHandle<PluginResult<()>>>>,
}

struct TokioTaskSignal {
    state: Arc<TaskSignalState>,
}

struct TaskSignalState {
    cancelled: AtomicBool,
    sender: watch::Sender<bool>,
    receiver: Mutex<watch::Receiver<bool>>,
}

impl TokioTaskSignal {
    fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::SeqCst) {
            let _ = self.state.sender.send(true);
        }
    }
}

impl TaskSignal for TokioTaskSignal {
    fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let state = self.state.clone();
        let mut receiver = state.receiver.lock().expect("任务信号锁中毒").clone();
        Box::pin(async move {
            if state.cancelled.load(Ordering::SeqCst) {
                return;
            }
            let _ = receiver.changed().await;
        })
    }
}

impl TokioTaskManager {
    fn allocate(&self, owner: PluginId, spec: &TaskSpec) -> (TaskId, Arc<TaskEntry>) {
        let id = TaskId::new(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let entry = Arc::new(TaskEntry {
            owner,
            name: spec.name.clone(),
            mode: spec.mode,
            schedule: spec.schedule.clone(),
            state: Mutex::new(TaskState::Scheduled),
            signal: Arc::new(TokioTaskSignal {
                state: Arc::new({
                    let (sender, receiver) = watch::channel(false);
                    TaskSignalState {
                        cancelled: AtomicBool::new(false),
                        sender,
                        receiver: Mutex::new(receiver),
                    }
                }),
            }),
            handle: Mutex::new(None),
        });
        self.inner
            .tasks
            .lock()
            .expect("任务锁中毒")
            .insert(id.clone(), entry.clone());
        (id, entry)
    }

    fn run_action(
        action: TaskAction,
        signal: Arc<TokioTaskSignal>,
        schedule: TaskSchedule,
    ) -> TaskFutureBox {
        Box::pin(async move {
            match schedule {
                TaskSchedule::Immediate => (action)(signal).await,
                TaskSchedule::After(delay) => {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => (action)(signal).await,
                        _ = signal.cancelled() => Ok(()),
                    }
                }
                TaskSchedule::Every { interval, runs } => {
                    let mut completed = 0_u32;
                    loop {
                        if signal.is_cancelled() {
                            return Ok(());
                        }
                        (action)(signal.clone()).await?;
                        completed += 1;
                        if runs.is_some_and(|limit| completed >= limit) {
                            return Ok(());
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(interval) => {},
                            _ = signal.cancelled() => return Ok(()),
                        }
                    }
                }
            }
        })
    }

    fn spawn_entry(&self, entry: Arc<TaskEntry>, spec: TaskSpec) -> PluginResult<()> {
        let action = spec.action.clone();
        let signal = entry.signal.clone();
        let schedule = spec.schedule;
        let entry_for_run = entry.clone();
        let handle = tokio::spawn(async move {
            *entry_for_run.state.lock().expect("任务状态锁中毒") = TaskState::Running;
            let result = Self::run_action(action, signal, schedule).await;
            *entry_for_run.state.lock().expect("任务状态锁中毒") = match &result {
                Ok(()) => TaskState::Finished,
                Err(_) => TaskState::Failed,
            };
            result
        });
        *entry.handle.lock().expect("任务句柄锁中毒") = Some(handle);
        Ok(())
    }
}

type TaskFutureBox = Pin<Box<dyn Future<Output = PluginResult<()>> + Send + 'static>>;

impl TaskManager for TokioTaskManager {
    fn spawn(&self, owner: PluginId, spec: TaskSpec) -> PluginResult<TaskId> {
        if spec.mode != TaskMode::Background {
            return Err(PluginError::Task(
                "只有 Background 任务可以通过 spawn_task 创建".into(),
            ));
        }
        let (id, entry) = self.allocate(owner, &spec);
        if let Err(error) = self.spawn_entry(entry, spec) {
            self.inner.tasks.lock().expect("任务锁中毒").remove(&id);
            return Err(error);
        }
        Ok(id)
    }

    fn run_foreground(self: Arc<Self>, owner: PluginId, spec: TaskSpec) -> PluginFutureBoxReport {
        Box::pin(async move {
            if spec.mode != TaskMode::Foreground {
                return Err(PluginError::Task(
                    "只有 Foreground 任务可以通过 run_foreground 执行".into(),
                ));
            }
            let (id, entry) = self.allocate(owner, &spec);
            *entry.state.lock().expect("任务状态锁中毒") = TaskState::Running;
            let mut report = TaskRunReport {
                id: id.clone(),
                runs: 0,
                errors: Vec::new(),
            };
            let action = spec.action.clone();
            let schedule = spec.schedule.clone();
            let result = Self::run_action(action, entry.signal.clone(), schedule.clone()).await;
            match result {
                Ok(()) => {
                    report.runs = match schedule {
                        TaskSchedule::Every { runs: Some(r), .. } => r,
                        _ => 1,
                    };
                    *entry.state.lock().expect("任务状态锁中毒") = TaskState::Finished;
                }
                Err(error) => {
                    report.errors.push(error.to_string());
                    *entry.state.lock().expect("任务状态锁中毒") = TaskState::Failed;
                }
            }
            self.inner.tasks.lock().expect("任务锁中毒").remove(&id);
            Ok(report)
        })
    }

    fn list(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>> {
        let entries = self.inner.tasks.lock().expect("任务锁中毒");
        Ok(entries
            .iter()
            .filter(|(_, entry)| entry.owner == *owner)
            .map(|(id, entry)| TaskInfo {
                id: id.clone(),
                owner: entry.owner.clone(),
                name: entry.name.clone(),
                mode: entry.mode,
                schedule: entry.schedule.clone(),
                state: *entry.state.lock().expect("任务状态锁中毒"),
            })
            .collect())
    }

    fn shutdown(
        self: Arc<Self>,
        owner: &PluginId,
        timeout: Duration,
    ) -> PluginFuture<'static, TaskShutdownReport> {
        let owner = owner.clone();
        Box::pin(async move {
            let entries: Vec<_> = self
                .inner
                .tasks
                .lock()
                .expect("任务锁中毒")
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect();
            for (_, entry) in &entries {
                *entry.state.lock().expect("任务状态锁中毒") = TaskState::Stopping;
                entry.signal.cancel();
            }
            let deadline = tokio::time::Instant::now() + timeout;
            let mut report = TaskShutdownReport::default();
            for (id, entry) in entries {
                let Some(mut handle) = entry.handle.lock().expect("任务句柄锁中毒").take()
                else {
                    report.stopped.push(id);
                    continue;
                };
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(remaining, &mut handle).await {
                    Ok(Ok(Ok(()))) => report.stopped.push(id.clone()),
                    Ok(Ok(Err(error))) => report.errors.push(format!("{id}: {error}")),
                    Ok(Err(error)) => report.errors.push(format!("{id}: {error}")),
                    Err(_) => {
                        handle.abort();
                        let _ = handle.await;
                        *entry.state.lock().expect("任务状态锁中毒") = TaskState::TimedOut;
                        report.timed_out.push(id.clone());
                    }
                }
                self.inner.tasks.lock().expect("任务锁中毒").remove(&id);
            }
            Ok(report)
        })
    }
}

type PluginFutureBoxReport =
    Pin<Box<dyn Future<Output = PluginResult<TaskRunReport>> + Send + 'static>>;
