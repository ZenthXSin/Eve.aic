//! 主动提问训练的公开契约；启停权属于可信通道，不作为模型工具发布。
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use std::sync::Arc;

pub const TRAINING_PLUGIN_ID: &str = "eve.training";
pub const TRAINING_SERVICE_ID: &str = "eve.training.mode";

pub trait TrainingService: Send + Sync {
    fn enabled(&self, scope: &ContextScope) -> PluginResult<bool>;
    /// 必须先持久化再确认；失败保留原状态。
    fn set_enabled(&self, scope: &ContextScope, enabled: bool) -> PluginResult<()>;
}
#[derive(Clone)]
pub struct TrainingServiceHandle(pub Arc<dyn TrainingService>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrainingCommand {
    Start,
    Stop,
    Status,
    Help,
}
impl TrainingCommand {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "/train start" => Some(Self::Start),
            "/train stop" => Some(Self::Stop),
            "/train status" => Some(Self::Status),
            text if text == "/train" || text.starts_with("/train ") => Some(Self::Help),
            _ => None,
        }
    }
}
