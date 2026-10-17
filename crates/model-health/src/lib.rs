//! 模型健康记录：按宿主登记的用途记录每次实际模型请求的结局，供面板与启动诊断区分
//! “通道已连接”与“模型能完成请求”。
//!
//! 只在内存中保存计数、最近一次成功的时间与最近一次失败的固定类别；不保存请求或回复正文、
//! 供应商错误正文与凭据。进程重启后清零，不补写、不推断重启前的状态。
//! 包装层只观察结局，不改变请求、不重试、不切换模型。
use eve_llm_api::{
    LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelHealthObserver, ModelOutcome,
    ModelRequest, ModelResponse, ModelSelection, ModelTextSink,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// 至多记录的用途数；超过时新用途不再记录，已登记的照常更新。
pub const MAX_ROLES: usize = 32;

/// 一个用途的健康记录。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RoleHealth {
    /// 有结局的请求数（取消的不计）。
    pub requests: u64,
    pub failures: u64,
    /// 最近连续失败的次数；成功一次后归零。
    pub consecutive_failures: u32,
    pub last_success_at_ms: Option<u64>,
    /// 最近一次失败的结局与时间。
    pub last_failure: Option<(ModelOutcome, u64)>,
    /// 最近一次结局是否成功；没有任何结局时为 None。按记录顺序，不依赖时钟。
    pub last_succeeded: Option<bool>,
}

/// 进程内的健康记录；先登记的用途在没有请求前也会出现在快照中。
#[derive(Default)]
pub struct HealthBook {
    roles: Mutex<BTreeMap<String, RoleHealth>>,
}

impl HealthBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个用途，使它在还没有请求时也显示为“尚未验证”。
    pub fn register(&self, role: &str) {
        if let Ok(mut roles) = self.roles.lock()
            && (roles.contains_key(role) || roles.len() < MAX_ROLES)
        {
            roles.entry(role.to_string()).or_default();
        }
    }

    /// 按用途名排序的快照；记录锁失效时为空。
    pub fn snapshot(&self) -> Vec<(String, RoleHealth)> {
        self.roles
            .lock()
            .map(|roles| {
                roles
                    .iter()
                    .map(|(role, health)| (role.clone(), health.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ModelHealthObserver for HealthBook {
    fn record(&self, role: &str, outcome: ModelOutcome, at_ms: u64) {
        let Ok(mut roles) = self.roles.lock() else {
            return;
        };
        if !roles.contains_key(role) && roles.len() >= MAX_ROLES {
            return;
        }
        let health = roles.entry(role.to_string()).or_default();
        health.requests = health.requests.saturating_add(1);
        match outcome {
            ModelOutcome::Succeeded => {
                health.consecutive_failures = 0;
                health.last_success_at_ms = Some(at_ms);
                health.last_succeeded = Some(true);
            }
            failure => {
                health.last_succeeded = Some(false);
                health.failures = health.failures.saturating_add(1);
                health.consecutive_failures = health.consecutive_failures.saturating_add(1);
                health.last_failure = Some((failure, at_ms));
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// 包装解析器：解析出的 Provider 每次请求结束后记录结局；配置无法解析时记为配置错误。
pub struct ObservedResolver {
    inner: Arc<dyn LlmModelResolver>,
    role: String,
    observer: Arc<dyn ModelHealthObserver>,
}

impl ObservedResolver {
    pub fn new(
        inner: Arc<dyn LlmModelResolver>,
        role: impl Into<String>,
        observer: Arc<dyn ModelHealthObserver>,
    ) -> Self {
        Self {
            inner,
            role: role.into(),
            observer,
        }
    }
}

impl LlmModelResolver for ObservedResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        match self.inner.resolve() {
            Ok(selection) => Ok(ModelSelection {
                provider: Arc::new(ObservedProvider {
                    inner: selection.provider,
                    role: self.role.clone(),
                    observer: self.observer.clone(),
                }),
                provider_timeout: selection.provider_timeout,
            }),
            Err(error) => {
                if let Some(outcome) = ModelOutcome::from_error(&error) {
                    self.observer.record(&self.role, outcome, now_ms());
                }
                Err(error)
            }
        }
    }
}

struct ObservedProvider {
    inner: Arc<dyn LlmProvider>,
    role: String,
    observer: Arc<dyn ModelHealthObserver>,
}

impl ObservedProvider {
    /// 丢弃的 Future（取消）不记录。
    fn observe(&self, result: &Result<ModelResponse, LlmError>) {
        let outcome = match result {
            Ok(_) => Some(ModelOutcome::Succeeded),
            Err(error) => ModelOutcome::from_error(error),
        };
        if let Some(outcome) = outcome {
            self.observer.record(&self.role, outcome, now_ms());
        }
    }
}

impl LlmProvider for ObservedProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let result = self.inner.complete(request).await;
            self.observe(&result);
            result
        })
    }
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let result = self.inner.complete_json(request).await;
            self.observe(&result);
            result
        })
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            let result = self.inner.stream(request, sink).await;
            self.observe(&result);
            result
        })
    }
}
