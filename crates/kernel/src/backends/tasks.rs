//! Tokio 任务执行器。登记与执行分开，前台与后台使用同一条停止路径。

use eve_plugin_api::{
    PluginError, PluginFuture, PluginId, PluginResult, TaskId, TaskInfo, TaskManager, TaskMode,
    TaskRunReport, TaskSchedule, TaskScheduleInfo, TaskShutdownReport, TaskSignal, TaskSpec,
    TaskState, TaskTypeId,
};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep_until, timeout_at};

#[derive(Default)]
pub struct TokioTaskManager {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: AtomicU64,
    tasks: Mutex<HashMap<TaskId, Arc<Entry>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for entry in self.tasks.get_mut().expect("任务锁中毒").values() {
            entry.execution.signal.cancel();
            entry.abort.abort();
        }
    }
}

struct Entry {
    id: TaskId,
    owner: PluginId,
    name: String,
    task_type: TaskTypeId,
    mode: TaskMode,
    schedule: TaskScheduleInfo,
    execution: Arc<Execution>,
    abort: AbortHandle,
    outcome: watch::Receiver<Option<TaskRunReport>>,
    fallback: Mutex<Option<TaskRunReport>>,
    retain: AtomicBool,
}

struct Execution {
    progress: Mutex<Progress>,
    signal: Arc<StopSignal>,
}

struct Progress {
    state: TaskState,
    runs: u32,
}

struct StopSignal {
    sender: watch::Sender<bool>,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            sender: watch::channel(false).0,
        }
    }
    fn cancel(&self) {
        self.sender.send_replace(true);
    }
}

