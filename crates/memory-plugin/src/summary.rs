//! 可替换的语义归纳实现及持久账本；保存准入后才允许模型调用。
mod model;
mod store;
mod validation;
use eve_memory_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
pub use model::ModelConversationSummarizer;
use std::sync::{Arc, Mutex};
use store::StoredSummaries;
pub use validation::{effective_points, validate_summary_points};

pub const SUMMARY_STATE_KEY: &str = "summaries.v1";
#[derive(Clone, Default)]
pub struct SummaryController {
    active: Arc<Mutex<Option<Arc<StoredSummaries>>>>,
}
impl SummaryController {
    fn service(&self) -> MemoryResult<Arc<StoredSummaries>> {
        self.active
            .lock()
            .map_err(|_| MemoryError::Unavailable)?
            .clone()
            .ok_or(MemoryError::Unavailable)
    }
}
impl SummaryAdmin for SummaryController {
    fn last_provider_failure_at(&self) -> MemoryResult<Option<u64>> {
        self.service()?.last_provider_failure_at()
    }
    fn jobs(&self, scope: &MemoryScope) -> MemoryResult<Vec<SummaryJob>> {
        self.service()?.jobs(scope)
    }
    fn reserve(
        &self,
        snapshot: &MemorySnapshot,
        now: u64,
        version: &str,
        options: SummaryOptions,
    ) -> MemoryResult<Option<SummaryBatch>> {
        self.service()?.reserve(snapshot, now, version, options)
    }
    fn finish(
        &self,
        batch: &SummaryBatch,
        now: u64,
        result: Result<Vec<SummaryPoint>, SummaryFailure>,
    ) -> MemoryResult<()> {
        self.service()?.finish(batch, now, result)
    }
}
pub struct ConversationSummaryPlugin {
    manifest: PluginManifest,
    controller: SummaryController,
}
impl ConversationSummaryPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(SUMMARY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: SummaryController::default(),
        })
    }
    pub fn controller(&self) -> SummaryController {
        self.controller.clone()
    }
}
impl Plugin for ConversationSummaryPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredSummaries::open(context.clone())
                    .map_err(|e| PluginError::State(e.to_string()))?,
            );
            let closing = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                closing.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("语义摘要句柄不可用".into()))?;
                if active.as_ref().is_some_and(|v| Arc::ptr_eq(v, &closing)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("语义摘要句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
