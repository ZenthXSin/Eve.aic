use crate::RulesJudge;
use eve_config_api::ConfigService;
use eve_message_api::*;
use eve_message_diagnostics::RelationObservationGuard;
use std::{
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

/// 明确规则 → 可选判断器 → 内置判断适配器；最终动作仍由路由器校验和执行。
pub struct FallbackJudge {
    config: Arc<dyn ConfigService>,
    primary: Option<Arc<dyn RelationJudge>>,
    fallback: Arc<dyn RelationJudge>,
    experience: Option<Arc<dyn RelationExperienceStore>>,
}

impl FallbackJudge {
    pub fn new(
        config: Arc<dyn ConfigService>,
        primary: Option<Arc<dyn RelationJudge>>,
        fallback: Arc<dyn RelationJudge>,
    ) -> Self {
        Self {
            config,
            primary,
            fallback,
            experience: None,
        }
    }
    pub fn with_experience(mut self, store: Option<Arc<dyn RelationExperienceStore>>) -> Self {
        self.experience = store;
        self
    }
    fn confirmation(&self, input: &RelationInput, decision: &RelationDecision) {
        let Some(store) = &self.experience else {
            return;
        };
        let Ok(snapshot) = self.config.snapshot(MESSAGE_NAMESPACE, 1) else {
            return;
        };
        if !MessageConfig::try_from(&snapshot).is_ok_and(|c| c.experience_enabled) {
            return;
        }
        let Some(intent) = crate::rules::classification_reply(&input.message.text) else {
            return;
        };
        let Some(question) = &input.clarification else {
            return;
        };
        if !matches!(
            intent,
            MessageIntent::Supplement | MessageIntent::Correction | MessageIntent::NewTask
        ) {
            return;
        }
        let mut source = input.clone();
        source.message.text = question.source_text.clone();
        let mut learned = decision.clone();
        learned.parts = vec![IntentPart {
            intent,
            confidence: 100,
            span: Some(TextSpan {
                start: 0,
                end: source.message.text.len(),
            }),
        }];
        learned.explanation = "用户明确指定当前未决原话与任务的关系".into();
        let _ = store.remember(
            &source,
            &learned,
            RelationExperienceSource::UserClarification,
        );
    }
}

impl RelationJudge for FallbackJudge {
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
            let rule = RulesJudge
                .judge_observed(input.clone(), observer.clone())
                .await?;
            // 格式错误或混合显式命令不会交给模型重新解释。
            if !rule
                .parts
                .iter()
                .any(|part| part.intent == MessageIntent::Ambiguous)
                || input
                    .message
                    .text
                    .lines()
                    .any(|line| line.trim_start().starts_with('/'))
            {
                self.confirmation(&input, &rule);
                return Ok(rule);
            }
            let request = self
                .config
                .begin_request(MESSAGE_NAMESPACE, 1)
                .map_err(|_| RelationError::Unavailable)?;
            let initial = MessageConfig::try_from(request.initial())
                .map_err(|_| RelationError::Unavailable)?;
            let started = Instant::now();
            let budget = Duration::from_millis(initial.judge_timeout_ms);
            let deadline = started + budget;
            let examples = if initial.experience_enabled {
                self.experience
                    .as_ref()
                    .and_then(|s| s.recall(&input).ok())
                    .unwrap_or_default()
            } else {
                vec![]
            };
            if let Some(primary) = &self.primary {
                let guard =
                    RelationObservationGuard::stage(observer.clone(), RelationStage::Auxiliary);
                let result = timeout_at(
                    started + budget / 2,
                    contain(async {
                        primary
                            .judge_with_examples(input.clone(), examples.clone(), observer.clone())
                            .await
                    }),
                )
                .await;
                let reason = match result {
                    Err(_) => {
                        guard.finish(RelationOutcome::Timeout);
                        FallbackReason::Timeout
                    }
                    Ok(Err(error)) => {
                        guard.finish(error.into());
                        error.into()
                    }
                    Ok(Ok(decision)) => {
                        guard.finish(RelationOutcome::Completed);
                        let current = self
                            .config
                            .read_request(&request)
                            .map_err(|_| RelationError::Unavailable)?;
                        let settings = MessageConfig::try_from(&current)
                            .map_err(|_| RelationError::Unavailable)?;
                        if decision.validate(&input).is_err() {
                            FallbackReason::InvalidDecision
                        } else if decision
                            .parts
                            .iter()
                            .any(|part| part.intent == MessageIntent::Ambiguous)
                        {
                            FallbackReason::Ambiguous
                        } else if decision
                            .parts
                            .iter()
                            .any(|part| part.confidence < settings.confidence_threshold)
                        {
                            FallbackReason::LowConfidence
                        } else {
                            return Ok(decision);
                        }
                    }
                };
                observe_relation(observer.as_ref(), RelationObservation::Fallback { reason });
            }
            // Primary 模式直接到此处，不报告一个不存在的失败回退。
            let guard = RelationObservationGuard::stage(observer.clone(), RelationStage::Primary);
            let result = timeout_at(
                deadline,
                contain(async {
                    self.fallback
                        .judge_with_examples(input.clone(), examples, observer)
                        .await
                }),
            )
            .await;
            let result = result
                .map_err(|_| RelationError::Timeout)
                .and_then(|result| result)
                .and_then(|decision| {
                    decision.validate(&input)?;
                    Ok(decision)
                });
            guard.finish(match &result {
                Ok(_) => RelationOutcome::Completed,
                Err(error) => (*error).into(),
            });
            if let Ok(decision) = &result
                && initial.experience_enabled
                && decision
                    .parts
                    .iter()
                    .all(|p| p.confidence >= initial.confidence_threshold)
                && let Some(store) = &self.experience
            {
                let _ = store.remember(&input, decision, RelationExperienceSource::PrimaryModel);
            }
            result
        })
    }
}

async fn contain<F: Future<Output = Result<RelationDecision, RelationError>>>(
    future: F,
) -> Result<RelationDecision, RelationError> {
    let mut future = Box::pin(future);
    poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Ready(result)) => Poll::Ready(result),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(RelationError::Panicked)),
        },
    )
    .await
}