impl TaskSignal for StopSignal {
    fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let mut receiver = self.sender.subscribe();
        Box::pin(async move {
            // subscribe 先于检查：取消发生在任意一个位置均不会丢失。
            loop {
                if *receiver.borrow_and_update() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

impl Execution {
    fn set_state(&self, state: TaskState) {
        let mut progress = self.progress.lock().expect("任务状态锁中毒");
        if !matches!(progress.state, TaskState::Stopping | TaskState::TimedOut) {
            progress.state = state;
        }
    }
}

impl TokioTaskManager {
    fn register(
        &self,
        owner: PluginId,
        spec: TaskSpec,
        mode: TaskMode,
    ) -> PluginResult<Arc<Entry>> {
        spec.validate()?;
        if spec.mode != mode {
            return Err(PluginError::Task("任务模式与调用入口不一致".into()));
        }
        let runtime = Handle::try_current()
            .map_err(|_| PluginError::Task("创建任务需要 Tokio Runtime".into()))?;
        let delay = match spec.schedule {
            TaskSchedule::After(delay) => delay,
            TaskSchedule::At(time) => time.duration_since(SystemTime::now()).unwrap_or_default(),
            _ => Duration::ZERO,
        };
        let first_due = deadline(delay)?;
        if let TaskSchedule::Every { interval, .. } = spec.schedule {
            deadline(interval)?;
        }
        let id = TaskId::new(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let execution = Arc::new(Execution {
            progress: Mutex::new(Progress {
                state: TaskState::Scheduled,
                runs: 0,
            }),
            signal: Arc::new(StopSignal::new()),
        });
        let (sender, outcome) = watch::channel(None);
        // 锁覆盖任务登记，shutdown 不会看见未安装取消句柄的半成品。
        let mut tasks = self.inner.tasks.lock().expect("任务锁中毒");
        prune_unretained_finished(&mut tasks);
        let worker_execution = execution.clone();
        let name = spec.name.clone();
        let task_type = spec.task_type.clone();
        let schedule = spec.schedule.info();
        let worker = runtime.spawn(async move { execute(spec, worker_execution, first_due).await });
        let entry = Arc::new(Entry {
            id: id.clone(),
            owner,
            name,
            task_type,
            mode,
            schedule,
            execution,
            abort: worker.abort_handle(),
            outcome,
            fallback: Mutex::new(None),
            retain: AtomicBool::new(true),
        });
        tasks.insert(id, entry.clone());
        drop(tasks);
        let monitored = entry.clone();
        let manager = Arc::downgrade(&self.inner);
        // JoinHandle 只由监视器等待，前台等待和 shutdown 共享结果而不争夺句柄。
        runtime.spawn(async move {
            let joined = worker.await;
            let (state, errors) = match joined {
                Ok(Ok(cancelled)) => (
                    if cancelled {
                        TaskState::Cancelled
                    } else {
                        TaskState::Finished
                    },
                    Vec::new(),
                ),
                Ok(Err(error)) => (TaskState::Failed, vec![error.to_string()]),
                Err(error) if error.is_cancelled() => (TaskState::Cancelled, Vec::new()),
                Err(error) => (TaskState::Failed, vec![error.to_string()]),
            };
            let report = {
                let mut progress = monitored.execution.progress.lock().expect("任务状态锁中毒");
                if progress.state != TaskState::TimedOut {
                    progress.state = state;
                }
                TaskRunReport {
                    id: monitored.id.clone(),
                    task_type: monitored.task_type.clone(),
                    runs: progress.runs,
                    state: progress.state,
                    errors,
                }
            };
            // Join 完成意味着动作及其捕获资源已经释放。
            sender.send_replace(Some(report));
            if !monitored.retain.load(Ordering::SeqCst) {
                remove(&manager, &monitored.id);
            }
        });
        Ok(entry)
    }
}

async fn execute(
    spec: TaskSpec,
    execution: Arc<Execution>,
    first_due: Instant,
) -> PluginResult<bool> {
    if execution.signal.is_cancelled() {
        return Ok(true);
    }
    // 用户工厂在 worker 内执行，不占用登记锁；每个实例拥有独立的调度状态。
    let mut scheduler = match &spec.schedule {
        TaskSchedule::Custom { factory, .. } => Some(factory()),
        _ => None,
    };
    let mut due = first_due;
    loop {
        execution.set_state(TaskState::Scheduled);
        if let Some(scheduler) = &mut scheduler {
            let completed = execution.progress.lock().expect("任务状态锁中毒").runs;
            let next = tokio::select! {
                biased;
                _ = execution.signal.cancelled() => return Ok(true),
                result = scheduler.next(completed) => result?
            };
            match next {
                Some(delay) => due = deadline(delay)?,
                None => return Ok(execution.signal.is_cancelled()),
            }
        }
        tokio::select! {
            biased;
            _ = execution.signal.cancelled() => return Ok(true),
            _ = sleep_until(due) => {}
        }
        if execution.signal.is_cancelled() {
            return Ok(true);
        }
        execution.set_state(TaskState::Running);
        (spec.action)(execution.signal.clone()).await?;
        let runs = {
            let mut progress = execution.progress.lock().expect("任务状态锁中毒");
            progress.runs = progress.runs.saturating_add(1);
            progress.runs
        };
        match spec.schedule {
            TaskSchedule::Custom { max_runs, .. }
                if max_runs.is_none_or(|limit| runs < limit) =>
            {
                if execution.signal.is_cancelled() {
                    return Ok(true);
                }
            }
            TaskSchedule::Every {
                interval,
                runs: limit,
            } if limit.is_none_or(|limit| runs < limit) => {
                if execution.signal.is_cancelled() {
                    return Ok(true);
                }
                // 固定延迟：不追赶错过的 tick，也不会让同一个动作重叠执行。
                due = deadline(interval)?;
            }
            _ => return Ok(execution.signal.is_cancelled()),
        }
    }
}

fn deadline(duration: Duration) -> PluginResult<Instant> {
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| PluginError::Task("任务时间超出支持范围".into()))
}

async fn wait(entry: &Entry) -> PluginResult<TaskRunReport> {
    let mut receiver = entry.outcome.clone();
    loop {
        if let Some(report) = receiver.borrow_and_update().clone() {
            return Ok(report);
        }
        if let Some(report) = read_report(entry) {
            return Ok(report);
        }
        if receiver.changed().await.is_err() {
            // watch 发送端由监视器持有；通道关闭且没有报告，说明监视器未能发布结果。
            entry.execution.signal.cancel();
            entry.abort.abort();
            loop {
                if let Some(report) = read_report(entry) {
                    return Ok(report);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// 监视器丢失即判失败，因为原始结果不可用。只有 worker 确认退出后才能报告退出；
/// abort 无法强制终止同步阻塞代码。
fn recover_monitor_report(entry: &Entry) -> TaskRunReport {
    let mut fallback = entry.fallback.lock().expect("任务报告锁中毒");
    if let Some(report) = fallback.as_ref() {
        return report.clone();
    }
    let mut progress = entry.execution.progress.lock().expect("任务状态锁中毒");
    if progress.state != TaskState::TimedOut {
        progress.state = TaskState::Failed;
    }
    let report = TaskRunReport {
        id: entry.id.clone(),
        task_type: entry.task_type.clone(),
        runs: progress.runs,
        state: progress.state,
        errors: vec!["任务监视器已中断，原始执行结果不可用".into()],
    };
    *fallback = Some(report.clone());
    report
}

/// 正常监视器报告优先；确认监视器关闭且 worker 退出后才缓存回退报告。
fn read_report(entry: &Entry) -> Option<TaskRunReport> {
    if let Some(report) = entry.outcome.borrow().clone() {
        return Some(report);
    }
    let closed_without_report = entry.outcome.has_changed().is_err();
    // has_changed 可能因 sender 已关闭而返回 Err，即使关闭前刚发送了结果；重读以保留正常报告。
    if closed_without_report && let Some(report) = entry.outcome.borrow().clone() {
        return Some(report);
    }

    if let Some(report) = entry.fallback.lock().expect("任务报告锁中毒").clone() {
        return Some(report);
    }
    if closed_without_report && entry.abort.is_finished() {
        return Some(recover_monitor_report(entry));
    }
    None
}

fn monitor_lost(entry: &Entry) -> bool {
    if entry.outcome.has_changed().is_ok() {
        return false;
    }
    entry.outcome.borrow().is_none()
}

fn prune_unretained_finished(tasks: &mut HashMap<TaskId, Arc<Entry>>) {
    tasks.retain(|_, entry| {
        if entry.retain.load(Ordering::SeqCst) {
            return true;
        }
        let report = read_report(entry);
        if report.is_some() {
            return false;
        }
        !entry.abort.is_finished() || !monitor_lost(entry)
    });
}

fn remove(manager: &Weak<Inner>, id: &TaskId) {
    if let Some(manager) = manager.upgrade() {
        manager.tasks.lock().expect("任务锁中毒").remove(id);
    }
}

struct ForegroundWait {
    entry: Arc<Entry>,
    manager: Weak<Inner>,
}

impl Drop for ForegroundWait {
    fn drop(&mut self) {
        self.entry.retain.store(false, Ordering::SeqCst);
        if read_report(&self.entry).is_none() {
            self.entry.execution.signal.cancel();
            self.entry.abort.abort();
        } else {
            remove(&self.manager, &self.entry.id);
        }
    }
}

impl TaskManager for TokioTaskManager {
    fn spawn(&self, owner: PluginId, spec: TaskSpec) -> PluginResult<TaskId> {
        Ok(self.register(owner, spec, TaskMode::Background)?.id.clone())
    }

    fn run_foreground(
        self: Arc<Self>,
        owner: PluginId,
        spec: TaskSpec,
    ) -> PluginResult<PluginFuture<'static, TaskRunReport>> {
        let entry = self.register(owner, spec, TaskMode::Foreground)?;
        let guard = ForegroundWait {
            entry,
            manager: Arc::downgrade(&self.inner),
        };
        // 在 Future 构造前创建 guard；即使从不 poll 就丢弃也会请求取消。
        Ok(Box::pin(async move { wait(&guard.entry).await }))
    }

    fn list(&self, owner: &PluginId) -> PluginResult<Vec<TaskInfo>> {
        let mut tasks = self.inner.tasks.lock().expect("任务锁中毒");
        prune_unretained_finished(&mut tasks);
        let mut result = Vec::new();
        for entry in tasks.values().filter(|entry| entry.owner == *owner) {
            let outcome = read_report(entry);
            let lost_monitor = outcome.is_none() && monitor_lost(entry);
            let progress = entry.execution.progress.lock().expect("任务状态锁中毒");
            result.push(TaskInfo {
                id: entry.id.clone(),
                owner: entry.owner.clone(),
                name: entry.name.clone(),
                task_type: entry.task_type.clone(),
                mode: entry.mode,
                schedule: entry.schedule.clone(),
                state: outcome.as_ref().map_or_else(
                    || {
                        if lost_monitor && progress.state != TaskState::TimedOut {
                            TaskState::Failed
                        } else {
                            progress.state
                        }
                    },
                    |report| report.state,
                ),
                runs: outcome.as_ref().map_or(progress.runs, |report| report.runs),
                exited: outcome.is_some() || entry.abort.is_finished(),
            });
        }
        result.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(result)
    }

    fn shutdown(
        self: Arc<Self>,
        owner: &PluginId,
        timeout: Duration,
        abort_timeout: Duration,
    ) -> PluginFuture<'static, TaskShutdownReport> {
        let owner = owner.clone();
        Box::pin(async move {
            let end = deadline(timeout)?;
            let mut entries: Vec<_> = {
                let mut tasks = self.inner.tasks.lock().expect("任务锁中毒");
                prune_unretained_finished(&mut tasks);
                tasks
                    .values()
                    .filter(|entry| entry.owner == owner)
                    .cloned()
                    .collect()
            };
            entries.sort_by(|a, b| a.id.cmp(&b.id));
            for entry in &entries {
                if read_report(entry).is_none() {
                    entry.execution.set_state(TaskState::Stopping);
                }
                entry.execution.signal.cancel();
            }
            let mut report = TaskShutdownReport::default();
            let mut pending = Vec::new();
            for entry in entries {
                match timeout_at(end, wait(&entry)).await {
                    Ok(result) => record_exit(&self.inner, &entry, result, &mut report),
                    Err(_) => pending.push(entry),
                }
            }
            // 同时发取消，再用第二个共享截止时间确认退出。
            for entry in &pending {
                entry
                    .execution
                    .progress
                    .lock()
                    .expect("任务状态锁中毒")
                    .state = TaskState::TimedOut;
                report.timed_out.push(entry.id.clone());
                entry.abort.abort();
            }
            let abort_end = deadline(abort_timeout)?;
            for entry in pending {
                match timeout_at(abort_end, wait(&entry)).await {
                    Ok(result) => record_exit(&self.inner, &entry, result, &mut report),
                    Err(_) => report.unfinished.push(entry.id.clone()),
                }
            }
            Ok(report)
        })
    }
}

fn record_exit(
    inner: &Inner,
    entry: &Entry,
    result: PluginResult<TaskRunReport>,
    report: &mut TaskShutdownReport,
) {
    match result {
        Ok(result) => {
            report.stopped.push(entry.id.clone());
            report.errors.extend(
                result
                    .errors
                    .into_iter()
                    .map(|error| format!("{}: {error}", entry.id)),
            );
            inner.tasks.lock().expect("任务锁中毒").remove(&entry.id);
        }
        Err(error) => {
            report.errors.push(format!("{}: {error}", entry.id));
            report.unfinished.push(entry.id.clone());
        }
    }
}
