//! 可替换消息判断与动作路由插件；只依赖公开服务，不调用模型或修改 Kernel。
mod fallback;
mod router;
mod rules;
use eve_message_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginId,
    PluginManifest, PluginResult, ServiceId, cleanup,
};
pub use fallback::FallbackJudge;
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
    source: JudgeSource,
}
enum JudgeSource {
    Direct(Arc<dyn RelationJudge>),
    Fallback {
        primary: Option<Arc<dyn RelationJudge>>,
        fallback: Arc<dyn RelationJudge>,
    },
}
impl RelationPlugin {
    pub fn new(judge: Arc<dyn RelationJudge>) -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(RELATION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            source: JudgeSource::Direct(judge),
        })
    }
    /// 可选判断器可关闭；复杂判断由宿主注入，插件只读取公开配置并组合回退。
    pub fn with_fallback(
        primary: Option<Arc<dyn RelationJudge>>,
        fallback: Arc<dyn RelationJudge>,
    ) -> PluginResult<Self> {
        let mut plugin = Self::new(fallback.clone())?;
        plugin.manifest.dependencies.push(PluginDependency {
            id: PluginId::new(eve_config_api::CONFIG_PLUGIN_ID)?,
            requirement: Some("^0.1".into()),
        });
        plugin.source = JudgeSource::Fallback { primary, fallback };
        Ok(plugin)
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
        let source = match &self.source {
            JudgeSource::Direct(judge) => JudgeSource::Direct(judge.clone()),
            JudgeSource::Fallback { primary, fallback } => JudgeSource::Fallback {
                primary: primary.clone(),
                fallback: fallback.clone(),
            },
        };
        Box::pin(async move {
            let judge: Arc<dyn RelationJudge> = match source {
                JudgeSource::Direct(judge) => judge,
                JudgeSource::Fallback { primary, fallback } => {
                    let config = ctx
                        .service::<eve_config_api::ConfigServiceHandle>(&ServiceId::new(
                            eve_config_api::CONFIG_SERVICE_ID,
                        )?)?
                        .ok_or_else(|| PluginError::State("缺少消息配置服务".into()))?
                        .0
                        .clone();
                    Arc::new(FallbackJudge::new(config, primary, fallback))
                }
            };
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
