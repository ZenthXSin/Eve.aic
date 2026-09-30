//! 可替换消息判断与动作路由插件；只依赖公开服务，不调用模型或修改 Kernel。
mod router;
mod rules;
use eve_message_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginFuture, PluginManifest, PluginResult, ServiceId, cleanup,
};
pub use router::MessageRouterPlugin;
pub use rules::RulesJudge;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct ActiveJudge {
    judge: Arc<dyn RelationJudge>,
    active: Arc<AtomicBool>,
}
impl RelationJudge for ActiveJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        Box::pin(async move {
            if !self.active.load(Ordering::SeqCst) {
                return Err(RelationError::Unavailable);
            }
            let result = self.judge.judge(input).await;
            if !self.active.load(Ordering::SeqCst) {
                return Err(RelationError::Unavailable);
            }
            result
        })
    }
}
pub struct RelationPlugin {
    manifest: PluginManifest,
    judge: Arc<dyn RelationJudge>,
}
impl RelationPlugin {
    pub fn new(judge: Arc<dyn RelationJudge>) -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(RELATION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            judge,
        })
    }
    pub fn rules() -> PluginResult<Self> {
        Self::new(Arc::new(RulesJudge))
    }
}
impl Plugin for RelationPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let judge = self.judge.clone();
        Box::pin(async move {
            let active = Arc::new(AtomicBool::new(true));
            let closing = active.clone();
            ctx.cleanup(cleanup(move || async move {
                closing.store(false, Ordering::SeqCst);
                Ok(())
            }))?;
            ctx.provide_service(
                ServiceId::new(RELATION_SERVICE_ID)?,
                RelationServiceHandle(Arc::new(ActiveJudge { judge, active })),
            )?;
            Ok(None)
        })
    }
}
