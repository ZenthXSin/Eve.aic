//! 后台任务账本。
mod store;
mod strategy;
mod strategy_policy;
mod strict_json;
pub use strategy::{
    TaskStrategyController, TaskStrategyPlugin, evaluate_task_effect, extract_task_strategy,
    task_strategy_views,
};
pub use strategy_policy::PairedTaskStrategyPolicy;

use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_task_api::*;
use std::sync::{Arc, Mutex};
use store::StoredTasks;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct TaskController {
    active: Arc<Mutex<Option<Arc<StoredTasks>>>>,
    strategies: Option<Arc<dyn TaskStrategyLearning>>,
}
impl TaskController {
    fn service(&self) -> TaskResult<Arc<StoredTasks>> {
        self.active
            .lock()
            .map_err(|_| TaskError::Unavailable)?
            .clone()
            .ok_or(TaskError::Unavailable)
    }
}
impl TaskAdmin for TaskController {
    fn snapshot(&self) -> TaskResult<TaskSnapshot> {
        self.service()?.snapshot()
    }
    fn summaries_for(&self, owner: &TaskOwner) -> TaskResult<Vec<TaskSummary>> {
        self.service()?.summaries_for(owner)
    }
    fn get_owned(&self, owner: &TaskOwner, id: &str) -> TaskResult<Option<Task>> {
        self.service()?.get_owned(owner, id)
    }
    fn strategy_guidance(&self, task: &Task) -> TaskResult<Option<TaskStrategyGuidance>> {
        self.strategies
            .as_ref()
            .map_or(Ok(None), |strategies| strategies.prepare(task))
    }
    fn reconcile_strategies(&self) -> TaskResult<()> {
        if let Some(strategies) = &self.strategies {
            strategies.reconcile(&self.snapshot()?)?;
        }
        Ok(())
    }
    fn create(
        &self,
        owner: &TaskOwner,
        spec: TaskSpec,
        source: &str,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.service()?.create(owner, spec, source, now_ms)
    }
    fn create_bounded(
        &self,
        owner: &TaskOwner,
        spec: TaskSpec,
        source: &str,
        deadline_at_ms: u64,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.service()?
            .create_with_deadline(owner, spec, source, Some(deadline_at_ms), now_ms)
    }
    fn start(&self, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.service()?.start(id, now_ms)
    }
    fn checkpoint(
        &self,
        id: &str,
        checkpoint: eve_llm_api::TurnCheckpoint,
        deadline_at_ms: u64,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.service()?
            .checkpoint(id, checkpoint, deadline_at_ms, now_ms)
    }
    fn retry(&self, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.service()?.retry(id, now_ms)
    }
    fn resume_blocked(
        &self,
        owner: &TaskOwner,
        id: &str,
        evidence: &str,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.service()?.resume_blocked(owner, id, evidence, now_ms)
    }
    fn finish(&self, id: &str, end: TaskEnd, usage: TaskUsage, now_ms: u64) -> TaskResult<Task> {
        self.service()?.finish(id, end, usage, now_ms)
    }
    fn cancel(&self, user_id: &str, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.service()?.cancel(user_id, id, now_ms)
    }
    fn claim(&self, id: &str, channel: DeliveryChannel, now_ms: u64) -> TaskResult<Task> {
        self.service()?.claim(id, channel, now_ms)
    }
    fn record_delivery(&self, id: &str, result: DeliveryResult, now_ms: u64) -> TaskResult<Task> {
        self.service()?.record_delivery(id, result, now_ms)
    }
    fn expire(&self, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.service()?.expire(id, now_ms)
    }
}

pub struct TaskPlugin {
    manifest: PluginManifest,
    controller: TaskController,
}
impl TaskPlugin {
    /// 只有可信宿主装配评价策略；未装配时保持原有执行路径。
    pub fn with_strategy_learning(mut self, strategies: Arc<dyn TaskStrategyLearning>) -> Self {
        self.controller.strategies = Some(strategies);
        self
    }
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(TASK_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: TaskController::default(),
        })
    }
    pub fn controller(&self) -> TaskController {
        self.controller.clone()
    }
}
impl Plugin for TaskPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredTasks::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("后台任务管理句柄不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("后台任务管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
