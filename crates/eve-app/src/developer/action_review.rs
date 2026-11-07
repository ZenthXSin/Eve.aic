//! QQ 与 Web 共用的审查组合；宿主启动前只保存独立凭据，首次需要时解析配置。
use super::REVIEW_USE;
use eve_action_review::{FallbackActionReviewer, ModelActionReviewer};
use eve_developer_api::{
    ActionClassification, ActionClassificationFuture, ActionReviewClassifier, ActionReviewFuture,
    ActionReviewMode, ActionReviewObserver, ActionReviewRequest, ActionReviewRoute,
    ActionReviewUncertainty, ActionReviewVerdict, ActionReviewer,
};
use eve_llm_api::{
    LlmError, LlmModelResolver, ModelHealthObserver, ModelOutcome, ModelSelection, ProviderFailure,
    ProviderFailureKind,
};
use eve_plugin_api::ServiceRegistry;
use serde_json::json;
use std::sync::{Arc, OnceLock};

struct RegistryResolver {
    registry: Arc<dyn ServiceRegistry>,
    key: String,
    resolver: OnceLock<crate::models::CoreModelResolver>,
}
impl LlmModelResolver for RegistryResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        if self.resolver.get().is_none() {
            let settings = crate::qq_tasks::config_service(self.registry.as_ref())
                .map_err(|e| LlmError::Configuration(e.to_string()))?;
            let _ = self.resolver.set(
                crate::models::CoreModelResolver::new(settings, self.key.clone())
                    .with_use(REVIEW_USE),
            );
        }
        self.resolver
            .get()
            .ok_or_else(|| LlmError::Configuration("审查模型不可用".into()))?
            .resolve()
    }
}

struct RegistryClassifier {
    registry: Arc<dyn ServiceRegistry>,
    key: String,
    judge: OnceLock<Arc<crate::qq_message_judge::JevRoleJudge>>,
}
impl RegistryClassifier {
    fn resolve(&self) -> Result<&Arc<crate::qq_message_judge::JevRoleJudge>, ()> {
        if self.judge.get().is_none() {
            let settings =
                crate::qq_tasks::config_service(self.registry.as_ref()).map_err(|_| ())?;
            let selected =
                crate::qq_message_judge::JevRoleJudge::shared(settings, self.key.clone())
                    .map_err(|_| ())?;
            let _ = self.judge.set(selected);
        }
        self.judge.get().ok_or(())
    }
}
impl ActionReviewClassifier for RegistryClassifier {
    fn classify(
        &self,
        mode: ActionReviewMode,
        request: ActionReviewRequest,
    ) -> ActionClassificationFuture<'_> {
        Box::pin(async move {
            match self.resolve() {
                Ok(judge) => judge.classify(mode, request).await,
                Err(()) => ActionClassification::Uncertain(ActionReviewUncertainty::Unavailable),
            }
        })
    }
}

fn outcome(result: &Result<ActionReviewVerdict, String>) -> ModelOutcome {
    match result {
        Ok(_) => ModelOutcome::Succeeded,
        Err(e) if e.contains("超时") => ModelOutcome::TimedOut,
        Err(e) if e.starts_with("审查输出") || e.starts_with("审查理由") => {
            ModelOutcome::InvalidResponse
        }
        Err(_) => ModelOutcome::Failed {
            failure: ProviderFailure::new(ProviderFailureKind::Other),
        },
    }
}

/// 仅实际进入 LLM 后备时更新 review 健康，不把 Jev 结果记为一次主模型调用。
struct ReportedLlm(ModelActionReviewer, Arc<eve_model_health::HealthBook>);
impl ReportedLlm {
    fn run(&self, request: ActionReviewRequest, system_safety: bool) -> ActionReviewFuture<'_> {
        Box::pin(async move {
            let result = if system_safety {
                self.0.review_system_safety(request).await
            } else {
                self.0.review(request).await
            };
            self.1
                .record(REVIEW_USE, outcome(&result), crate::qq_people::now_ms());
            result
        })
    }
}
impl ActionReviewer for ReportedLlm {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(request, false)
    }
    fn review_system_safety(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(request, true)
    }
}

