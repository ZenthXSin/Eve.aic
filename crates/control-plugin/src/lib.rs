//! 内置任务控制插件；持有执行 Future 和取消信号，不调用模型或管理会话磁盘格式。
use eve_control_api::*;
use eve_llm_api::{LlmError, LlmFuture, TurnEvent, TurnEventKind, TurnEventSink};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginManifest,
    PluginResult, ServiceId, cleanup,
};
use eve_session_api::SessionKey;
use std::{
    collections::BTreeMap,
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::{sync::watch, task::JoinHandle};

struct Generation {
    key: GenerationKey,
    state: Mutex<ControlSnapshot>,
    cancel: watch::Sender<bool>,
    done: watch::Sender<Option<ControlReport>>,
    task: Mutex<Option<JoinHandle<()>>>,
}
struct Registry {
    active: bool,
    next: u64,
    sessions: BTreeMap<String, Arc<Generation>>,
}
struct Controller {
    epoch: [u8; 16],
    runner: Arc<dyn ControlRunner>,
    registry: Mutex<Registry>,
}
impl Controller {
    fn open(runner: Arc<dyn ControlRunner>) -> PluginResult<Self> {
        let mut epoch = [0; 16];
        getrandom::fill(&mut epoch)
            .map_err(|_| PluginError::State("无法生成控制器启动标识".into()))?;
        Ok(Self {
            epoch,
            runner,
            registry: Mutex::new(Registry {
                active: true,
                next: 1,
                sessions: BTreeMap::new(),
            }),
        })
    }
    fn lock(&self) -> ControlResult<MutexGuard<'_, Registry>> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| ControlError::Unavailable)?;
        if !registry.active {
            return Err(ControlError::Unavailable);
        }
        Ok(registry)
    }
    fn generation(&self, key: &GenerationKey) -> ControlResult<Arc<Generation>> {
        let registry = self.lock()?;
        let generation = registry
            .sessions
            .get(&key.session.session_id)
            .ok_or(ControlError::StaleGeneration)?;
        if generation.key.session != key.session {
            return Err(ControlError::OwnerMismatch);
        }
        if generation.key != *key {
            return Err(ControlError::StaleGeneration);
        }
        Ok(generation.clone())
    }
    async fn close(&self) -> PluginResult<()> {
        let generations = {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| PluginError::State("控制器锁不可用".into()))?;
            registry.active = false;
            registry.sessions.values().cloned().collect::<Vec<_>>()
        };
        for generation in &generations {
            request_cancel(generation).map_err(|error| PluginError::State(error.to_string()))?;
        }
        for generation in generations {
            let task = generation
                .task
                .lock()
                .map_err(|_| PluginError::State("控制器任务锁不可用".into()))?
                .take();
            if let Some(task) = task {
                task.await
                    .map_err(|_| PluginError::Task("控制器收尾异常".into()))?;
            }
        }
        Ok(())
    }
}
fn request_cancel(generation: &Generation) -> ControlResult<CancelDisposition> {
    let mut state = generation
        .state
        .lock()
        .map_err(|_| ControlError::Unavailable)?;
    state.events_retired = true;
    if state.report.is_some() {
        return Ok(CancelDisposition::AlreadyFinished);
    }
    if state.cancel_requested {
        return Ok(CancelDisposition::AlreadyRequested);
    }
    state.cancel_requested = true;
    state.phase = ControlPhase::Cancelling;
    generation.cancel.send_replace(true);
    Ok(CancelDisposition::Requested)
}
impl Controller {
    fn submit_inner(
        &self,
        expected: Option<&GenerationKey>,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        input.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ControlError::Unavailable)?;
        let mut registry = self.lock()?;
        if let Some(expected) = expected {
            if expected.session != input.session.key {
                return Err(ControlError::OwnerMismatch);
            }
            let old = registry
                .sessions
                .get(&expected.session.session_id)
                .ok_or(ControlError::StaleGeneration)?;
            if old.key != *expected {
                return Err(ControlError::StaleGeneration);
            }
        }
        if let Some(old) = registry.sessions.get(&input.session.key.session_id) {
            let state = old.state.lock().map_err(|_| ControlError::Unavailable)?;
            let never_started = state
                .report
                .as_ref()
                .is_some_and(|report| report.run.commit == CommitState::NotStarted);
            if old.key.session != input.session.key && !never_started {
                return Err(ControlError::OwnerMismatch);
            }
            if state.phase == ControlPhase::Blocked {
                return Err(ControlError::Blocked);
            }
            if state.report.is_none() {
                return Err(ControlError::Busy);
            }
        }
        let next = registry
            .next
            .checked_add(1)
            .ok_or(ControlError::LimitReached)?;
        let key = GenerationKey {
            session: input.session.key.clone(),
            task_id: input.task_id,
            controller_epoch: self.epoch,
            generation: registry.next,
        };
        let (cancel, _) = watch::channel(false);
        let (done, _) = watch::channel(None);
        let generation = Arc::new(Generation {
            key: key.clone(),
            state: Mutex::new(ControlSnapshot {
                key: key.clone(),
                input_text: input.session.text.clone(),
                phase: ControlPhase::Starting,
                cancel_requested: false,
                events_retired: false,
                turn_id: None,
                report: None,
            }),
            cancel,
            done,
            task: Mutex::new(None),
        });
        let runner = self.runner.clone();
        let running = generation.clone();
        let task = runtime.spawn(async move {
            let events = GenerationSink {
                generation: running.clone(),
                sink,
            };
            let run = contain_panic(async { runner.run(input.session, &events).await }).await;
            let run = run.unwrap_or(RunReport {
                turn_id: None,
                commit: CommitState::Unknown,
                text: None,
                transcript: None,
                started_tools: None,
                tool_results: vec![],
                failure: Some(RunFailure::RunnerPanicked),
            });
            let mut state = running
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let report = ControlReport {
                key: running.key.clone(),
                cancel_requested: state.cancel_requested,
                run,
            };
            state.turn_id = report.run.turn_id.or(state.turn_id);
            state.phase = match report.run.commit {
                CommitState::Pending | CommitState::Unknown => ControlPhase::Blocked,
                _ => ControlPhase::Finished,
            };
            state.report = Some(report.clone());
            running.cancel.send_replace(true);
            running.done.send_replace(Some(report));
        });
        *generation
            .task
            .lock()
            .map_err(|_| ControlError::Unavailable)? = Some(task);
        registry.next = next;
        registry
            .sessions
            .insert(key.session.session_id.clone(), generation);
        Ok(key)
    }
}
impl ControlService for Controller {
    fn submit(
        &self,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.submit_inner(None, input, sink)
    }
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.submit_inner(Some(expected), input, sink)
    }
    fn cancel(&self, key: &GenerationKey) -> ControlResult<CancelDisposition> {
        let registry = self.lock()?;
        let generation = registry
            .sessions
            .get(&key.session.session_id)
            .ok_or(ControlError::StaleGeneration)?;
        if generation.key.session != key.session {
            return Err(ControlError::OwnerMismatch);
        }
        if generation.key != *key {
            return Err(ControlError::StaleGeneration);
        }
        request_cancel(generation)
    }
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        let generation = self.generation(key);
        Box::pin(async move {
            let generation = generation?;
            let mut done = generation.done.subscribe();
            loop {
                let report = done.borrow_and_update().clone();
                if let Some(report) = report {
                    return Ok(report);
                }
                done.changed()
                    .await
                    .map_err(|_| ControlError::Unavailable)?;
            }
        })
    }
    fn snapshot(&self, key: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        key.validate().map_err(|_| ControlError::InvalidInput)?;
        let registry = self.lock()?;
        let Some(generation) = registry.sessions.get(&key.session_id) else {
            return Ok(None);
        };
        if generation.key.session != *key {
            return Err(ControlError::OwnerMismatch);
        }
        generation
            .state
            .lock()
            .map(|state| Some(state.clone()))
            .map_err(|_| ControlError::Unavailable)
    }
    fn accepts(&self, event: &ControlEvent) -> bool {
        let Ok(registry) = self.lock() else {
            return false;
        };
        let Some(generation) = registry.sessions.get(&event.key.session.session_id) else {
            return false;
        };
        generation.key == event.key
            && generation.state.lock().is_ok_and(|state| {
                !state.events_retired
                    && state.phase != ControlPhase::Blocked
                    && state.turn_id == event.event.turn_id
            })
    }
}
struct GenerationSink {
    generation: Arc<Generation>,
    sink: Arc<dyn ControlEventSink>,
}
impl TurnEventSink for GenerationSink {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            {
                let mut state = self
                    .generation
                    .state
                    .lock()
                    .map_err(|_| LlmError::Backend("控制器状态不可用".into()))?;
                if state.cancel_requested || state.report.is_some() {
                    return Err(LlmError::Cancelled);
                }
                if let Some(old) = state.turn_id
                    && event.turn_id != Some(old)
                {
                    return Err(LlmError::Protocol("执行器轮次标识改变".into()));
                }
                state.turn_id = event.turn_id.or(state.turn_id);
                state.phase = match &event.kind {
                    TurnEventKind::ProviderStarted { .. }
                    | TurnEventKind::TextDelta { .. }
                    | TurnEventKind::ResponseCompleted { .. } => ControlPhase::Generating,
                    TurnEventKind::ToolBatchStarted { .. } | TurnEventKind::ToolResult { .. } => {
                        ControlPhase::Tools
                    }
                    TurnEventKind::TurnCompleted { .. }
                    | TurnEventKind::SessionSaved
                    | TurnEventKind::Failed { .. } => ControlPhase::Committing,
                };
            }
            tokio::select! {
                biased;
                _ = self.closed() => Err(LlmError::Cancelled),
                result = self.sink.emit(ControlEvent { key: self.generation.key.clone(), event }) => result,
            }
        })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        let mut cancel = self.generation.cancel.subscribe();
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = async {
                    loop {
                        if *cancel.borrow_and_update() { break; }
                        if cancel.changed().await.is_err() { break; }
                    }
                } => Ok(()),
                result = self.sink.closed() => result,
            }
        })
    }
}
async fn contain_panic<F: Future>(future: F) -> Result<F::Output, ()> {
    let mut future = Box::pin(future);
    poll_fn(
        |context| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(std::task::Poll::Ready(value)) => std::task::Poll::Ready(Ok(value)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        },
    )
    .await
}
pub struct ControlPlugin {
    manifest: PluginManifest,
    runner: Arc<dyn ControlRunner>,
}
impl ControlPlugin {
    /// 组合层显式声明执行器需要的插件依赖，控制实现不耦合某一种执行器。
    pub fn new(
        runner: Arc<dyn ControlRunner>,
        dependencies: Vec<PluginDependency>,
    ) -> PluginResult<Self> {
        let mut manifest = PluginManifest::new(CONTROL_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?;
        manifest.dependencies = dependencies;
        Ok(Self { manifest, runner })
    }
}
impl Plugin for ControlPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let runner = self.runner.clone();
        Box::pin(async move {
            let controller = Arc::new(Controller::open(runner)?);
            let to_close = controller.clone();
            context.cleanup(cleanup(move || async move { to_close.close().await }))?;
            context.provide_service(
                ServiceId::new(CONTROL_SERVICE_ID)?,
                ControlServiceHandle(controller),
            )?;
            Ok(None)
        })
    }
}
