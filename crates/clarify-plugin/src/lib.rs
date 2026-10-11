//! 请求澄清账本与单次模型请求的提问撰写器。
mod composer;
mod store;
mod strict_json;

pub use composer::{COMPOSER_VERSION, ModelClarificationComposer};
use eve_clarify_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use std::sync::{Arc, Mutex};
use store::StoredClarify;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct ClarifyController {
    active: Arc<Mutex<Option<Arc<StoredClarify>>>>,
}
impl ClarifyController {
    fn service(&self) -> ClarifyResult<Arc<StoredClarify>> {
        self.active
            .lock()
            .map_err(|_| ClarifyError::Unavailable)?
            .clone()
            .ok_or(ClarifyError::Unavailable)
    }
}
impl ClarifyAdmin for ClarifyController {
    fn snapshot(&self) -> ClarifyResult<ClarifySnapshot> {
        self.service()?.snapshot()
    }
    fn begin(
        &self,
        owner: &str,
        goal_id: &str,
        goal_revision: u64,
        stall: Stall,
        facts: Vec<Fact>,
        composer_version: &str,
        now_ms: u64,
    ) -> ClarifyResult<Option<Clarification>> {
        self.service()?.begin(
            owner,
            goal_id,
            goal_revision,
            stall,
            facts,
            composer_version,
            now_ms,
        )
    }
    fn record_composition(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<String, ClarifyFailure>,
    ) -> ClarifyResult<Clarification> {
        self.service()?.record_composition(id, at_ms, result)
    }
    fn begin_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
    ) -> ClarifyResult<Clarification> {
        self.service()?.begin_judgement(id, message_id, at_ms)
    }
    fn record_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
        outcome: Result<bool, ClarifyFailure>,
    ) -> ClarifyResult<Clarification> {
        self.service()?
            .record_judgement(id, message_id, at_ms, outcome)
    }
    fn claim(&self, id: &str, message_id: &str, at_ms: u64) -> ClarifyResult<Clarification> {
        self.service()?.claim(id, message_id, at_ms)
    }
    fn record_delivery(
        &self,
        id: &str,
        at_ms: u64,
        result: DeliveryResult,
    ) -> ClarifyResult<Clarification> {
        self.service()?.record_delivery(id, at_ms, result)
    }
    fn record_answer(&self, id: &str, answer: Answer) -> ClarifyResult<Clarification> {
        self.service()?.record_answer(id, answer)
    }
    fn expire(&self, id: &str, at_ms: u64) -> ClarifyResult<Clarification> {
        self.service()?.expire(id, at_ms)
    }
    fn cancel(&self, id: &str, at_ms: u64, reason: CancelReason) -> ClarifyResult<Clarification> {
        self.service()?.cancel(id, at_ms, reason)
    }
    fn set_quiet(&self, owner: &str, quiet: bool, at_ms: u64) -> ClarifyResult<OwnerPreference> {
        self.service()?.set_quiet(owner, quiet, at_ms)
    }
}

pub struct ClarifyPlugin {
    manifest: PluginManifest,
    controller: ClarifyController,
}
impl ClarifyPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(CLARIFY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: ClarifyController::default(),
        })
    }
    pub fn controller(&self) -> ClarifyController {
        self.controller.clone()
    }
}
impl Plugin for ClarifyPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredClarify::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("请求澄清管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("请求澄清管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
