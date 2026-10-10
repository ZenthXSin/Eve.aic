//! 语义记忆召回：持久向量索引、后台建索引，以及词项与语义融合的混合召回。
//! 向量模型通过 `eve-semantic-api` 的公开契约替换；索引只保存范围摘要、条目键、正文摘要与量化向量。
mod indexer;
mod items;
mod recall;
mod store;
mod strict_json;

use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_semantic_api::EmbeddingProfile;
pub use indexer::{IndexStatus, IndexStepError, Indexer};
pub use recall::{HybridRecall, MIN_SIMILARITY};
use std::sync::{Arc, Mutex};
use store::StoredIndex;
pub use store::{IndexEntry, IndexError, MAX_ENTRIES, SEMANTIC_PLUGIN_ID, SEMANTIC_STATE_KEY};

/// 索引句柄只交给可信宿主；不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct SemanticIndex {
    active: Arc<Mutex<Option<Arc<StoredIndex>>>>,
}
impl SemanticIndex {
    fn service(&self) -> Result<Arc<StoredIndex>, IndexError> {
        self.active
            .lock()
            .map_err(|_| IndexError::Unavailable)?
            .clone()
            .ok_or(IndexError::Unavailable)
    }
    pub fn entries(&self) -> Result<Vec<IndexEntry>, IndexError> {
        self.service()?.entries()
    }
    pub fn apply(
        &self,
        profile: &EmbeddingProfile,
        upserts: Vec<IndexEntry>,
        removals: Vec<(String, String)>,
    ) -> Result<(), IndexError> {
        self.service()?.apply(profile, upserts, removals)
    }
}

pub struct SemanticPlugin {
    manifest: PluginManifest,
    index: SemanticIndex,
}
impl SemanticPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(SEMANTIC_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            index: SemanticIndex::default(),
        })
    }
    pub fn index(&self) -> SemanticIndex {
        self.index.clone()
    }
}
impl Plugin for SemanticPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredIndex::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let index = self.index.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = index
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("语义索引句柄不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .index
                .active
                .lock()
                .map_err(|_| PluginError::State("语义索引句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
