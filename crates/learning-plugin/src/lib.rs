//! 低频偏好候选、单次模型提炼与严格恢复。
mod extractor;
mod store;
mod strict_json;

use eve_learning_api::*;
use eve_memory_api::{MemoryScope, MemorySnapshot};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
pub use extractor::ModelPreferenceExtractor;
use std::sync::{Arc, Mutex};
use store::StoredLearning;
pub use store::validate_drafts;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct LearningController {
    active: Arc<Mutex<Option<Arc<StoredLearning>>>>,
}
impl LearningController {
    fn service(&self) -> LearningResult<Arc<StoredLearning>> {
        self.active
            .lock()
            .map_err(|_| LearningError::Unavailable)?
            .clone()
            .ok_or(LearningError::Unavailable)
    }
}
impl LearningAdmin for LearningController {
    fn snapshot(&self, scope: &MemoryScope) -> LearningResult<LearningSnapshot> {
        self.service()?.snapshot(scope)
    }
    fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        extractor_version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        self.service()?
            .reserve(memory, now_ms, extractor_version, options)
    }
    fn finish(
        &self,
        batch: &LearningBatch,
        at_ms: u64,
        outcome: LearningOutcome,
    ) -> LearningResult<()> {
        self.service()?.finish(batch, at_ms, outcome)
    }
}

pub struct LearningPlugin {
    manifest: PluginManifest,
    controller: LearningController,
}
impl LearningPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(LEARNING_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: LearningController::default(),
        })
    }
    pub fn controller(&self) -> LearningController {
        self.controller.clone()
    }
}
impl Plugin for LearningPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredLearning::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("偏好提炼管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("偏好提炼管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
