use eve_message_api::*;
pub struct RulesJudge;
impl RelationJudge for RulesJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        Box::pin(async move {
            let mut parts = vec![];
            let mut offset = 0;
            for line in input.message.text.split_inclusive('\n') {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    offset += line.len();
                    continue;
                }
                let command_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
                let command = &trimmed[..command_end];
                let payload = trimmed[command_end..].trim();
                let mapping = match command {
                    "/add" => Some((MessageIntent::Supplement, true)),
                    "/correct" => Some((MessageIntent::Correction, true)),
                    "/answer" => Some((MessageIntent::Answer, true)),
                    "/new" => Some((MessageIntent::NewTask, true)),
                    "/cancel" => Some((MessageIntent::Cancel, false)),
                    "/continue" => Some((MessageIntent::Continue, false)),
                    "/unrelated" => Some((MessageIntent::Unrelated, false)),
                    "/pause" => Some((MessageIntent::Pause, false)),
                    "/resume" => Some((MessageIntent::Resume, false)),
                    _ => None,
                };
                let part = match mapping {
                    Some((intent, needs_text)) if needs_text != payload.is_empty() => {
                        let span = needs_text.then(|| {
                            let start = offset + line.len() - line.trim_start().len()
                                + trimmed.len()
                                - payload.len();
                            TextSpan {
                                start,
                                end: start + payload.len(),
                            }
                        });
                        IntentPart {
                            intent,
                            confidence: 100,
                            span,
                        }
                    }
                    _ => IntentPart {
                        intent: MessageIntent::Ambiguous,
                        confidence: 0,
                        span: None,
                    },
                };
                parts.push(part);
                offset += line.len();
            }
            if parts.is_empty()
                || parts.len() > 16
                || parts.iter().any(|p| p.intent == MessageIntent::Ambiguous)
            {
                parts = vec![IntentPart {
                    intent: MessageIntent::Ambiguous,
                    confidence: 0,
                    span: None,
                }];
            }
            let decision = RelationDecision {
                target: input.message.target.clone(),
                message_id: input.message.message_id.clone(),
                parts,
                explanation: "根据逐行明确命令判断；其他文字需要澄清。".into(),
            };
            decision.validate(&input)?;
            Ok(decision)
        })
    }
}
