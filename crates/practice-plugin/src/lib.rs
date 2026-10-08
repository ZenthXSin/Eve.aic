//! 实践账本、单次模型草稿器，以及串起草稿、结构检查、实际运行与修正的实践流程。
mod drafter;
mod practitioner;
mod store;
mod strict_json;

pub use drafter::{DRAFTER_VERSION, ModelPracticeDrafter};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_practice_api::*;
pub use practitioner::Practitioner;
use std::sync::{Arc, Mutex};
use store::StoredPractice;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct PracticeController {
    active: Arc<Mutex<Option<Arc<StoredPractice>>>>,
}
impl PracticeController {
    fn service(&self) -> PracticeResult<Arc<StoredPractice>> {
        self.active
            .lock()
            .map_err(|_| PracticeError::Unavailable)?
            .clone()
            .ok_or(PracticeError::Unavailable)
    }
}
impl PracticeAdmin for PracticeController {
    fn snapshot(&self) -> PracticeResult<PracticeSnapshot> {
        self.service()?.snapshot()
    }
    fn begin(
        &self,
        task: PracticeTask,
        runner: &RunnerProfile,
        drafter_version: &str,
        now_ms: u64,
    ) -> PracticeResult<Option<PracticeRun>> {
        self.service()?.begin(task, runner, drafter_version, now_ms)
    }
    fn record_draft(
        &self,
        run_id: &str,
        at_ms: u64,
        result: Result<PracticeDraft, PracticeFailure>,
        issues: Vec<String>,
    ) -> PracticeResult<PracticeRun> {
        self.service()?.record_draft(run_id, at_ms, result, issues)
    }
    fn record_evidence(
        &self,
        run_id: &str,
        at_ms: u64,
        evidence: RunEvidence,
    ) -> PracticeResult<PracticeRun> {
        self.service()?.record_evidence(run_id, at_ms, evidence)
    }
    fn abandon(
        &self,
        run_id: &str,
        at_ms: u64,
        failure: PracticeFailure,
    ) -> PracticeResult<PracticeRun> {
        self.service()?.abandon(run_id, at_ms, failure)
    }
}

pub struct PracticePlugin {
    manifest: PluginManifest,
    controller: PracticeController,
}
impl PracticePlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(PRACTICE_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: PracticeController::default(),
        })
    }
    pub fn controller(&self) -> PracticeController {
        self.controller.clone()
    }
}
impl Plugin for PracticePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredPractice::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("实践管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("实践管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
