//! 技能账本、提炼器与选择器，以及串起提炼、宿主检查、验证运行与调用结算的流程。
mod consolidator;
mod invocation;
mod model;
mod store;
mod strict_json;
mod tool;

pub use consolidator::{Candidate, Consolidator};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_practice_api::{PracticeDraft, RunEvidence, RunnerProfile};
use eve_skill_api::*;
pub use invocation::{Clock, SkillAwareDrafter, settle};
pub use model::{DISTILLER_VERSION, ModelSkillDistiller, ModelSkillSelector, SELECTOR_VERSION};
use std::sync::{Arc, Mutex};
use store::StoredSkills;
pub use tool::{SKILL_TOOL_NAME, SkillTool};

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct SkillController {
    active: Arc<Mutex<Option<Arc<StoredSkills>>>>,
}
impl SkillController {
    fn service(&self) -> SkillResult<Arc<StoredSkills>> {
        self.active
            .lock()
            .map_err(|_| SkillError::Unavailable)?
            .clone()
            .ok_or(SkillError::Unavailable)
    }
}
impl SkillAdmin for SkillController {
    fn snapshot(&self) -> SkillResult<SkillSnapshot> {
        self.service()?.snapshot()
    }
    fn begin_distillation(
        &self,
        owner: &str,
        origin: SkillOrigin,
        source: PracticeDraft,
        runner: &RunnerProfile,
        distiller_version: &str,
        now_ms: u64,
    ) -> SkillResult<Option<Distillation>> {
        self.service()?
            .begin_distillation(owner, origin, source, runner, distiller_version, now_ms)
    }
    fn record_proposal(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<Proposal, SkillFailure>,
        issues: Vec<String>,
        holdout: Option<Arguments>,
    ) -> SkillResult<Distillation> {
        self.service()?
            .record_proposal(id, at_ms, result, issues, holdout)
    }
    fn record_verification(
        &self,
        id: &str,
        at_ms: u64,
        evidence: RunEvidence,
    ) -> SkillResult<Distillation> {
        self.service()?.record_verification(id, at_ms, evidence)
    }
    fn abandon_distillation(
        &self,
        id: &str,
        at_ms: u64,
        failure: SkillFailure,
    ) -> SkillResult<Distillation> {
        self.service()?.abandon_distillation(id, at_ms, failure)
    }
    fn set_enabled(
        &self,
        skill_id: &str,
        at_ms: u64,
        actor: Actor,
        enabled: Option<u32>,
    ) -> SkillResult<Skill> {
        self.service()?.set_enabled(skill_id, at_ms, actor, enabled)
    }
    fn begin_selection(
        &self,
        id: &str,
        owner: &str,
        goal_id: &str,
        candidates: Vec<SkillRef>,
        selector_version: &str,
        now_ms: u64,
    ) -> SkillResult<Option<Selection>> {
        self.service()?
            .begin_selection(id, owner, goal_id, candidates, selector_version, now_ms)
    }
    fn record_selection(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<Option<Choice>, SkillFailure>,
        issues: Vec<String>,
    ) -> SkillResult<Selection> {
        self.service()?.record_selection(id, at_ms, result, issues)
    }
    fn settle(&self, id: &str, at_ms: u64, outcome: InvocationOutcome) -> SkillResult<Selection> {
        self.service()?.settle(id, at_ms, outcome)
    }
    fn begin_tool_call(
        &self,
        id: &str,
        owner: &str,
        skill: SkillRef,
        arguments: Arguments,
        now_ms: u64,
    ) -> SkillResult<ToolCallRecord> {
        self.service()?
            .begin_tool_call(id, owner, skill, arguments, now_ms)
    }
    fn record_tool_call(
        &self,
        id: &str,
        at_ms: u64,
        outcome: InvocationOutcome,
        evidence: Option<RunEvidence>,
    ) -> SkillResult<ToolCallRecord> {
        self.service()?
            .record_tool_call(id, at_ms, outcome, evidence)
    }
}

pub struct SkillPlugin {
    manifest: PluginManifest,
    controller: SkillController,
}
impl SkillPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(SKILL_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: SkillController::default(),
        })
    }
    pub fn controller(&self) -> SkillController {
        self.controller.clone()
    }
}
impl Plugin for SkillPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredSkills::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("技能管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("技能管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
