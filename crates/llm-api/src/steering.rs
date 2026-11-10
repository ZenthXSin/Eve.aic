//! 宿主在同一轮内提供的新用户消息；不改变工具、身份、步数或取消信号。
use crate::{ContextScope, LlmError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 单轮与持久失败证据共享的有界交接快照上限。
pub const MAX_INPUT_REPLY_HANDOFFS: usize = 64;
pub const MAX_INPUT_REPLY_HANDOFF_BYTES: usize = 512 * 1024;
pub const MAX_INPUT_REPLY_PARTS: usize = 5;
pub const MAX_INPUT_REPLY_PART_BYTES: usize = 2048;

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
    /// 默认仍是原输入交接；progress 只是原轮发言事实，不生成或消费用户输入。
    #[serde(default, skip_serializing_if = "InputReplyKind::is_user_input")]
    pub kind: InputReplyKind,
    pub input_message_id: String,
    /// 从入站队列取出的原正文，只用于把多条输入分别关联到交接事实。
    pub input_text: String,
    pub event_id: String,
    pub reply_text: String,
    pub delivery: InputReplyDelivery,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputReplyKind {
    #[default]
    UserInput,
    Progress,
}
impl InputReplyKind {
    fn is_user_input(&self) -> bool {
        *self == Self::UserInput
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputReplyDelivery {
    Sending,
    Sent {
        platform_message_id: String,
    },
    Unknown,
    Failed,
    NotSent,
    /// 同一事件的完整有序快照；首条成功不代表全组送达。
    Parts {
        parts: Vec<InputReplyPart>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputReplyPart {
    pub index: usize,
    pub text: String,
    pub delivery: InputReplyPartDelivery,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputReplyPartDelivery {
    Planned,
    Sending,
    Sent { platform_message_id: String },
    Failed,
    Unknown,
    NotAttempted,
}
impl<'de> Deserialize<'de> for InputReplyPartDelivery {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Planned {},
            Sending {},
            Sent { platform_message_id: String },
            Failed {},
            Unknown {},
            NotAttempted {},
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Planned {} => Self::Planned,
            Wire::Sending {} => Self::Sending,
            Wire::Sent {
                platform_message_id,
            } => Self::Sent {
                platform_message_id,
            },
            Wire::Failed {} => Self::Failed,
            Wire::Unknown {} => Self::Unknown,
            Wire::NotAttempted {} => Self::NotAttempted,
        })
    }
}
impl InputReplyPartDelivery {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Planned | Self::Sending)
    }
    fn permits_snapshot(&self, next: &Self) -> bool {
        self == next
            || match self {
                // 消费者可跳过中间快照，但已确认或未知的终态绝不倒退。
                Self::Planned => true,
                Self::Sending => matches!(next, Self::Sent { .. } | Self::Failed | Self::Unknown),
                _ => false,
            }
    }
}
impl InputReplyDelivery {
    /// 校验独立持久层的同一事件快照；旧单条不受新增分条上限影响。
    pub fn validate_parts(&self, reply_text: &str) -> Result<(), LlmError> {
        let Self::Parts { parts } = self else {
            return Ok(());
        };
        let invalid = || LlmError::Context("输入交流分条快照无效".into());
        if parts.is_empty()
            || parts.len() > MAX_INPUT_REPLY_PARTS
            || reply_text.len() > MAX_INPUT_REPLY_PART_BYTES
            || parts
                .iter()
                .fold(0usize, |bytes, part| bytes.saturating_add(part.text.len()))
                .saturating_add(parts.len().saturating_sub(1) * 2)
                != reply_text.len()
            || parts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
                != reply_text
        {
            return Err(invalid());
        }
        // 0=已确认前缀，1=唯一在途，2=待发送后缀，3=已停止后缀。
        let mut phase = 0;
        let mut ids = std::collections::BTreeSet::new();
        for (index, part) in parts.iter().enumerate() {
            if part.index != index || part.text.trim().is_empty() {
                return Err(invalid());
            }
            match &part.delivery {
                InputReplyPartDelivery::Sent {
                    platform_message_id,
                } if phase == 0
                    && valid_id(platform_message_id)
                    && ids.insert(platform_message_id) => {}
                InputReplyPartDelivery::Planned if phase <= 2 => {
                    phase = 2;
                }
                InputReplyPartDelivery::Sending if phase == 0 => {
                    phase = 1;
                }
                InputReplyPartDelivery::Failed | InputReplyPartDelivery::Unknown if phase == 0 => {
                    phase = 3;
                }
                // 取消时在途条保持 Sending，未开始的后缀同时封为 NotAttempted。
                InputReplyPartDelivery::NotAttempted if phase != 2 => {
                    phase = 3;
                }
                _ => return Err(invalid()),
            }
        }
        Ok(())
    }
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Sending => false,
            Self::Parts { parts } => {
                !parts.is_empty() && parts.iter().all(|p| p.delivery.is_terminal())
            }
            _ => true,
        }
    }
    pub fn permits_snapshot(&self, next: &Self) -> bool {
        match (self, next) {
            (Self::Parts { parts }, Self::Parts { parts: next }) => {
                parts.len() == next.len()
                    && parts.iter().zip(next).all(|(old, new)| {
                        old.index == new.index
                            && old.text == new.text
                            && old.delivery.permits_snapshot(&new.delivery)
                    })
            }
            (Self::Sending, Self::Parts { .. }) => false,
            (Self::Sending, _) => true,
            _ => self == next,
        }
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

impl<'de> Deserialize<'de> for InputReplyDelivery {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 内部标签的 unit variant 会忽略未知键，持久交接须用空 struct 严格读取。
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Sending {},
            Sent { platform_message_id: String },
            Unknown {},
            Failed {},
            NotSent {},
            Parts { parts: Vec<InputReplyPart> },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Sending {} => Self::Sending,
            Wire::Sent {
                platform_message_id,
            } => Self::Sent {
                platform_message_id,
            },
            Wire::Unknown {} => Self::Unknown,
            Wire::Failed {} => Self::Failed,
            Wire::NotSent {} => Self::NotSent,
            Wire::Parts { parts } => Self::Parts { parts },
        })
    }
}

