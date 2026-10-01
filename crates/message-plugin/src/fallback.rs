use eve_config_api::ConfigService;
use crate::RulesJudge;
use eve_message_api::*;
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
        }
    }
}

impl RelationJudge for FallbackJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        Box::pin(async move {
            let rule = RulesJudge.judge(input.clone()).await?;
            // 不把格式错误或混合文字的显式命令交给模型重新解释。
            if !rule.parts.iter().any(|p| p.intent == MessageIntent::Ambiguous)
                || input.message.text.lines().any(|line| line.trim_start().starts_with('/'))
            {
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
            if let Some(primary) = &self.primary {
                // 最多使用总期限的一半，为后续内置判断留出时间。
                let result = timeout_at(
                    started + budget / 2,
                    contain(async { primary.judge(input.clone()).await }),
                )
                .await;
                if let Ok(Ok(decision)) = result {
                    let current = self
                        .config
                        .read_request(&request)
                        .map_err(|_| RelationError::Unavailable)?;
                    let settings = MessageConfig::try_from(&current)
                        .map_err(|_| RelationError::Unavailable)?;
                    if decision.validate(&input).is_ok()
                        && decision.parts.iter().all(|part| {
                            part.confidence >= settings.confidence_threshold
                                && part.intent != MessageIntent::Ambiguous
                        })
                    {
                        return Ok(decision);
                    }
                }
            }
            let decision = timeout_at(
                deadline,
                contain(async { self.fallback.judge(input.clone()).await }),
            )
            .await
            .map_err(|_| RelationError::Timeout)??;
            decision.validate(&input)?;
            Ok(decision)
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
