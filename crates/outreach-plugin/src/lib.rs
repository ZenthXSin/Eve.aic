//! 主动交流账本，单次模型请求的邀请撰写器、时机判断器与回应识别器，
//! 以及由用户提出的想法确定性派生后续创作目标的派生器。
mod composer;
mod goal;
mod store;
mod strict_json;

pub use composer::{
    COMPOSER_VERSION, JUDGE_VERSION, ModelInvitationComposer, ModelResponseJudge, ModelTimingJudge,
    RESPONSE_JUDGE_VERSION,
};
use eve_outreach_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
pub use goal::{REQUEST_DERIVER_VERSION, RequestGoals};
use std::sync::{Arc, Mutex};
use store::StoredOutreach;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct OutreachController {
    active: Arc<Mutex<Option<Arc<StoredOutreach>>>>,
}
impl OutreachController {
    fn service(&self) -> OutreachResult<Arc<StoredOutreach>> {
        self.active
            .lock()
            .map_err(|_| OutreachError::Unavailable)?
            .clone()
            .ok_or(OutreachError::Unavailable)
    }
}
impl OutreachAdmin for OutreachController {
    fn snapshot(&self) -> OutreachResult<OutreachSnapshot> {
        self.service()?.snapshot()
    }
    fn begin(
        &self,
        owner: &str,
        goal_id: &str,
        milestone: Milestone,
        facts: Vec<Fact>,
        composer_version: &str,
        now_ms: u64,
    ) -> OutreachResult<Option<Invitation>> {
        self.service()?
            .begin(owner, goal_id, milestone, facts, composer_version, now_ms)
    }
    fn record_composition(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<String, OutreachFailure>,
    ) -> OutreachResult<Invitation> {
        self.service()?.record_composition(id, at_ms, result)
    }
    fn begin_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
    ) -> OutreachResult<Invitation> {
        self.service()?.begin_judgement(id, message_id, at_ms)
    }
    fn record_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
        outcome: Result<Verdict, OutreachFailure>,
    ) -> OutreachResult<Invitation> {
        self.service()?
            .record_judgement(id, message_id, at_ms, outcome)
    }
    fn claim(&self, id: &str, at_ms: u64, channel: DeliveryChannel) -> OutreachResult<Invitation> {
        self.service()?.claim(id, at_ms, channel)
    }
    fn record_delivery(
        &self,
        id: &str,
        at_ms: u64,
        result: AttemptResult,
    ) -> OutreachResult<Invitation> {
        self.service()?.record_delivery(id, at_ms, result)
    }
    fn cancel(&self, id: &str, at_ms: u64, reason: CancelReason) -> OutreachResult<Invitation> {
        self.service()?.cancel(id, at_ms, reason)
    }
    fn set_quiet(&self, owner: &str, quiet: bool, at_ms: u64) -> OutreachResult<OwnerPreference> {
        self.service()?.set_quiet(owner, quiet, at_ms)
    }
    fn begin_response(
        &self,
        id: &str,
        turns: Vec<ResponseTurn>,
        at_ms: u64,
    ) -> OutreachResult<Invitation> {
        self.service()?.begin_response(id, turns, at_ms)
    }
    fn record_response(
        &self,
        id: &str,
        at_ms: u64,
        outcome: Result<ResponseVerdict, OutreachFailure>,
    ) -> OutreachResult<Invitation> {
        self.service()?.record_response(id, at_ms, outcome)
    }
}

pub struct OutreachPlugin {
    manifest: PluginManifest,
    controller: OutreachController,
}
impl OutreachPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(OUTREACH_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: OutreachController::default(),
        })
    }
    pub fn controller(&self) -> OutreachController {
        self.controller.clone()
    }
}
impl Plugin for OutreachPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredOutreach::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("主动交流管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("主动交流管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
