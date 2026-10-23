//! 消息关系经验契约：只提供当前范围的分类示例，不授予控制权限。
use crate::{MessageIntent, RelationDecision, RelationError, RelationInput};
use serde::{Deserialize, Serialize};

pub const MAX_RELATION_EXAMPLES: usize = 4;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationExamplePart {
    pub intent: MessageIntent,
    pub text: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationExperienceSource {
    /// 通过格式与置信度校验的模型分类，仍可能判断错误，不代表任务成功。
    PrimaryModel,
    UserClarification,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationExample {
    pub task_text: String,
    pub message: String,
    pub parts: Vec<RelationExamplePart>,
    pub explanation: String,
    pub source: RelationExperienceSource,
}
impl RelationExample {
    pub fn validate(&self) -> Result<(), RelationError> {
        if self.task_text.trim().is_empty()
            || self.task_text.len() > 8192
            || self.message.trim().is_empty()
            || self.message.len() > 2048
            || self.explanation.trim().is_empty()
            || self.explanation.len() > 4096
            || self.parts.is_empty()
            || self.parts.len() > 16
            || self.parts.iter().any(|p| {
                !matches!(
                    p.intent,
                    MessageIntent::Supplement
                        | MessageIntent::Correction
                        | MessageIntent::Continue
                        | MessageIntent::NewTask
                        | MessageIntent::Unrelated
                ) || match &p.text {
                    Some(t) => {
                        t.trim().is_empty()
                            || !self.message.contains(t)
                            || matches!(
                                p.intent,
                                MessageIntent::Continue | MessageIntent::Unrelated
                            )
                    }
                    None => !matches!(p.intent, MessageIntent::Continue | MessageIntent::Unrelated),
                }
            })
        {
            return Err(RelationError::Protocol);
        }
        Ok(())
    }
}
/// 实现负责用户与会话隔离、原子持久化和来源去重；模型只能读取示例，不能写任意规则。
pub trait RelationExperienceStore: Send + Sync {
    fn recall(&self, input: &RelationInput) -> Result<Vec<RelationExample>, RelationError>;
    fn remember(
        &self,
        input: &RelationInput,
        decision: &RelationDecision,
        source: RelationExperienceSource,
    ) -> Result<(), RelationError>;
}
