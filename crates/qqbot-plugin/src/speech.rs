//! Eve 对宿主事实的发言决定。事实、决定和平台投递分别记录，内部结束不是消息。
use eve_llm_api::LlmFuture;
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 来自可信适配器的事实版本；正文没有发送含义，模型不能改变来源或收件人。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqSpeechEvent {
    pub id: String,
    pub session: SessionKey,
    pub anchor: String,
    pub facts: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QqSpeechDecision {
    Speak {
        text: String,
        after_ms: u64,
    },
    SpeakParts {
        parts: Vec<QqSpeechPart>,
        after_ms: u64,
    },
    Wait {
        after_ms: u64,
    },
    Silence {},
}

impl Serialize for QqSpeechDecision {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Speak { text, after_ms } => {
                serde_json::json!({"action":"speak","text":text,"after_ms":after_ms})
            }
            Self::SpeakParts { parts, after_ms } => {
                serde_json::json!({"action":"speak","parts":parts,"after_ms":after_ms})
            }
            Self::Wait { after_ms } => serde_json::json!({"action":"wait","after_ms":after_ms}),
            Self::Silence {} => serde_json::json!({"action":"silence"}),
        };
        value.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for QqSpeechDecision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Action,
            Text,
            Parts,
            AfterMs,
        }
        struct DecisionVisitor;
        impl<'de> serde::de::Visitor<'de> for DecisionVisitor {
            type Value = QqSpeechDecision;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a strict speech decision object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                use serde::de::Error;
                let mut action: Option<String> = None;
                let mut text: Option<String> = None;
                let mut parts: Option<Vec<QqSpeechPart>> = None;
                let mut after_ms: Option<u64> = None;
                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::Action => {
                            if action.is_some() {
                                return Err(M::Error::duplicate_field("action"));
                            }
                            action = Some(map.next_value()?);
                        }
                        Field::Text => {
                            if text.is_some() {
                                return Err(M::Error::duplicate_field("text"));
                            }
                            text = Some(map.next_value()?);
                        }
                        Field::Parts => {
                            if parts.is_some() {
                                return Err(M::Error::duplicate_field("parts"));
                            }
                            parts = Some(map.next_value()?);
                        }
                        Field::AfterMs => {
                            if after_ms.is_some() {
                                return Err(M::Error::duplicate_field("after_ms"));
                            }
                            after_ms = Some(map.next_value()?);
                        }
                    }
                }
                let action = action.ok_or_else(|| M::Error::missing_field("action"))?;
                match action.as_str() {
                    "speak" => {
                        let after_ms =
                            after_ms.ok_or_else(|| M::Error::missing_field("after_ms"))?;
                        match (text, parts) {
                            (Some(text), None) => Ok(QqSpeechDecision::Speak { text, after_ms }),
                            (None, Some(parts)) => {
                                Ok(QqSpeechDecision::SpeakParts { parts, after_ms })
                            }
                            _ => Err(M::Error::custom(
                                "speak requires exactly one of text or parts",
                            )),
                        }
                    }
                    "wait" if text.is_none() && parts.is_none() => Ok(QqSpeechDecision::Wait {
                        after_ms: after_ms.ok_or_else(|| M::Error::missing_field("after_ms"))?,
                    }),
                    "silence" if text.is_none() && parts.is_none() && after_ms.is_none() => {
                        Ok(QqSpeechDecision::Silence {})
                    }
                    "wait" | "silence" => {
                        Err(M::Error::custom("fields do not match speech action"))
                    }
                    _ => Err(M::Error::unknown_variant(
                        &action,
                        &["speak", "wait", "silence"],
                    )),
                }
            }
        }
        deserializer.deserialize_map(DecisionVisitor)
    }
}

/// 模型明确选择的消息边界；停顿不是宿主按字数生成的打字模拟。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqSpeechPart {
    pub text: String,
    #[serde(default)]
    pub after_ms: u64,
}