struct RouteLog;
impl ActionReviewObserver for RouteLog {
    fn route(&self, mode: ActionReviewMode, route: ActionReviewRoute) {
        eprintln!(
            "EVE_ACTION_REVIEW_ROUTE {}",
            json!({"mode":mode,"decision":route})
        );
    }
}

/// 每次结局仍写入标准错误，不包含请求正文或提供方错误正文。
struct Reported(FallbackActionReviewer);
impl Reported {
    fn run(&self, request: ActionReviewRequest, system_safety: bool) -> ActionReviewFuture<'_> {
        Box::pin(async move {
            let delegated = request.delegated;
            let result = if system_safety {
                self.0.review_system_safety(request).await
            } else {
                self.0.review(request).await
            };
            let line = match &result {
                Ok(v) => json!({
                    "outcome": if (system_safety && v.permits_system_safety()) || (!system_safety && v.permits()) { "allowed" } else { "denied" },
                    "risk":v.risk,"authorization":v.authorization,"delegated":delegated,"system_safety":system_safety
                }),
                Err(_) => {
                    json!({"outcome":"failed","error":outcome(&result),"delegated":delegated,"system_safety":system_safety})
                }
            };
            eprintln!("EVE_ACTION_REVIEW {line}");
            result
        })
    }
}
impl ActionReviewer for Reported {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(request, false)
    }
    fn review_system_safety(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.run(request, true)
    }
}

pub(crate) fn action_reviewer(
    registry: Arc<dyn ServiceRegistry>,
    health: Arc<eve_model_health::HealthBook>,
) -> Option<Arc<dyn ActionReviewer>> {
    action_reviewer_with_environment(registry, health, |name| std::env::var(name).ok())
}

fn action_reviewer_with_environment(
    registry: Arc<dyn ServiceRegistry>,
    health: Arc<eve_model_health::HealthBook>,
    mut read_environment: impl FnMut(&str) -> Option<String>,
) -> Option<Arc<dyn ActionReviewer>> {
    // 启动时统一关闭两层模型审查；不读凭据、不装配后备，也不生成放行结论。
    if read_environment("EVE_ACTION_REVIEW_ENABLED")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("false"))
    {
        return None;
    }
    action_reviewer_with_keys(
        registry,
        health,
        read_environment("EVE_JEV_API_KEY"),
        read_environment("EVE_OPENAI_API_KEY"),
    )
}

fn action_reviewer_with_keys(
    registry: Arc<dyn ServiceRegistry>,
    health: Arc<eve_model_health::HealthBook>,
    jev_key: Option<String>,
    llm_key: Option<String>,
) -> Option<Arc<dyn ActionReviewer>> {
    let jev_key = jev_key.filter(|key| !key.trim().is_empty());
    let llm_key = llm_key.filter(|key| !key.trim().is_empty());
    if jev_key.is_none() && llm_key.is_none() {
        return None;
    }
    health.register(REVIEW_USE);
    let classifier = jev_key.map(|key| {
        Arc::new(RegistryClassifier {
            registry: registry.clone(),
            key,
            judge: OnceLock::new(),
        }) as Arc<dyn ActionReviewClassifier>
    });
    let fallback = llm_key.map(|key| {
        let resolver = RegistryResolver {
            registry,
            key,
            resolver: OnceLock::new(),
        };
        Arc::new(ReportedLlm(
            ModelActionReviewer::new(Arc::new(resolver)),
            health,
        )) as Arc<dyn ActionReviewer>
    });
    Some(Arc::new(Reported(
        FallbackActionReviewer::new(classifier, fallback).with_observer(Arc::new(RouteLog)),
    )))
}

#[cfg(test)]
mod tests;
