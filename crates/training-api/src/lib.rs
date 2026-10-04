//! 主动提问训练的公开契约；启停权属于可信通道，不作为模型工具发布。
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use std::sync::Arc;

pub const TRAINING_PLUGIN_ID: &str = "eve.training";
pub const TRAINING_SERVICE_ID: &str = "eve.training.mode";

/// 用户原文的表达统计，不包含正文、身份、观点或已确认的长期偏好。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpressionSnapshot {
    pub revision: String,
    pub samples: usize,
    pub median_chars: usize,
    pub p75_chars: usize,
    pub short_percent: usize,
    pub single_paragraph_percent: usize,
    pub question_percent: usize,
    pub formal_percent: usize,
    pub casual_percent: usize,
}

pub trait TrainingService: Send + Sync {
    fn enabled(&self, scope: &ContextScope) -> PluginResult<bool>;
    /// 必须先持久化再确认；失败保留原状态。
    fn set_enabled(&self, scope: &ContextScope, enabled: bool) -> PluginResult<()>;
    /// 只接收可信宿主验证后的用户输入；重复 ID 不重复学习。
    fn observe_user_message(
        &self,
        _scope: &ContextScope,
        _id: &str,
        _text: &str,
    ) -> PluginResult<()> {
        Ok(())
    }
    fn expression_snapshot(
        &self,
        _scope: &ContextScope,
    ) -> PluginResult<Option<ExpressionSnapshot>> {
        Ok(None)
    }
    /// 跨会话仅汇总数值特征，供样本不足时采用一般表达习惯。
    fn expression_baseline(&self) -> PluginResult<Option<ExpressionSnapshot>> {
        Ok(None)
    }
    /// 保留去重凭据与原始历史，重置当前可信作用域的统计。
    fn reset_expression(&self, _scope: &ContextScope) -> PluginResult<()> {
        Err(eve_plugin_api::PluginError::State(
            "该训练实现不支持重置".into(),
        ))
    }
}
#[derive(Clone)]
pub struct TrainingServiceHandle(pub Arc<dyn TrainingService>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrainingCommand {
    Start,
    Stop,
    Status,
    Stats,
    Reset,
    Help,
}
impl TrainingCommand {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "/train start" => Some(Self::Start),
            "/train stop" => Some(Self::Stop),
            "/train status" => Some(Self::Status),
            "/train stats" => Some(Self::Stats),
            "/train reset" => Some(Self::Reset),
            text if text == "/train" || text.starts_with("/train ") => Some(Self::Help),
            _ => None,
        }
    }
}
