//! 一次自然发言的冻结计划；每条先落盘，再独立等待平台回执。
use super::*;
use eve_llm_api::{InputReplyDelivery, InputReplyPart, InputReplyPartDelivery};

pub(super) struct SpeechParts {
    pub event: crate::QqSpeechEvent,
    pub receipt: crate::QqSpeechPartsReceipt,
    pub route: Message,
    pub metadata: eve_training_api::ConversationMetadata,
    pub passive: bool,
    pub foreground: Option<(GenerationKey, u64)>,
    pub source_id: Option<String>,
    pub claim: Option<crate::live::ClaimedInputReply>,
    pub due: tokio::time::Instant,
    pub deferred_finish: Option<Value>,
    pub closed: bool,
}

pub(super) fn prepare(
    live: Option<&crate::LiveDialogue>,
    segmentation: Option<&Segmentation>,
    event: &crate::QqSpeechEvent,
    chosen: &[crate::QqSpeechPart],
) -> PluginResult<crate::QqSpeechPartsReceipt> {
    let scope = eve_llm_api::ContextScope {
        session_id: event.session.session_id.clone(),
        user_id: event.session.user_id.clone(),
    };
    let mut plan = if let Some(live) = live {
        live.prepare_speech_parts(&scope, chosen)?
    } else {
        // 同一偏好实现，也覆盖未启用实时主轮的后回复路径。
        let planner = crate::LiveDialogue::default();
        planner.configure_segmentation(
            segmentation.and_then(|value| value.preferences.clone()),
            segmentation.map_or(crate::QQ_SEGMENT_POLICY, |value| value.policy),
        )?;
        planner.prepare_speech_parts(&scope, chosen)?
    };
    // 首段等待由决定账本 due 处理，不能在实际发送阶段再等一次。
    plan[0].after_ms = 0;
    let parts = plan
        .iter()
        .enumerate()
        .map(|(index, part)| InputReplyPart {
            index,
            text: part.text.clone(),
            delivery: if index == 0 {
                InputReplyPartDelivery::Sending
            } else {
                InputReplyPartDelivery::Planned
            },
        })
        .collect();
    let receipt = crate::QqSpeechPartsReceipt {
        version: 2,
        plan,
        delivery: InputReplyDelivery::Parts { parts },
    };
    receipt.validate()?;
    Ok(receipt)
}

impl SpeechParts {
    fn parts(&self) -> &[InputReplyPart] {
        let InputReplyDelivery::Parts { parts } = &self.receipt.delivery else {
            unreachable!()
        };
        parts
    }
    fn parts_mut(&mut self) -> &mut [InputReplyPart] {
        let InputReplyDelivery::Parts { parts } = &mut self.receipt.delivery else {
            unreachable!()
        };
        parts
    }
    pub fn targets(&self, message: &Message) -> bool {
        self.route.scope == message.scope && self.route.target_id == message.target_id
    }
    pub fn next(&self) -> Option<usize> {
        self.parts().iter().position(|part| {
            matches!(
                part.delivery,
                InputReplyPartDelivery::Sending | InputReplyPartDelivery::Planned
            )
        })
    }
    pub fn frame(&self, index: usize) -> Value {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.event.id.as_bytes())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut frame = json!({"type":if self.passive {"passive_push"} else {"push"}, "version":1,
            "id":format!("speech-parts-{digest}-{index}"), "scope":self.route.scope,
            "target_id":self.route.target_id,"text":self.receipt.plan[index].text,
            "based_on_message_id":self.event.anchor,"sender_id":self.route.user_id});
        if self.foreground.is_some() {
            frame["foreground"] = json!(true);
        }
        if self.route.scope == "group" {
            frame["mention_user_id"] = json!(self.route.user_id);
        }
        frame
    }
    pub fn sending(&mut self, index: usize, speaker: &dyn crate::QqSpeech) -> PluginResult<()> {
        self.parts_mut()[index].delivery = InputReplyPartDelivery::Sending;
        self.save(speaker)
    }
    pub fn result(
        &mut self,
        result: &QqOutreachResult,
        speaker: &dyn crate::QqSpeech,
    ) -> PluginResult<()> {
        let index = self
            .parts()
            .iter()
            .position(|part| part.delivery == InputReplyPartDelivery::Sending)
            .ok_or_else(|| failure("自主分条缺少在途部分"))?;
        self.parts_mut()[index].delivery = match input_reply_delivery(result) {
            InputReplyDelivery::Sent {
                platform_message_id,
            } => InputReplyPartDelivery::Sent {
                platform_message_id,
            },
            InputReplyDelivery::Unknown => InputReplyPartDelivery::Unknown,
            _ => InputReplyPartDelivery::Failed,
        };
        if !matches!(
            self.parts()[index].delivery,
            InputReplyPartDelivery::Sent { .. }
        ) {
            self.seal_planned();
        }
        self.save(speaker)?;
        if let Some(next) = self.next() {
            self.due = tokio::time::Instant::now()
                + Duration::from_millis(self.receipt.plan[next].after_ms);
        }
        Ok(())
    }
    fn seal_planned(&mut self) {
        for part in self.parts_mut() {
            if part.delivery == InputReplyPartDelivery::Planned {
                part.delivery = InputReplyPartDelivery::NotAttempted;
            }
        }
    }
    /// 在途条目等待真实回执；尚未写进 SDK 的首条明确失败。其余一律封存。
    pub fn stop(&mut self, in_flight: bool, speaker: &dyn crate::QqSpeech) -> PluginResult<()> {
        self.seal_planned();
        if !in_flight {
            for part in self.parts_mut() {
                if part.delivery == InputReplyPartDelivery::Sending {
                    part.delivery = InputReplyPartDelivery::Failed;
                }
            }
        }
        self.save(speaker)
    }
    pub fn save(&self, speaker: &dyn crate::QqSpeech) -> PluginResult<()> {
        speaker.update_parts(&self.event, &self.receipt)?;
        if let Some(claim) = &self.claim {
            claim.update(self.receipt.delivery.clone())?;
        }
        Ok(())
    }
    pub fn aggregate(&self) -> QqOutreachResult {
        if self
            .parts()
            .iter()
            .all(|part| matches!(part.delivery, InputReplyPartDelivery::Sent { .. }))
        {
            let Some(InputReplyPart {
                delivery:
                    InputReplyPartDelivery::Sent {
                        platform_message_id,
                    },
                ..
            }) = self.parts().last()
            else {
                unreachable!()
            };
            QqOutreachResult::Sent {
                platform_message_id: Some(platform_message_id.clone()),
            }
        } else if self.parts().iter().any(|part| {
            matches!(
                part.delivery,
                InputReplyPartDelivery::Sent { .. } | InputReplyPartDelivery::Unknown
            )
        }) {
            QqOutreachResult::Unknown
        } else {
            QqOutreachResult::Failed {
                http_status: None,
                biz_code: None,
            }
        }
    }
}