impl InputReplyHandoff {
    pub fn validate(&self) -> Result<(), LlmError> {
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
        self.delivery.validate_parts(&self.reply_text)?;
        Ok(())
    }
    pub fn validate_successor(&self, next: &Self) -> Result<(), LlmError> {
        self.validate()?;
        next.validate()?;
        if self.kind != next.kind
            || self.input_message_id != next.input_message_id
            || self.input_text != next.input_text
            || self.event_id != next.event_id
            || self.reply_text != next.reply_text
            || !self.delivery.permits_snapshot(&next.delivery)
        {
            return Err(LlmError::Context("输入交流交接身份或终态不一致".into()));
        }
        Ok(())
    }
}

/// 每条真实入站消息、每个前台事件只保留一个最新状态；不按正文查重。
pub fn validate_input_reply_handoffs(handoffs: &[InputReplyHandoff]) -> Result<(), LlmError> {
    if handoffs.len() > MAX_INPUT_REPLY_HANDOFFS {
        return Err(LlmError::Context("输入交流交接批次过大".into()));
    }
    let mut inputs = std::collections::BTreeSet::new();
    let mut events = std::collections::BTreeSet::new();
    for handoff in handoffs {
        handoff.validate()?;
        if (handoff.kind == InputReplyKind::UserInput && !inputs.insert(&handoff.input_message_id))
            || !events.insert(&handoff.event_id)
        {
            return Err(LlmError::Context("输入交流交接身份重复".into()));
        }
    }
    if serde_json::to_vec(handoffs)
        .map_err(|_| LlmError::Context("输入交流交接数据无法序列化".into()))?
        .len()
        > MAX_INPUT_REPLY_HANDOFF_BYTES
    {
        return Err(LlmError::Context("输入交流交接数据过大".into()));
    }
    Ok(())
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

    fn parts(states: Vec<InputReplyPartDelivery>) -> InputReplyHandoff {
        let parts = states
            .into_iter()
            .enumerate()
            .map(|(index, delivery)| InputReplyPart {
                index,
                text: format!("第{index}条"),
                delivery,
            })
            .collect::<Vec<_>>();
        InputReplyHandoff {
            kind: InputReplyKind::UserInput,
            input_message_id: "input".into(),
            input_text: "继续查".into(),
            event_id: "event".into(),
            reply_text: parts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            delivery: InputReplyDelivery::Parts { parts },
        }
    }
    fn sent(id: &str) -> InputReplyPartDelivery {
        InputReplyPartDelivery::Sent {
            platform_message_id: id.into(),
        }
    }
    #[test]
    fn parts_keep_confirmed_prefix_and_unknown_suffix_without_claiming_whole_delivery() {
        use InputReplyPartDelivery::{NotAttempted, Planned, Sending, Unknown};
        let initial = parts(vec![Sending, Planned, Planned]);
        let middle = parts(vec![sent("platform-0"), Sending, Planned]);
        let cancelled = parts(vec![sent("platform-0"), Sending, NotAttempted]);
        let terminal = parts(vec![sent("platform-0"), Unknown, NotAttempted]);
        initial.validate_successor(&middle).unwrap();
        middle.validate_successor(&cancelled).unwrap();
        cancelled.validate_successor(&terminal).unwrap();
        assert!(!middle.delivery.is_terminal());
        assert!(terminal.delivery.is_terminal());
        assert!(terminal.validate_successor(&middle).is_err());
        assert!(
            terminal
                .validate_successor(&parts(vec![sent("different-id"), Unknown, NotAttempted]))
                .is_err()
        );
        assert!(
            terminal
                .validate_successor(&InputReplyHandoff {
                    delivery: InputReplyDelivery::Sent {
                        platform_message_id: "platform-0".into()
                    },
                    ..terminal.clone()
                })
                .is_err()
        );
        assert_eq!(
            serde_json::from_value::<InputReplyHandoff>(serde_json::to_value(&terminal).unwrap())
                .unwrap(),
            terminal
        );
        assert!(
            InputReplyHandoff {
                delivery: InputReplyDelivery::Sending,
                ..initial.clone()
            }
            .validate_successor(&initial)
            .is_err()
        );
    }
    #[test]
    fn parts_reject_out_of_order_receipts_changed_plan_and_unbounded_text() {
        use InputReplyPartDelivery::{Failed, NotAttempted, Planned, Sending, Unknown};
        for invalid in [
            vec![],
            vec![Sending; 6],
            vec![Sending, Sending],
            vec![Planned, Sending],
            vec![Failed, Planned],
            vec![Unknown, Planned],
            vec![NotAttempted, Planned],
            vec![Planned, NotAttempted],
            vec![sent("same"), sent("same")],
            vec![Sending, sent("later")],
        ] {
            assert!(parts(invalid).validate().is_err());
        }
        let initial = parts(vec![Sending, Planned]);
        let mut changed = initial.clone();
        let InputReplyDelivery::Parts {
            parts: ref mut items,
        } = changed.delivery
        else {
            unreachable!()
        };
        items[1].index = 0;
        assert!(changed.validate().is_err());
        let mut changed = initial.clone();
        changed.reply_text.push_str("新文字");
        assert!(changed.validate().is_err());
        let huge = "a".repeat(MAX_INPUT_REPLY_PART_BYTES + 1);
        assert!(
            InputReplyDelivery::Parts {
                parts: vec![InputReplyPart {
                    index: 0,
                    text: huge.clone(),
                    delivery: Planned
                }]
            }
            .validate_parts(&huge)
            .is_err()
        );
        assert!(parts(vec![sent("")]).validate().is_err());
        let skipped = parts(vec![sent("0"), sent("1")]);
        initial.validate_successor(&skipped).unwrap();
    }
    #[test]
    fn every_part_state_and_container_rejects_unknown_wire_fields() {
        for state in [
            "planned",
            "sending",
            "sent",
            "failed",
            "unknown",
            "not_attempted",
        ] {
            let mut value = serde_json::json!({"state":state});
            if state == "sent" {
                value["platform_message_id"] = "id".into();
            }
            assert!(serde_json::from_value::<InputReplyPartDelivery>(value.clone()).is_ok());
            value["unexpected"] = true.into();
            assert!(serde_json::from_value::<InputReplyPartDelivery>(value).is_err());
        }
        let mut value = serde_json::to_value(parts(vec![InputReplyPartDelivery::Planned])).unwrap();
        value["delivery"]["unexpected"] = true.into();
        assert!(serde_json::from_value::<InputReplyHandoff>(value).is_err());
    }

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
            kind: InputReplyKind::UserInput,
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

    #[test]
    fn progress_events_share_real_anchor_without_changing_old_user_input_encoding() {
        let original = serde_json::json!({"input_message_id":"original","input_text":"继续查", "event_id":"input-event", "reply_text":"我在查", "delivery":{"state":"sending"}});
        let handoff: InputReplyHandoff = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(handoff.kind, InputReplyKind::UserInput);
        assert_eq!(serde_json::to_value(&handoff).unwrap(), original);
        let first = InputReplyHandoff {
            kind: InputReplyKind::Progress,
            event_id: "progress-1".into(),
            ..handoff.clone()
        };
        let second = InputReplyHandoff {
            event_id: "progress-2".into(),
            ..first.clone()
        };
        validate_input_reply_handoffs(&[handoff.clone(), first.clone(), second]).unwrap();
        assert!(validate_input_reply_handoffs(&[first.clone(), first]).is_err());
        assert!(
            validate_input_reply_handoffs(&[
                handoff.clone(),
                InputReplyHandoff {
                    event_id: "other-input-event".into(),
                    ..handoff
                }
            ])
            .is_err()
        );
    }

    #[test]
    fn input_reply_delivery_rejects_unknown_persisted_fields_in_every_state() {
        for state in ["sending", "sent", "unknown", "failed", "not_sent"] {
            let mut valid = serde_json::json!({"state":state});
            if state == "sent" {
                valid["platform_message_id"] = "platform-1".into();
            }
            assert!(serde_json::from_value::<InputReplyDelivery>(valid.clone()).is_ok());
            valid["unexpected"] = true.into();
            assert!(serde_json::from_value::<InputReplyDelivery>(valid).is_err());
        }
    }
}
