//! 宿主在同一轮内提供的新用户消息；不改变工具、身份、步数或取消信号。
use crate::{ContextScope, LlmError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 同一次消费的消息与审核锚点必须原子取出；后到的消息不能提前成为本批授权。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SteeringBatch {
    pub messages: Vec<String>,
    pub review_anchor: Option<String>,
}

/// 同一条输入的即时交流由宿主另一支路接手。业务含义仍由原轮消费，
/// 这里不是用户原话授权，也不是再次触发模型或发送的队列。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputReplyHandoff {
    pub input_message_id: String,
    /// 从入站队列取出的原正文，只用于把多条输入分别关联到交接事实。
    pub input_text: String,
    pub event_id: String,
    pub reply_text: String,
    pub delivery: InputReplyDelivery,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputReplyDelivery {
    Sending,
    Sent { platform_message_id: String },
    Unknown,
    Failed,
    NotSent,
}

impl InputReplyHandoff {
    pub fn validate(&self) -> Result<(), LlmError> {
        let valid_id = |value: &str| {
            !value.is_empty()
                && value.len() <= 512
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        if !valid_id(&self.input_message_id)
            || !valid_id(&self.event_id)
            || self.input_text.is_empty()
            || self.input_text.len() > 256 * 1024
            || self.reply_text.trim().is_empty()
            || self.reply_text.len() > 32768
            || matches!(&self.delivery, InputReplyDelivery::Sent { platform_message_id } if !valid_id(platform_message_id))
        {
            return Err(LlmError::Context("输入交流交接事实无效".into()));
        }
        Ok(())
    }
}

/// 用户消息与同消息的即时交流交接必须在同一锁/事务中取出。
/// 终态更新可以单独出现，但不构成新用户输入，不要求额外模型调用。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SteeringUpdate {
    pub batch: SteeringBatch,
    pub input_replies: Vec<InputReplyHandoff>,
}

pub trait TurnSteering: Send + Sync {
    /// finish=true 时，若没有新消息则原子关闭入口，避免最终答复与新输入竞态。
    fn take(&self, finish: bool) -> Result<Vec<String>, LlmError>;
    fn take_batch(&self, finish: bool) -> Result<SteeringBatch, LlmError> {
        self.take(finish).map(|messages| SteeringBatch {
            messages,
            review_anchor: None,
        })
    }
    /// 旧来源保持空交接；实现可在同一消费边界提供已领取的即时交流事实。
    /// finish 仍只由新用户输入阻止关闭，交接回执本身不能延长或重启一轮。
    fn take_update(&self, finish: bool) -> Result<SteeringUpdate, LlmError> {
        self.take_batch(finish).map(|batch| SteeringUpdate {
            batch,
            input_replies: Vec::new(),
        })
    }
}

pub trait TurnSteeringSource: Send + Sync {
    /// 每个会话轮次独立入口；句柄析构时必须关闭，禁止流入下一轮。
    fn open(&self, scope: &ContextScope, turn_id: u64) -> Result<Arc<dyn TurnSteering>, LlmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_steering_sources_keep_their_batch_without_new_observations() {
        struct OriginalSource;
        impl TurnSteering for OriginalSource {
            fn take(&self, finish: bool) -> Result<Vec<String>, LlmError> {
                Ok(if finish {
                    vec![]
                } else {
                    vec!["真实新输入".into()]
                })
            }
        }
        assert_eq!(
            OriginalSource.take_update(false).unwrap(),
            SteeringUpdate {
                batch: SteeringBatch {
                    messages: vec!["真实新输入".into()],
                    review_anchor: None
                },
                input_replies: vec![],
            }
        );
        assert_eq!(
            OriginalSource.take_update(true).unwrap(),
            SteeringUpdate::default()
        );
    }

    #[test]
    fn input_reply_delivery_requires_a_platform_id_to_claim_confirmed_delivery() {
        let mut handoff = InputReplyHandoff {
            input_message_id: "input-1".into(),
            input_text: "继续".into(),
            event_id: "event-1".into(),
            reply_text: "我在继续".into(),
            delivery: InputReplyDelivery::Sending,
        };
        handoff.validate().unwrap();
        let encoded = serde_json::to_value(&handoff).unwrap();
        assert_eq!(encoded["delivery"]["state"], "sending");
        assert!(encoded["delivery"].get("platform_message_id").is_none());
        handoff.delivery = InputReplyDelivery::Sent {
            platform_message_id: String::new(),
        };
        assert!(handoff.validate().is_err());
        assert!(
            serde_json::from_value::<InputReplyDelivery>(serde_json::json!({"state":"sent"}))
                .is_err()
        );
        handoff.delivery = InputReplyDelivery::Sent {
            platform_message_id: "platform-1".into(),
        };
        handoff.validate().unwrap();
        assert_eq!(
            serde_json::from_value::<InputReplyHandoff>(serde_json::to_value(&handoff).unwrap())
                .unwrap(),
            handoff
        );
        handoff.input_message_id = "input\nother".into();
        assert!(handoff.validate().is_err());
    }
}
