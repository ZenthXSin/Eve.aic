//! 核心主模型的每轮解析实现；读取公开配置契约，不把模型配置交给 Kernel。
use crate::config;
use eve_config_api::{ConfigService, ConfigSnapshot, ModelProfile, ModelRolesConfig, MODELS_NAMESPACE, MODELS_SCHEMA_VERSION};
use eve_llm_api::{LlmError, LlmModelResolver, ModelSelection};
use eve_llm_openai::OpenAiProvider;
use std::sync::{Arc, Mutex};

#[derive(PartialEq)]
struct CapturedModel {
    provider: ConfigSnapshot,
    primary: Option<ModelProfile>,
    role_revision: Option<u64>,
}
struct CachedModel {
    captured: CapturedModel,
    selected: ModelSelection,
}

/// 凭据只在宿主启动时提供；缓存至多一份已校验选择，不实现 Debug。
pub(crate) struct CoreModelResolver {
    settings: Arc<dyn ConfigService>,
    api_key: String,
    cached: Mutex<Option<CachedModel>>,
}
impl CoreModelResolver {
    pub(crate) fn new(settings: Arc<dyn ConfigService>, api_key: String) -> Self {
        Self {
            settings,
            api_key,
            cached: Mutex::new(None),
        }
    }

    fn capture(&self) -> Result<CapturedModel, LlmError> {
        // 两个命名空间沿用同一 ConfigDocument revision；有界重读避免混用修订。
        for _ in 0..4 {
            let provider = self.settings.snapshot(config::OPENAI_NAMESPACE, 1)
                .map_err(|error| LlmError::Configuration(error.to_string()))?;
            let role = config::configured_role(&provider)
                .map_err(|error| LlmError::Configuration(error.to_string()))?;
            let (primary, role_revision) = match role {
                Some(role) => {
                    let snapshot = self.settings.snapshot(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION)
                        .map_err(|error| LlmError::Configuration(error.to_string()))?;
                    if provider.revision != snapshot.revision {
                        continue;
                    }
                    let roles = ModelRolesConfig::try_from(&snapshot)
                        .map_err(|error| LlmError::Configuration(error.to_string()))?;
                    let profile = roles.require(role)
                        .map_err(|error| LlmError::Configuration(error.to_string()))?.clone();
                    (Some(profile), Some(roles.revision()))
                }
                None => (None, None),
            };
            return Ok(CapturedModel { provider, primary, role_revision });
        }
        Err(LlmError::Configuration("模型配置持续变化，请在配置稳定后开启新轮。".into()))
    }
}
impl LlmModelResolver for CoreModelResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        let captured = self.capture()?;
        let mut cached = self.cached.lock()
            .map_err(|_| LlmError::Backend("模型 Provider 缓存不可用".into()))?;
        if let Some(previous) = cached.as_ref()
            && previous.captured == captured
        {
            return Ok(previous.selected.clone());
        }
        let config = config::provider_config(&captured.provider, captured.primary.as_ref())
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let selected = ModelSelection {
            provider_timeout: config.request_timeout,
            provider: Arc::new(OpenAiProvider::new(config, &self.api_key)?),
        };
        *cached = Some(CachedModel { captured, selected: selected.clone() });
        Ok(selected)
    }
}

#[cfg(test)]
mod tests;