/// 冻结的实际发送计划和逐条凭据；版本 2 不复用单条 receipt 的含义。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqSpeechPartsReceipt {
    pub version: u32,
    pub plan: Vec<QqSpeechPart>,
    pub delivery: eve_llm_api::InputReplyDelivery,
}
impl QqSpeechPartsReceipt {
    pub fn text(&self) -> String {
        self.plan
            .iter()
            .map(|part| part.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    pub fn validate(&self) -> PluginResult<()> {
        let invalid = || eve_plugin_api::PluginError::State("自主分条回执无效".into());
        if self.version != 2
            || self.plan.is_empty()
            || self.plan.len() > 5
            || self.plan.iter().any(|part| {
                part.after_ms > 5000 || part.text.trim().is_empty() || part.text.trim() != part.text
            })
        {
            return Err(invalid());
        }
        let eve_llm_api::InputReplyDelivery::Parts { parts } = &self.delivery else {
            return Err(invalid());
        };
        if parts.len() != self.plan.len()
            || parts
                .iter()
                .zip(&self.plan)
                .any(|(part, plan)| part.text != plan.text)
        {
            return Err(invalid());
        }
        self.delivery
            .validate_parts(&self.text())
            .map_err(|_| invalid())
    }
}

/// `consider` 必须先保存决定，不能 claim 来源或执行发送。
/// `None` 表示已处理或还没到复审时点；绝不表示发送成功。
pub trait QqSpeech: Send + Sync {
    fn enqueue(&self, event: QqSpeechEvent) -> PluginResult<()>;
    fn pending(&self) -> PluginResult<Vec<QqSpeechEvent>>;
    /// 来源暂不可用或路由尚未就绪；让出队列，不消耗发送尝试。
    fn postpone(&self, event: &QqSpeechEvent, after_ms: u64) -> PluginResult<()>;
    /// 事实版本已经失效；原决定保留在账本中，不再进入待发队列。
    fn retire(&self, event: &QqSpeechEvent) -> PluginResult<()>;
    fn silenced(&self, event: &QqSpeechEvent) -> PluginResult<bool>;
    /// future 可能在通道停止或事实失效时被丢弃；实现须恢复仍在审视的记录，
    /// 不能覆盖已失效、已决定或已交给发送层的状态，也不能触发投递。
    fn consider(&self, event: QqSpeechEvent) -> LlmFuture<'_, Option<QqSpeechDecision>>;
    fn dispatched(&self, event: &QqSpeechEvent) -> PluginResult<()>;
    fn delivered(&self, event: &QqSpeechEvent, result: super::QqOutreachResult)
    -> PluginResult<()>;
    /// 冻结全部实际部分；第一条也须先登记 Sending，不能只保存首条。
    fn dispatched_parts(
        &self,
        _event: &QqSpeechEvent,
        _receipt: &QqSpeechPartsReceipt,
    ) -> PluginResult<()> {
        Err(eve_plugin_api::PluginError::State(
            "发言服务未实现分条持久化".into(),
        ))
    }
    /// 单调保存全部部分的快照；终态不得回到 waiting 或重放前缀。
    fn update_parts(
        &self,
        _event: &QqSpeechEvent,
        _receipt: &QqSpeechPartsReceipt,
    ) -> PluginResult<()> {
        Err(eve_plugin_api::PluginError::State(
            "发言服务未实现分条回执".into(),
        ))
    }
    /// 导入前台明确表达的实际回执。重复导入必须幂等，不能触发发送。
    fn record_expression(
        &self,
        event: QqSpeechEvent,
        text: &str,
        result: super::QqOutreachResult,
    ) -> PluginResult<()>;
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_parts_wait_and_silence_keep_their_public_wire_shapes() {
        let cases = [
            (
                QqSpeechDecision::Speak {
                    text: "已经核实".into(),
                    after_ms: 400,
                },
                json!({"action":"speak","text":"已经核实","after_ms":400}),
            ),
            (
                QqSpeechDecision::SpeakParts {
                    parts: vec![QqSpeechPart {
                        text: "先说这一项".into(),
                        after_ms: 20,
                    }],
                    after_ms: 400,
                },
                json!({"action":"speak","parts":[{"text":"先说这一项","after_ms":20}],"after_ms":400}),
            ),
            (
                QqSpeechDecision::Wait { after_ms: 1000 },
                json!({"action":"wait","after_ms":1000}),
            ),
            (QqSpeechDecision::Silence {}, json!({"action":"silence"})),
        ];
        for (decision, wire) in cases {
            assert_eq!(serde_json::to_value(&decision).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<QqSpeechDecision>(wire).unwrap(),
                decision
            );
        }
        assert_eq!(
            serde_json::from_str::<QqSpeechDecision>(
                r#"{"parts":[{"text":"先说这一项"}],"after_ms":0,"action":"speak"}"#
            )
            .unwrap(),
            QqSpeechDecision::SpeakParts {
                parts: vec![QqSpeechPart {
                    text: "先说这一项".into(),
                    after_ms: 0
                }],
                after_ms: 0,
            },
        );
    }

    #[test]
    fn speech_decision_rejects_mixed_extra_missing_and_null_fields() {
        for wire in [
            r#"{"action":"speak","text":"x","parts":[{"text":"y"}],"after_ms":0}"#,
            r#"{"action":"speak","after_ms":0}"#,
            r#"{"action":"speak","text":"x"}"#,
            r#"{"action":"speak","parts":[{"text":"x"}]}"#,
            r#"{"action":"speak","text":"x","after_ms":0,"extra":true}"#,
            r#"{"action":"speak","parts":[{"text":"x","extra":true}],"after_ms":0}"#,
            r#"{"action":"speak","text":null,"after_ms":0}"#,
            r#"{"action":"speak","parts":null,"after_ms":0}"#,
            r#"{"action":"speak","text":"x","parts":null,"after_ms":0}"#,
            r#"{"action":"speak","text":"x","after_ms":null}"#,
            r#"{"action":"wait"}"#,
            r#"{"action":"wait","after_ms":1000,"text":"x"}"#,
            r#"{"action":"wait","after_ms":1000,"parts":[]}"#,
            r#"{"action":"wait","after_ms":1000,"extra":true}"#,
            r#"{"action":"silence","text":"x"}"#,
            r#"{"action":"silence","parts":[]}"#,
            r#"{"action":"silence","after_ms":0}"#,
            r#"{"action":"silence","extra":true}"#,
            r#"{"action":"speak_parts","parts":[{"text":"x"}],"after_ms":0}"#,
            r#"{"text":"x","after_ms":0}"#,
            r#"["speak","x",0]"#,
        ] {
            assert!(
                serde_json::from_str::<QqSpeechDecision>(wire).is_err(),
                "accepted {wire}"
            );
        }
    }

    #[test]
    fn duplicate_fields_are_rejected_before_they_can_be_collapsed() {
        for wire in [
            r#"{"action":"speak","action":"speak","text":"x","after_ms":0}"#,
            r#"{"action":"speak","text":"x","text":"x","after_ms":0}"#,
            r#"{"action":"speak","parts":[],"parts":[],"after_ms":0}"#,
            r#"{"action":"speak","text":"x","after_ms":0,"after_ms":0}"#,
            r#"{"action":"speak","parts":[{"text":"x","text":"x"}],"after_ms":0}"#,
            r#"{"action":"speak","parts":[{"text":"x","after_ms":0,"after_ms":0}],"after_ms":0}"#,
            r#"{"action":"wait","after_ms":1000,"after_ms":1000}"#,
            r#"{"action":"silence","action":"silence"}"#,
        ] {
            assert!(
                serde_json::from_str::<QqSpeechDecision>(wire).is_err(),
                "accepted {wire}"
            );
        }
    }
}
