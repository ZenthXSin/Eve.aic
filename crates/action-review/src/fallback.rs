use eve_developer_api::{
    ActionClassification, ActionReviewClassifier, ActionReviewFuture, ActionReviewMode,
    ActionReviewObserver, ActionReviewRequest, ActionReviewRoute, ActionReviewUncertainty,
    ActionReviewVerdict, ActionReviewer,
};
use std::{
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

/// 宿主更早的工具、任务与取消期限继续优先，不因回退重新计时。
#[derive(Clone, Copy, Debug)]
pub struct ReviewChainLimits {
    pub classifier_timeout: Duration,
    pub total_timeout: Duration,
    pub confidence_threshold: u8,
}
impl Default for ReviewChainLimits {
    fn default() -> Self {
        Self {
            classifier_timeout: Duration::from_secs(10),
            // 兼容现有 LLM 最大 600 秒及 SSE 一秒收尾，不缩短其自身期限。
            total_timeout: Duration::from_secs(611),
            confidence_threshold: 80,
        }
    }
}

/// 只组合两个可替换契约；不增加任何送审操作，也不把拒绝转交后备寻找许可。
pub struct FallbackActionReviewer {
    classifier: Option<Arc<dyn ActionReviewClassifier>>,
    fallback: Option<Arc<dyn ActionReviewer>>,
    limits: ReviewChainLimits,
    observer: Option<Arc<dyn ActionReviewObserver>>,
}
impl FallbackActionReviewer {
    pub fn new(
        classifier: Option<Arc<dyn ActionReviewClassifier>>,
        fallback: Option<Arc<dyn ActionReviewer>>,
    ) -> Self {
        Self {
            classifier,
            fallback,
            limits: ReviewChainLimits::default(),
            observer: None,
        }
    }

    pub fn with_limits(mut self, limits: ReviewChainLimits) -> Result<Self, &'static str> {
        if limits.classifier_timeout.is_zero()
            || limits.total_timeout < Duration::from_millis(2)
            || limits.total_timeout > Duration::from_secs(611)
            || !(1..=100).contains(&limits.confidence_threshold)
        {
            return Err("审查链期限或路由阈值无效");
        }
        self.limits = limits;
        Ok(self)
    }

    pub fn with_observer(mut self, observer: Arc<dyn ActionReviewObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    fn route(&self, mode: ActionReviewMode, route: ActionReviewRoute) {
        if let Some(observer) = &self.observer {
            let _ = catch_unwind(AssertUnwindSafe(|| observer.route(mode, route)));
        }
    }

    fn run(&self, mode: ActionReviewMode, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        Box::pin(async move {
            request.validate_capacity()?;
            let started = Instant::now();
            let deadline = started + self.limits.total_timeout;
            let classifier_deadline = started
                + self
                    .limits
                    .classifier_timeout
                    .min(self.limits.total_timeout / 2);
            let reason = if let Some(classifier) = &self.classifier {
                let result = timeout_at(
                    classifier_deadline,
                    contain(async { classifier.classify(mode, request.clone()).await }),
                )
                .await;
                match result {
                    Err(_) => ActionReviewUncertainty::Timeout,
                    Ok(Err(())) => ActionReviewUncertainty::Panicked,
                    Ok(Ok(ActionClassification::Uncertain(reason))) => reason,
                    Ok(Ok(ActionClassification::Decided {
                        verdict,
                        confidence,
                    })) => {
                        if confidence > 100 || !valid_verdict(&verdict) {
                            ActionReviewUncertainty::InvalidResponse
                        } else if confidence < self.limits.confidence_threshold {
                            ActionReviewUncertainty::LowConfidence
                        } else {
                            self.route(mode, ActionReviewRoute::Classifier { confidence });
                            return Ok(verdict);
                        }
                    }
                }
            } else {
                ActionReviewUncertainty::Unavailable
            };
            self.route(mode, ActionReviewRoute::Fallback { reason });
            let Some(fallback) = &self.fallback else {
                return Err("前置审查无法确定，且后备审查模型不可用；未执行动作".into());
            };
            if Instant::now() >= deadline {
                return Err("审查总期限已耗尽；未执行动作".into());
            }
            // 原对象完整移交，模型会话不变，不能把 Jev 的分类或正文变成新的权限证据。
            timeout_at(
                deadline,
                contain(async {
                    match mode {
                        ActionReviewMode::IdentityBoundary => fallback.review(request).await,
                        ActionReviewMode::SystemSafety => {
                            fallback.review_system_safety(request).await
                        }
                    }
                }),
            )
            .await
            .map_err(|_| "审查总期限超时；未执行动作".to_string())?
            .map_err(|()| "后备审查异常；未执行动作".to_string())?
        })
    }
}

fn valid_verdict(verdict: &ActionReviewVerdict) -> bool {
    !verdict.rationale.trim().is_empty()
        && verdict.rationale.chars().count() <= 300
        && !(verdict.allow && verdict.risk >= eve_developer_api::ReviewRisk::High)
}

impl ActionReviewer for FallbackActionReviewer {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(ActionReviewMode::IdentityBoundary, request)
    }

    fn review_system_safety(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(ActionReviewMode::SystemSafety, request)
    }
}

async fn contain<F: Future>(future: F) -> Result<F::Output, ()> {
    let mut future = Box::pin(future);
    poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Ready(result)) => Poll::Ready(Ok(result)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        },
    )
    .await
}

#[cfg(test)]
mod tests;
