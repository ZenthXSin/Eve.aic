//! 完成确认提问账本。
mod store;
mod strict_json;

use eve_completion_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use std::sync::{Arc, Mutex};
use store::StoredQuestions;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct CompletionController {
    active: Arc<Mutex<Option<Arc<StoredQuestions>>>>,
}
impl CompletionController {
    fn service(&self) -> CompletionResult<Arc<StoredQuestions>> {
        self.active
            .lock()
            .map_err(|_| CompletionError::Unavailable)?
            .clone()
            .ok_or(CompletionError::Unavailable)
    }
}
impl CompletionLedger for CompletionController {
    fn snapshot(&self) -> CompletionResult<CompletionSnapshot> {
        self.service()?.snapshot()
    }
    fn claim(
        &self,
        target: &QuestionTarget,
        message_id: &str,
        now_ms: u64,
    ) -> CompletionResult<CompletionQuestion> {
        self.service()?.claim(target, message_id, now_ms)
    }
    fn record(
        &self,
        id: &str,
        result: AttemptResult,
        now_ms: u64,
    ) -> CompletionResult<CompletionQuestion> {
        self.service()?.record(id, result, now_ms)
    }
}

pub struct CompletionPlugin {
    manifest: PluginManifest,
    controller: CompletionController,
}
impl CompletionPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(COMPLETION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: CompletionController::default(),
        })
    }
    pub fn controller(&self) -> CompletionController {
        self.controller.clone()
    }
}
impl Plugin for CompletionPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredQuestions::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("完成确认提问管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("完成确认提问管理句柄不可用".into()))? =
                Some(stored);
            Ok(None)
        })
    }
}
