//! 领域知识账本、受控 HTTP 抓取器、单次模型来源选择与知识提炼，以及串起各阶段的研究流程。
mod fetch;
mod html;
mod model;
mod research;
mod store;
mod strict_json;

use eve_knowledge_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
pub use fetch::HttpSourceFetcher;
pub use model::{
    EXTRACTOR_VERSION, ModelKnowledgeExtractor, ModelSourceSelector, SELECTOR_VERSION,
};
pub use research::Researcher;
use std::sync::{Arc, Mutex};
use store::StoredKnowledge;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct KnowledgeController {
    active: Arc<Mutex<Option<Arc<StoredKnowledge>>>>,
}
impl KnowledgeController {
    fn service(&self) -> KnowledgeResult<Arc<StoredKnowledge>> {
        self.active
            .lock()
            .map_err(|_| KnowledgeError::Unavailable)?
            .clone()
            .ok_or(KnowledgeError::Unavailable)
    }
}
impl KnowledgeAdmin for KnowledgeController {
    fn snapshot(&self) -> KnowledgeResult<KnowledgeSnapshot> {
        self.service()?.snapshot()
    }
    fn begin(
        &self,
        topic: ResearchTopic,
        policy: &SourcePolicy,
        researcher_version: &str,
        now_ms: u64,
    ) -> KnowledgeResult<Option<ResearchRun>> {
        self.service()?
            .begin(topic, policy, researcher_version, now_ms)
    }
    fn advance(&self, run_id: &str, progress: ResearchProgress) -> KnowledgeResult<ResearchRun> {
        self.service()?.advance(run_id, progress)
    }
    fn finish(
        &self,
        run_id: &str,
        at_ms: u64,
        outcome: ResearchOutcome,
    ) -> KnowledgeResult<ResearchRun> {
        self.service()?.finish(run_id, at_ms, outcome)
    }
}

pub struct KnowledgePlugin {
    manifest: PluginManifest,
    controller: KnowledgeController,
}
impl KnowledgePlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(KNOWLEDGE_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: KnowledgeController::default(),
        })
    }
    pub fn controller(&self) -> KnowledgeController {
        self.controller.clone()
    }
}
impl Plugin for KnowledgePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredKnowledge::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("领域知识管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("领域知识管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
