//! 健康记录只观察结局：成功、分类的失败与配置错误照实记录，取消不记录，请求与结果原样传递。
use eve_llm_api::*;
use eve_model_health::{HealthBook, MAX_ROLES, ObservedResolver};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// 按脚本依次返回结果的 Provider；记录收到的请求数。
struct Scripted {
    results: Mutex<Vec<Result<ModelResponse, LlmError>>>,
    calls: AtomicUsize,
}
impl LlmProvider for Scripted {
    fn complete(&self, _request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.results.lock().unwrap().remove(0);
        Box::pin(async move { next })
    }
}
struct Fixed(Arc<Scripted>);
impl LlmModelResolver for Fixed {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: Duration::from_secs(1),
        })
    }
}
struct Broken;
impl LlmModelResolver for Broken {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Err(LlmError::Configuration("缺少模型".into()))
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "你好")],
        tools: vec![],
    }
}
fn text(value: &str) -> ModelResponse {
    ModelResponse::Final { text: value.into() }
}
fn payment() -> LlmError {
    LlmError::ProviderFailed(ProviderFailure::http(402).unwrap())
}

#[tokio::test]
async fn outcomes_are_recorded_per_role_without_changing_results() {
    let book = Arc::new(HealthBook::new());
    book.register("dialogue");
    book.register("research");
    let [(dialogue, before), (research, idle)] = book.snapshot().try_into().unwrap();
    assert_eq!(
        (dialogue.as_str(), research.as_str()),
        ("dialogue", "research")
    );
    assert_eq!(before.last_succeeded, None, "登记后、请求前尚未验证");
    assert_eq!(idle.requests, 0);

    let scripted = Arc::new(Scripted {
        results: Mutex::new(vec![
            Err(payment()),
            Err(LlmError::ProviderTimeout),
            Ok(text("好")),
        ]),
        calls: AtomicUsize::new(0),
    });
    let resolver =
        ObservedResolver::new(Arc::new(Fixed(scripted.clone())), "dialogue", book.clone());
    let provider = resolver.resolve().unwrap().provider;
    assert_eq!(provider.complete(request()).await.unwrap_err(), payment());
    let [(_, failing), _] = book.snapshot().try_into().unwrap();
    assert_eq!(
        (
            failing.requests,
            failing.failures,
            failing.consecutive_failures
        ),
        (1, 1, 1)
    );
    assert_eq!(failing.last_succeeded, Some(false));
    assert_eq!(
        failing.last_failure.unwrap().0,
        ModelOutcome::Failed {
            failure: ProviderFailure {
                kind: ProviderFailureKind::PaymentRequired,
                http_status: Some(402)
            }
        }
    );

    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::ProviderTimeout
    );
    assert_eq!(provider.complete(request()).await.unwrap(), text("好"));
    let [(_, recovered), (_, untouched)] = book.snapshot().try_into().unwrap();
    assert_eq!(
        (
            recovered.requests,
            recovered.failures,
            recovered.consecutive_failures
        ),
        (3, 2, 0)
    );
    assert_eq!(recovered.last_succeeded, Some(true));
    assert_eq!(recovered.last_failure.unwrap().0, ModelOutcome::TimedOut);
    assert_eq!(untouched, RoleHealth::default());
    // 每次请求恰好转发一次，没有隐藏重试。
    assert_eq!(scripted.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cancellation_is_not_an_outcome_and_misconfiguration_is() {
    let book = Arc::new(HealthBook::new());
    let scripted = Arc::new(Scripted {
        results: Mutex::new(vec![Err(LlmError::Cancelled)]),
        calls: AtomicUsize::new(0),
    });
    let provider = ObservedResolver::new(Arc::new(Fixed(scripted)), "plan", book.clone())
        .resolve()
        .unwrap()
        .provider;
    assert_eq!(
        provider.complete(request()).await.unwrap_err(),
        LlmError::Cancelled
    );
    assert!(book.snapshot().is_empty(), "取消不记录");

    let broken = ObservedResolver::new(Arc::new(Broken), "plan", book.clone());
    assert!(matches!(broken.resolve(), Err(LlmError::Configuration(_))));
    let [(_, plan)] = book.snapshot().try_into().unwrap();
    assert_eq!(plan.last_failure.unwrap().0, ModelOutcome::Misconfigured);
}

#[test]
fn the_number_of_roles_is_bounded() {
    let book = HealthBook::new();
    for index in 0..MAX_ROLES + 5 {
        book.register(&format!("role-{index:02}"));
        book.record(&format!("extra-{index:02}"), ModelOutcome::Succeeded, 1);
    }
    assert_eq!(book.snapshot().len(), MAX_ROLES);
    // 已登记的用途照常更新。
    book.record("role-00", ModelOutcome::Succeeded, 5);
    let snapshot = book.snapshot();
    let (_, role) = snapshot.iter().find(|(name, _)| name == "role-00").unwrap();
    assert_eq!(role.last_success_at_ms, Some(5));
}

use eve_model_health::RoleHealth;
