//! 工具锻造账本、模型锻造器，以及串起缺口推导、锻造、回放验证与运行前调用的流程。
mod forge;
mod model;
mod store;
mod strict_json;

use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_toolforge_api::*;
pub use forge::{Candidate, Forge, ForgedChecks};
pub use model::{FORGER_VERSION, ModelToolForger};
use std::sync::{Arc, Mutex};
use store::StoredTools;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct ToolForgeController {
    active: Arc<Mutex<Option<Arc<StoredTools>>>>,
}
impl ToolForgeController {
    fn service(&self) -> ToolResult<Arc<StoredTools>> {
        self.active
            .lock()
            .map_err(|_| ToolError::Unavailable)?
            .clone()
            .ok_or(ToolError::Unavailable)
    }
}
impl ToolAdmin for ToolForgeController {
    fn snapshot(&self) -> ToolResult<ToolSnapshot> {
        self.service()?.snapshot()
    }
    fn begin_forge(
        &self,
        owner: &str,
        gap: GapRef,
        occurrences: usize,
        forger_version: &str,
        now_ms: u64,
    ) -> ToolResult<Option<ForgeAttempt>> {
        self.service()?
            .begin_forge(owner, gap, occurrences, forger_version, now_ms)
    }
    fn record_forge(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<ForgeOutput, ForgeFailure>,
        verification: Option<Verification>,
    ) -> ToolResult<ForgeAttempt> {
        self.service()?
            .record_forge(id, at_ms, result, verification)
    }
    fn record_reuse(
        &self,
        owner: &str,
        gap: GapRef,
        occurrences: usize,
        tool: ToolRef,
        verification: Verification,
        now_ms: u64,
    ) -> ToolResult<Option<ForgeAttempt>> {
        self.service()?
            .record_reuse(owner, gap, occurrences, tool, verification, now_ms)
    }
    fn abandon_forge(
        &self,
        id: &str,
        at_ms: u64,
        failure: ForgeFailure,
    ) -> ToolResult<ForgeAttempt> {
        self.service()?.abandon_forge(id, at_ms, failure)
    }
    fn set_enabled(
        &self,
        tool_id: &str,
        at_ms: u64,
        actor: Actor,
        enabled: Option<u32>,
    ) -> ToolResult<ForgedTool> {
        self.service()?.set_enabled(tool_id, at_ms, actor, enabled)
    }
    fn record_call(&self, call: ToolCall) -> ToolResult<ToolCall> {
        self.service()?.record_call(call)
    }
}

pub struct ToolForgePlugin {
    manifest: PluginManifest,
    controller: ToolForgeController,
}
impl ToolForgePlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(TOOLFORGE_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: ToolForgeController::default(),
        })
    }
    pub fn controller(&self) -> ToolForgeController {
        self.controller.clone()
    }
}
impl Plugin for ToolForgePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredTools::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("工具锻造管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("工具锻造管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
