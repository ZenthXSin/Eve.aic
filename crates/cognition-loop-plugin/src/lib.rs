//! 有界认知业务插件；使用公开状态/执行契约，模型装配留在组合层。
mod agenda;
pub use agenda::evaluate_agenda;
mod endogenous;
pub use endogenous::{
    EndogenousPlanner, ReflectionPlannerFactory, current_reflection, evaluate_reflection_agenda,
};
mod policy;
mod reflection;
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_control_api::CommitState;
use eve_plugin_api::{
    Cleanup, Event, EventId, LogEntry, LogLevel, Plugin, PluginContext, PluginDependency,
    PluginError, PluginFuture, PluginManifest, PluginResult, ServiceId, cleanup,
};
pub use policy::{
    EchoReceiptVerifier, EvidenceDrivePolicy, PriorityDrivePolicy, ReflectionDrivePolicy,
};
pub use reflection::{
    MAX_REFLECTION_JSON_BYTES, MAX_REFLECTION_TEXT_BYTES, ReflectionArtifact, ReflectionVerifier,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

fn now_ms() -> LoopResult<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LoopError::Unavailable)?;
    u64::try_from(elapsed.as_millis()).map_err(|_| LoopError::LimitReached)
}
struct Running {
    wake: mpsc::Sender<(WakeReason, Instant)>,
    stop: watch::Sender<bool>,
    cancel: watch::Sender<u64>,
    done: watch::Sender<Option<LoopResult<()>>>,
    accepting: AtomicBool,
    stats: Mutex<LoopStats>,
    task: Mutex<Option<JoinHandle<()>>>,
}
impl Running {
    fn update(&self, change: impl FnOnce(&mut LoopStats)) {
        if let Ok(mut stats) = self.stats.lock() {
            change(&mut stats);
        }
    }
    fn signal(&self, reason: WakeReason) -> LoopResult<bool> {
        if !self.accepting.load(Ordering::SeqCst) {
            return Err(LoopError::Unavailable);
        }
        self.update(|stats| stats.wakes = stats.wakes.saturating_add(1));
        match self.wake.try_send((reason, Instant::now())) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.update(|stats| {
                    stats.coalesced_wakes = stats.coalesced_wakes.saturating_add(1)
                });
                Ok(false)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(LoopError::Unavailable),
        }
    }
    fn shutdown(self: &Arc<Self>) -> LoopFuture<'static, ()> {
        self.accepting.store(false, Ordering::SeqCst);
        self.stop.send_replace(true);
        let mut done = self.done.subscribe();
        Box::pin(async move {
            loop {
                if let Some(result) = done.borrow().clone() {
                    return result;
                }
                done.changed().await.map_err(|_| LoopError::Unavailable)?;
            }
        })
    }
    async fn close(self: &Arc<Self>) -> PluginResult<()> {
        let result = self.shutdown().await;
        let task = self
            .task
            .lock()
            .map_err(|_| PluginError::Task("认知任务锁不可用".into()))?
            .take();
        if let Some(task) = task {
            task.await
                .map_err(|_| PluginError::Task("认知后台任务异常".into()))?;
        }
        result.map_err(|error| PluginError::Task(error.to_string()))
    }
}
struct CompletionGuard(Arc<Running>);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.accepting.store(false, Ordering::SeqCst);
        self.0.update(|stats| stats.active = false);
        let unfinished = self.0.done.borrow().is_none();
        if unfinished {
            self.0.done.send_replace(Some(Err(LoopError::Unavailable)));
        }
    }
}
#[derive(Clone, Default)]
pub struct LoopController {
    active: Arc<Mutex<Option<Arc<Running>>>>,
}
impl LoopController {
    fn running(&self) -> LoopResult<Arc<Running>> {
        self.active
            .lock()
            .map_err(|_| LoopError::Unavailable)?
            .clone()
            .ok_or(LoopError::Unavailable)
    }
}
impl LoopStatus for LoopController {
    fn stats(&self) -> LoopResult<LoopStats> {
        let running = self.running()?;
        let stats = *running.stats.lock().map_err(|_| LoopError::Unavailable)?;
        Ok(stats)
    }
}
impl LoopControl for LoopController {
    fn wake(&self, reason: WakeReason) -> LoopResult<bool> {
        self.running()?.signal(reason)
    }
    fn cancel_current(&self) -> LoopResult<bool> {
        let running = self.running()?;
        if !running.accepting.load(Ordering::SeqCst) {
            return Err(LoopError::Unavailable);
        }
        if !running
            .stats
            .lock()
            .map_err(|_| LoopError::Unavailable)?
            .active
        {
            return Ok(false);
        }
        let mut overflow = false;
        running.cancel.send_modify(|value| {
            if let Some(next) = value.checked_add(1) {
                *value = next;
            } else {
                overflow = true;
            }
        });
        if overflow {
            return Err(LoopError::LimitReached);
        }
        Ok(true)
    }
    fn shutdown(&self) -> LoopFuture<'static, ()> {
        match self.running() {
            Ok(running) => running.shutdown(),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}
struct StatusReader(Arc<Running>);
impl LoopStatus for StatusReader {
    fn stats(&self) -> LoopResult<LoopStats> {
        if !self.0.accepting.load(Ordering::SeqCst) {
            return Err(LoopError::Unavailable);
        }
        self.0
            .stats
            .lock()
            .map(|stats| *stats)
            .map_err(|_| LoopError::Unavailable)
    }
}
struct Worker {
    admin: Arc<dyn CognitionAdmin>,
    policy: Arc<dyn DrivePolicy>,
    verifier: Arc<dyn GoalVerifier>,
    executor: Arc<dyn GoalExecutor>,
    options: LoopOptions,
    context: PluginContext,
    running: Arc<Running>,
}
impl Worker {
    fn warn(&self, message: &'static str) {
        if self
            .context
            .log(LogEntry::new(LogLevel::Warn, "cognition.loop", message).expect("固定日志有效"))
            .is_err()
        {
            eprintln!("认知循环诊断投递失败");
        }
    }
    async fn run(self, mut wake: mpsc::Receiver<(WakeReason, Instant)>) -> LoopResult<()> {
        let mut stop = self.running.stop.subscribe();
        let mut timer = tokio::time::interval(Duration::from_millis(self.options.poll_interval_ms));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            let evaluated = tokio::select! {
                biased;
                _ = stop.changed() => continue,
                signal = wake.recv() => signal.ok_or(LoopError::Unavailable)?.1,
                _ = timer.tick() => Instant::now(),
            };
            if *stop.borrow() {
                return Ok(());
            }
            self.running
                .update(|stats| stats.evaluations = stats.evaluations.saturating_add(1));
            let snapshot = self.admin.snapshot()?;
            if snapshot.subject_id != self.options.scope.subject_id {
                return Err(CognitionError::SubjectMismatch.into());
            }
            let budget_started = Instant::now();
            let epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| LoopError::Unavailable)?;
            let time = u64::try_from(epoch.as_millis()).map_err(|_| LoopError::LimitReached)?;
            let submitted = self
                .running
                .stats
                .lock()
                .map_err(|_| LoopError::Unavailable)?
                .submitted;
            let evaluation = evaluate_agenda(
                &snapshot,
                &self.options,
                self.policy.as_ref(),
                self.verifier.as_ref(),
                submitted,
                time,
            )?;
            if evaluation.blocker.is_some() || evaluation.ranked.is_empty() {
                self.running
                    .update(|stats| stats.idle_ticks = stats.idle_ticks.saturating_add(1));
                continue;
            }
            let ranked = evaluation.ranked;
            let selected = &ranked[0].goal_id;
            let mut state = snapshot.state;
            state
                .drives
                .retain(|id, _| !id.starts_with("eve.loop.drive."));
            let drive_capacity = MAX_RECORDS.saturating_sub(state.drives.len());
            if ranked.len() > drive_capacity {
                self.warn("驱动记录容量不足；保留宿主驱动，按议程推进最高优先级目标");
            }
            for (index, item) in ranked.iter().take(drive_capacity).enumerate() {
                let id = format!("eve.loop.drive.{index}");
                state.drives.insert(
                    id.clone(),
                    Drive {
                        id,
                        visibility: Visibility::Internal,
                        goal_ids: vec![item.goal_id.clone()],
                        strength: item.strength,
                        reason: item.reason.clone(),
                        evaluated_at_ms: time,
                        valid_until_ms: time.saturating_add(self.options.poll_interval_ms),
                    },
                );
            }
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|_| LoopError::Unavailable)?;
            let nonce = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let attempt = ExecutionAttempt {
                attempt_id: format!("attempt-{nonce}"),
                session_id: format!("internal-{nonce}"),
                task_id: format!("goal-{nonce}"),
                turn_id: None,
                started_at_ms: time,
            };
            let mut goal = state.goals[selected].clone();
            let timeout = Duration::from_millis(goal.budget.timeout_ms);
            let remaining = goal.expires_at_ms.map_or(timeout, |expires| {
                timeout.min(Duration::from_millis(expires).saturating_sub(epoch))
            });
            let deadline = budget_started + remaining;
            goal.status = GoalStatus::Executing;
            goal.execution = Some(attempt.clone());
            state.goals.insert(selected.clone(), goal.clone());
            state.agenda = Some(Agenda {
                visibility: Visibility::Internal,
                candidates: ranked
                    .iter()
                    .skip(1)
                    .map(|item| item.goal_id.clone())
                    .collect(),
                selected: Some(selected.clone()),
                reason: ranked[0].reason.clone(),
                valid_until_ms: time.saturating_add(self.options.poll_interval_ms),
            });
            let cancellation = *self.running.cancel.borrow();
            self.running.update(|stats| stats.active = true);
            match self.admin.replace(snapshot.revision, state) {
                Ok(_) => {}
                Err(CognitionError::StaleRevision | CognitionError::Busy) => {
                    self.running.update(|stats| stats.active = false);
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            let result = self
                .execute(&goal, &attempt, cancellation, evaluated, deadline)
                .await;
            self.running.update(|stats| stats.active = false);
            result?;
        }
    }
    async fn execute(
        &self,
        goal: &Goal,
        attempt: &ExecutionAttempt,
        cancellation: u64,
        evaluated: Instant,
        deadline: Instant,
    ) -> LoopResult<()> {
        if *self.running.stop.borrow()
            || *self.running.cancel.borrow() != cancellation
            || Instant::now() >= deadline
        {
            return self.finish(goal, attempt, None, true, true).await;
        }
        let key = match self.executor.submit_before(goal, attempt, deadline) {
            Ok(key) => key,
            Err(_) => return self.finish(goal, attempt, None, false, false).await,
        };
        self.running.update(|stats| {
            stats.submitted = stats.submitted.saturating_add(1);
            stats.last_wakeup_to_admission_us =
                evaluated.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        });
        let mut wait = self.executor.wait(&key);
        let mut stop = self.running.stop.subscribe();
        let mut cancel = self.running.cancel.subscribe();
        let requested = async {
            loop {
                if *stop.borrow() || *cancel.borrow() != cancellation {
                    return;
                }
                tokio::select! {
                    _ = stop.changed() => {},
                    _ = cancel.changed() => {},
                }
            }
        };
        let (report, cancelled) = tokio::select! {
            biased;
            report = &mut wait => (report, false),
            _ = requested => {
                let started = Instant::now();
                let cancel_result = self.executor.cancel(&key);
                let report = wait.await;
                self.running.update(|stats| stats.last_cancel_settle_us =
                    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
                if cancel_result.is_err() { self.warn("取消准入未确认；以实际收尾报告为准"); }
                (report, true)
            },
            _ = tokio::time::sleep_until(deadline.into()) => {
                let started = Instant::now();
                let cancel_result = self.executor.cancel(&key);
                let report = wait.await;
                self.running.update(|stats| stats.last_cancel_settle_us =
                    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
                if cancel_result.is_err() { self.warn("期限取消未确认；以实际收尾报告为准"); }
                (report, true)
            },
        };
        let report = report.ok().filter(|report| {
            report.control.key == key
                && key.session.session_id == attempt.session_id
                && key.task_id == attempt.task_id
        });
        self.finish(goal, attempt, report, cancelled, false).await
    }
    async fn finish(
        &self,
        goal: &Goal,
        attempt: &ExecutionAttempt,
        report: Option<GoalExecutionReport>,
        cancelled: bool,
        not_started: bool,
    ) -> LoopResult<()> {
        let cancelled = cancelled
            || report
                .as_ref()
                .is_some_and(|report| report.control.cancel_requested);
        let usage_valid = not_started
            || report.as_ref().is_some_and(|report| {
                report.usage.model_requests <= u64::from(goal.budget.max_model_requests)
                    && report.usage.admitted_tool_calls <= u64::from(goal.budget.max_tool_calls)
                    && report
                        .control
                        .run
                        .started_tools
                        .is_some_and(|started| started <= report.usage.admitted_tool_calls)
            });
        let verified = if usage_valid {
            report.as_ref().is_some_and(|report| {
                report.control.run.commit == CommitState::Completed
                    && report.control.run.turn_id.is_some()
                    && self.verifier.verify(goal, report).unwrap_or(false)
            })
        } else {
            false
        };
        let commit = report.as_ref().map_or(
            if not_started {
                ExecutionCommit::NotStarted
            } else {
                ExecutionCommit::Unknown
            },
            |report| match report.control.run.commit {
                CommitState::NotStarted => ExecutionCommit::NotStarted,
                CommitState::Completed => ExecutionCommit::Completed,
                CommitState::Failed => ExecutionCommit::Failed,
                CommitState::Pending => ExecutionCommit::Pending,
                CommitState::Unknown => ExecutionCommit::Unknown,
            },
        );
        let (status, reason) = if verified {
            (GoalStatus::Completed, None)
        } else if matches!(commit, ExecutionCommit::Pending | ExecutionCommit::Unknown)
            || !usage_valid
        {
            (GoalStatus::Blocked, Some(BlockReason::UnknownCommit))
        } else if cancelled {
            (GoalStatus::Cancelled, None)
        } else {
            (GoalStatus::Blocked, Some(BlockReason::Invalidated))
        };
        let feedback = Feedback {
            commit,
            verification_met: verified,
            started_tools: if not_started {
                Some(0)
            } else {
                report
                    .as_ref()
                    .and_then(|report| report.control.run.started_tools)
            },
            summary: report.as_ref().map_or_else(
                || {
                    if not_started {
                        "未准入执行；模型和工具均未启动"
                    } else {
                        "执行报告不确定；不能推断零副作用"
                    }
                    .into()
                },
                |report| {
                    format!(
                        "验证结果 {}；模型准入 {}，工具准入 {}",
                        verified, report.usage.model_requests, report.usage.admitted_tool_calls
                    )
                },
            ),

            at_ms: now_ms()?,
        };
        let turn_id = report
            .as_ref()
            .and_then(|report| report.control.run.turn_id);
        // 执行已确认的计数独立于反馈提交；保存失败也不能伪装成零副作用。
        if let Some(report) = &report {
            self.running.update(|stats| {
                stats.model_requests = stats
                    .model_requests
                    .saturating_add(report.usage.model_requests);
                stats.admitted_tool_calls = stats
                    .admitted_tool_calls
                    .saturating_add(report.usage.admitted_tool_calls);
                stats.started_tools = stats
                    .started_tools
                    .saturating_add(report.control.run.started_tools.unwrap_or(0));
            });
        }
        let saved = self.save_feedback(goal, attempt, status.clone(), reason, feedback, turn_id);
        let snapshot = match saved {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.running.update(|stats| {
                    stats.feedback_save_failures = stats.feedback_save_failures.saturating_add(1)
                });
                self.warn("认知反馈保存失败；停止循环，不重放工具");
                // 尽力落盘阻塞；再次失败仍保留 Executing，下一次恢复会阻塞。
                let _ = self.save_feedback(
                    goal,
                    attempt,
                    GoalStatus::Blocked,
                    Some(BlockReason::FeedbackSaveFailed),
                    Feedback {
                        commit: ExecutionCommit::Unknown,
                        verification_met: false,
                        started_tools: report
                            .as_ref()
                            .and_then(|report| report.control.run.started_tools),
                        summary: "真实执行反馈未能可靠保存".into(),
                        at_ms: now_ms()?,
                    },
                    turn_id,
                );
                return Err(error);
            }
        };
        self.running
            .update(|stats| match snapshot.state.goals[&goal.id].status {
                GoalStatus::Completed => stats.completed = stats.completed.saturating_add(1),
                GoalStatus::Cancelled => stats.cancelled = stats.cancelled.saturating_add(1),
                GoalStatus::Blocked => stats.blocked = stats.blocked.saturating_add(1),
                _ => {}
            });
        // 通知只有无正文的全局修订，不能据此读取私有目标；不订阅此事件作唤醒。
        if self
            .context
            .emit(
                Event::new(
                    LOOP_FEEDBACK_EVENT_ID,
                    snapshot.revision.to_string().into_bytes(),
                )
                .map_err(|_| LoopError::Unavailable)?,
            )
            .is_err()
        {
            self.running.update(|stats| {
                stats.notification_failures = stats.notification_failures.saturating_add(1)
            });
            self.warn("认知终态已保存，通知投递失败");
        }
        Ok(())
    }
    fn save_feedback(
        &self,
        goal: &Goal,
        attempt: &ExecutionAttempt,
        status: GoalStatus,
        reason: Option<BlockReason>,
        feedback: Feedback,
        turn_id: Option<u64>,
    ) -> LoopResult<CognitiveSnapshot> {
        for _ in 0..4 {
            let snapshot = self.admin.snapshot()?;
            let mut state = snapshot.state.clone();
            let current = state
                .goals
                .get_mut(&goal.id)
                .ok_or(LoopError::InvalidInput)?;
            if current.status != GoalStatus::Executing
                || current
                    .execution
                    .as_ref()
                    .is_none_or(|execution| execution.attempt_id != attempt.attempt_id)
            {
                return Err(CognitionError::StaleRevision.into());
            }
            current.status = status.clone();
            current.block_reason = reason.clone();
            current.feedback = Some(feedback.clone());
            current.execution.as_mut().expect("执行关联已检查").turn_id = turn_id;
            if let Some(agenda) = &mut state.agenda {
                agenda
                    .candidates
                    .retain(|id| state.goals[id].status == GoalStatus::Ready);
                if agenda.selected.as_deref() == Some(goal.id.as_str()) {
                    agenda.selected = None;
                }
            }
            match self.admin.replace(snapshot.revision, state) {
                Ok(saved) => return Ok(saved),
                Err(CognitionError::StaleRevision) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(CognitionError::StaleRevision.into())
    }
}
pub struct CognitionLoopPlugin {
    manifest: PluginManifest,
    admin: Arc<dyn CognitionAdmin>,
    policy: Arc<dyn DrivePolicy>,
    verifier: Arc<dyn GoalVerifier>,
    executor: Arc<dyn GoalExecutor>,
    options: LoopOptions,
    controller: LoopController,
}
impl CognitionLoopPlugin {
    pub fn new(
        admin: Arc<dyn CognitionAdmin>,
        policy: Arc<dyn DrivePolicy>,
        verifier: Arc<dyn GoalVerifier>,
        executor: Arc<dyn GoalExecutor>,
        options: LoopOptions,
        dependencies: Vec<PluginDependency>,
    ) -> PluginResult<Self> {
        options
            .validate()
            .map_err(|error| PluginError::State(error.to_string()))?;
        let mut manifest = PluginManifest::new(LOOP_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?;
        manifest.dependencies = dependencies;
        Ok(Self {
            manifest,
            admin,
            policy,
            verifier,
            executor,
            options,
            controller: LoopController::default(),
        })
    }
    pub fn controller(&self) -> LoopController {
        self.controller.clone()
    }
}
impl Plugin for CognitionLoopPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| PluginError::Task("认知循环需要异步 Runtime".into()))?;
            let (wake, receiver) = mpsc::channel(1);
            let (stop, _) = watch::channel(false);
            let (cancel, _) = watch::channel(0);
            let (done, _) = watch::channel(None);
            let running = Arc::new(Running {
                wake,
                stop,
                cancel,
                done,
                accepting: AtomicBool::new(true),
                stats: Mutex::new(LoopStats::default()),
                task: Mutex::new(None),
            });
            // 任何启动步骤失败都发布完成信号，回滚不会等待未创建的任务。
            let guard = CompletionGuard(running.clone());
            let closing = running.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                let result = closing.close().await;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("认知循环句柄锁不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &closing))
                {
                    *active = None;
                }
                result
            }))?;
            let listener = running.clone();
            context.on(
                EventId::new(LOOP_WAKE_EVENT_ID)?,
                Arc::new(move |_| match listener.signal(WakeReason::StateChanged) {
                    Ok(_) | Err(LoopError::Unavailable) => Ok(()),
                    Err(error) => Err(PluginError::Task(error.to_string())),
                }),
            )?;
            context.provide_service(
                ServiceId::new(LOOP_STATUS_SERVICE_ID)?,
                LoopStatusHandle(Arc::new(StatusReader(running.clone()))),
            )?;
            let worker = Worker {
                admin: self.admin.clone(),
                policy: self.policy.clone(),
                verifier: self.verifier.clone(),
                executor: self.executor.clone(),
                options: self.options.clone(),
                context: context.clone(),
                running: running.clone(),
            };
            let completion = running.clone();
            let task = runtime.spawn(async move {
                let _guard = guard;
                let result = worker.run(receiver).await;
                completion.done.send_replace(Some(result));
            });
            *running
                .task
                .lock()
                .map_err(|_| PluginError::Task("认知任务锁不可用".into()))? = Some(task);
            running
                .signal(WakeReason::Startup)
                .map_err(|error| PluginError::Task(error.to_string()))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("认知句柄锁不可用".into()))? = Some(running);
            Ok(None)
        })
    }
}
