//! 自主运行预算的持久用量账本，以及按用途记账的模型解析器：每次后台模型请求前先持久化计数。
mod store;
mod strict_json;

use eve_budget_api::*;
use eve_llm_api::{
    LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse,
    ModelSelection, ModelTextSink,
};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use store::StoredBudget;

/// 预算句柄只交给可信宿主；不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct BudgetController {
    active: Arc<Mutex<Option<Arc<StoredBudget>>>>,
}
impl BudgetController {
    fn service(&self) -> BudgetResult<Arc<StoredBudget>> {
        self.active
            .lock()
            .map_err(|_| BudgetError::Unavailable)?
            .clone()
            .ok_or(BudgetError::Unavailable)
    }
}
impl AutonomyBudget for BudgetController {
    fn snapshot(&self, now_ms: u64) -> BudgetResult<BudgetSnapshot> {
        self.service()?.snapshot(now_ms)
    }
    fn reserve(&self, category: &str, count: u32, now_ms: u64) -> BudgetResult<u32> {
        self.service()?.reserve(category, count, now_ms)
    }
}

pub struct BudgetPlugin {
    manifest: PluginManifest,
    policy: BudgetPolicy,
    controller: BudgetController,
}
impl BudgetPlugin {
    pub fn new(policy: BudgetPolicy) -> PluginResult<Self> {
        policy
            .validate()
            .map_err(|error| PluginError::State(error.to_string()))?;
        Ok(Self {
            manifest: PluginManifest::new(BUDGET_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            policy,
            controller: BudgetController::default(),
        })
    }
    pub fn controller(&self) -> BudgetController {
        self.controller.clone()
    }
}
impl Plugin for BudgetPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredBudget::open(context.clone(), self.policy)
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("自主运行预算句柄不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("自主运行预算句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// 包装后台组件使用的模型解析器：每次请求前按用途计数一次，用尽时请求失败且不发出。
pub struct BudgetedResolver {
    inner: Arc<dyn LlmModelResolver>,
    budget: Arc<dyn AutonomyBudget>,
    category: String,
}
impl BudgetedResolver {
    pub fn new(
        inner: Arc<dyn LlmModelResolver>,
        budget: Arc<dyn AutonomyBudget>,
        category: &str,
    ) -> BudgetResult<Self> {
        validate_category(category)?;
        Ok(Self {
            inner,
            budget,
            category: category.into(),
        })
    }
}
impl LlmModelResolver for BudgetedResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        let selection = self.inner.resolve()?;
        Ok(ModelSelection {
            provider: Arc::new(BudgetedProvider {
                inner: selection.provider,
                budget: self.budget.clone(),
                category: self.category.clone(),
            }),
            provider_timeout: selection.provider_timeout,
        })
    }
}

struct BudgetedProvider {
    inner: Arc<dyn LlmProvider>,
    budget: Arc<dyn AutonomyBudget>,
    category: String,
}
impl BudgetedProvider {
    fn admit(&self) -> Result<(), LlmError> {
        match self.budget.reserve(&self.category, 1, now_ms()) {
            Ok(_) => Ok(()),
            Err(BudgetError::Exhausted { .. }) => {
                Err(LlmError::Provider("自主运行预算已用尽；未发出请求".into()))
            }
            Err(_) => Err(LlmError::Provider("自主运行预算不可用；未发出请求".into())),
        }
    }
}
impl LlmProvider for BudgetedProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            self.admit()?;
            self.inner.complete(request).await
        })
    }
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            self.admit()?;
            self.inner.complete_json(request).await
        })
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            self.admit()?;
            self.inner.stream(request, sink).await
        })
    }
}
