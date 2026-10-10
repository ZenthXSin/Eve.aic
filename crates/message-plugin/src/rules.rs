use eve_message_api::*;
use eve_message_diagnostics::RelationObservationGuard;
use std::sync::Arc;
pub struct RulesJudge;
/// 只识别明确标签，避免从任意聊天猜测取消/更正。标签答复由路由器绑定最近待澄清原文。
pub(crate) fn classification_reply(text: &str) -> Option<MessageIntent> {
    match text.trim().trim_end_matches(['。', '.', '！', '!']) {
        "补充" | "这是补充" | "这是补充，继续原任务" => {
            Some(MessageIntent::Supplement)
        }
        "纠正" | "这是纠正" => Some(MessageIntent::Correction),
        "澄清答复" | "这是澄清答复" => Some(MessageIntent::Answer),
        "新任务" | "这是新任务" => Some(MessageIntent::NewTask),
        _ => None,
    }
}
fn natural(text: &str) -> Option<(MessageIntent, Option<usize>)> {
    if let Some(intent) = classification_reply(text) {
        return Some((intent, Some(0)));
    }
    for (prefix, intent) in [
        ("补充：", MessageIntent::Supplement),
        ("补充:", MessageIntent::Supplement),
        ("这是补充：", MessageIntent::Supplement),
        ("我补充一下：", MessageIntent::Supplement),
        ("纠正：", MessageIntent::Correction),
        ("纠正:", MessageIntent::Correction),
        ("这是纠正：", MessageIntent::Correction),
        ("前面说错了：", MessageIntent::Correction),
        ("答复：", MessageIntent::Answer),
        ("这是澄清答复：", MessageIntent::Answer),
        ("新任务：", MessageIntent::NewTask),
        ("新任务:", MessageIntent::NewTask),
        ("先停下，改做：", MessageIntent::NewTask),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some((intent, Some(text.len() - rest.len())));
            }
        }
    }
    match text {
        "继续原任务" | "继续" => Some((MessageIntent::Continue, None)),
        "取消当前任务" | "停止当前任务" => Some((MessageIntent::Cancel, None)),
        _ => None,
    }
}
impl RelationJudge for RulesJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        self.judge_observed(input, Arc::new(DiscardRelationObservations))
    }
    fn judge_observed(
        &self,
        input: RelationInput,
        observer: Arc<dyn RelationObserver>,
    ) -> RelationFuture<'_> {
        Box::pin(async move {
            observe_relation(observer.as_ref(), RelationObservation::Supported);
            let guard = RelationObservationGuard::stage(observer, RelationStage::Rules);
            let result: Result<RelationDecision, RelationError> = async {
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
                        _ if natural(trimmed).is_some() => {
                            let (intent, start) = natural(trimmed).expect("checked natural marker");
                            let base = offset + line.len() - line.trim_start().len();
                            IntentPart {
                                intent,
                                confidence: 100,
                                span: start.map(|start| TextSpan {
                                    start: base + start,
                                    end: base + trimmed.len(),
                                }),
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
                    explanation:
                        "根据逐行明确命令或明确中文关系标记判断；其他文字由可选语义判断处理。"
                            .into(),
                };
                decision.validate(&input)?;
                Ok(decision)
            }
            .await;
            guard.finish(match &result {
                Ok(_) => RelationOutcome::Completed,
                Err(error) => (*error).into(),
            });
            result
        })
    }
}
