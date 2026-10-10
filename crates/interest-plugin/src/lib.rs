//! 兴趣观察账本、单次模型观察器与学习目标派生器。
mod goal;
mod observer;
mod settings;
mod store;
mod strict_json;

use eve_interest_api::*;
use eve_memory_api::{MemoryScope, MemorySnapshot};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
pub use goal::{DERIVER_VERSION, LearningGoalDeriver, learning_goal_id};
pub use observer::{ModelInterestObserver, OBSERVER_VERSION};
pub use settings::{InterestSettingsController, InterestSettingsPlugin};
use std::sync::{Arc, Mutex};
use store::StoredInterests;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct InterestController {
    active: Arc<Mutex<Option<Arc<StoredInterests>>>>,
}
impl InterestController {
    fn service(&self) -> InterestResult<Arc<StoredInterests>> {
        self.active
            .lock()
            .map_err(|_| InterestError::Unavailable)?
            .clone()
            .ok_or(InterestError::Unavailable)
    }
}
impl InterestAdmin for InterestController {
    fn scopes(&self) -> InterestResult<Vec<MemoryScope>> {
        self.service()?.scopes()
    }
    fn snapshot(&self, scope: &MemoryScope) -> InterestResult<InterestSnapshot> {
        self.service()?.snapshot(scope)
    }
    fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        observer_version: &str,
        options: &ObservationOptions,
    ) -> InterestResult<Option<ObservationBatch>> {
        self.service()?
            .reserve(memory, now_ms, observer_version, options)
    }
    fn finish(
        &self,
        batch: &ObservationBatch,
        at_ms: u64,
        outcome: ObservationOutcome,
    ) -> InterestResult<Vec<UpdateResult>> {
        self.service()?.finish(batch, at_ms, outcome)
    }
    fn withdraw(
        &self,
        scope: &MemoryScope,
        interest_id: &str,
        request: WithdrawalRequest,
    ) -> InterestResult<InterestRecord> {
        self.service()?.withdraw(scope, interest_id, request)
    }
}

pub struct InterestPlugin {
    manifest: PluginManifest,
    controller: InterestController,
}
impl InterestPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(INTEREST_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: InterestController::default(),
        })
    }
    pub fn controller(&self) -> InterestController {
        self.controller.clone()
    }
}
impl Plugin for InterestPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredInterests::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("兴趣观察管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("兴趣观察管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
