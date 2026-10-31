//! 工作便笺索引常驻，正文分版本落盘；原会话、原话和工具回执不在此插件修改范围。
mod archive;
mod store;
mod strict_json;
pub use archive::FileContextArchive;
use eve_context_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use std::sync::{Arc, Mutex};
use store::StoredContext;

pub use store::{
    CONTEXT_INDEX_KEY, MAX_CONTEXT_INDEX_BYTES, MAX_CONTEXT_OWNERS, MAX_CONTEXT_VERSIONS,
};

#[derive(Clone, Default)]
pub struct ContextController {
    active: Arc<Mutex<Option<Arc<StoredContext>>>>,
}
impl ContextController {
    fn service(&self) -> ContextResult<Arc<StoredContext>> {
        self.active
            .lock()
            .map_err(|_| ContextError::Unavailable)?
            .clone()
            .ok_or(ContextError::Unavailable)
    }
}
impl ContextAdmin for ContextController {
    fn inspect(&self, owner: &ContextOwner) -> ContextResult<ContextView> {
        self.service()?.inspect(owner)
    }
    fn read_version(&self, owner: &ContextOwner, revision: u64) -> ContextResult<ContextView> {
        self.service()?.read_version(owner, revision)
    }
    fn versions(
        &self,
        owner: &ContextOwner,
        before: Option<u64>,
        limit: usize,
    ) -> ContextResult<Vec<ContextVersion>> {
        self.service()?.versions(owner, before, limit)
    }
    fn preview(&self, owner: &ContextOwner, text: &str) -> ContextResult<ContextPreview> {
        self.service()?.preview(owner, text)
    }
    fn update(
        &self,
        owner: &ContextOwner,
        expected_revision: u64,
        text: &str,
    ) -> ContextResult<ContextView> {
        self.service()?.update(owner, expected_revision, text)
    }
    fn rollback(
        &self,
        owner: &ContextOwner,
        expected_revision: u64,
        target_revision: u64,
    ) -> ContextResult<ContextView> {
        self.service()?
            .rollback(owner, expected_revision, target_revision)
    }
}

pub struct ContextPlugin {
    manifest: PluginManifest,
    controller: ContextController,
    archive: Arc<dyn ContextArchiveStore>,
}
impl ContextPlugin {
    pub fn new(archive: Arc<dyn ContextArchiveStore>) -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(CONTEXT_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: ContextController::default(),
            archive,
        })
    }
    pub fn controller(&self) -> ContextController {
        self.controller.clone()
    }
}
impl Plugin for ContextPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let service = Arc::new(
                StoredContext::open(context.clone(), self.archive.clone())
                    .map_err(|e| PluginError::State(e.to_string()))?,
            );
            let to_close = service.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close();
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("便笺管理句柄不可用".into()))?;
                if active.as_ref().is_some_and(|v| Arc::ptr_eq(v, &to_close)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("便笺管理句柄不可用".into()))? = Some(service);
            Ok(None)
        })
    }
}
