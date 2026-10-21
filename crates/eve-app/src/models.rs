//! 核心主模型的每轮解析实现；读取公开配置契约，不把模型配置交给 Kernel。
use crate::config;
use eve_config_api::{
    ConfigService, ConfigSnapshot, ConfigSource, MODELS_NAMESPACE, MODELS_SCHEMA_VERSION,
    ModelProfile, ModelRolesConfig,
};
use eve_llm_api::{LlmError, LlmModelResolver, ModelSelection};
use eve_llm_openai::OpenAiProvider;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) fn timing_enabled() -> bool {
    std::env::var("EVE_TRACE_DIALOGUE_TIMING").is_ok_and(|v| matches!(v.as_str(), "1" | "true"))
}

#[derive(PartialEq)]
struct CapturedModel {
    provider: ConfigSnapshot,
    primary: Option<ModelProfile>,
    role_revision: Option<u64>,
    /// 用途配置的单次输出上限；None 表示沿用主模型。
    use_limit: Option<u32>,
    natural_style: bool,
    streamed_completions: bool,
}
struct CachedModel {
    captured: CapturedModel,
    selected: ModelSelection,
}

/// 凭据只在宿主启动时提供；缓存至多一份已校验选择，不实现 Debug。
pub(crate) struct CoreModelResolver {
    settings: Arc<dyn ConfigService>,
    api_key: String,
    use_name: Option<String>,
    timeout_cap: Option<Duration>,
    cached: Mutex<Option<CachedModel>>,
}
impl CoreModelResolver {
    pub(crate) fn new(settings: Arc<dyn ConfigService>, api_key: String) -> Self {
        Self {
            settings,
            api_key,
            use_name: None,
            timeout_cap: None,
            cached: Mutex::new(None),
        }
    }

    /// 按用途读取 runtime.model_uses 中的单次输出上限；宿主须登记该命名空间，缺失时解析失败。
    pub(crate) fn with_use(mut self, name: &str) -> Self {
        self.use_name = Some(name.into());
        self
    }

    /// 用途的供应商请求期限，留给组件收尾余量，避免外层取消吞掉健康结局。
    pub(crate) fn with_timeout_cap(mut self, cap: Duration) -> Self {
        self.timeout_cap = Some(cap);
        self
    }

    fn capture(&self) -> Result<CapturedModel, LlmError> {
        capture(&*self.settings, self.use_name.as_deref())
    }
}

/// 面板显示的当前主模型配置；不含凭据与端点。
pub(crate) struct ModelSummary {
    pub(crate) model: String,
    pub(crate) protocol: &'static str,
    pub(crate) max_output_tokens: Option<u32>,
    /// 单次输出上限取自哪个配置键（命名空间.字段）。
    pub(crate) max_output_tokens_key: String,
    /// 该键当前生效值的来源；配置服务不提供时为空。
    pub(crate) max_output_tokens_source: Option<ConfigSource>,
}

/// 按与每轮解析相同的规则读取当前模型配置；给出用途时按该用途的单次输出上限。
pub(crate) fn summary(
    settings: &dyn ConfigService,
    use_name: Option<&str>,
) -> Result<ModelSummary, LlmError> {
    let captured = capture(settings, use_name)?;
    let config = selected_config(&captured)?;
    let (namespace, field) = match (use_name, captured.use_limit, &captured.primary) {
        (Some(name), Some(_), _) => (config::MODEL_USES_NAMESPACE, config::use_limit_field(name)),
        (_, _, Some(_)) => (MODELS_NAMESPACE, "primary_max_output_tokens".into()),
        _ => (config::OPENAI_NAMESPACE, "max_output_tokens".into()),
    };
    Ok(ModelSummary {
        model: config.model,
        protocol: match config.protocol {
            eve_llm_openai::OpenAiProtocol::Responses => "responses",
            eve_llm_openai::OpenAiProtocol::ChatCompletions => "chat",
        },
        max_output_tokens: config.max_output_tokens,
        max_output_tokens_source: settings.field_source(namespace, &field).ok(),
        max_output_tokens_key: format!("{namespace}.{field}"),
    })
}

/// 主模型配置叠加用途上限；用途上限只替换单次输出上限，不改变模型、端点或期限。
fn selected_config(captured: &CapturedModel) -> Result<eve_llm_openai::OpenAiConfig, LlmError> {
    let mut config = config::provider_config(&captured.provider, captured.primary.as_ref())
        .map_err(|error| LlmError::Configuration(error.to_string()))?;
    if let Some(limit) = captured.use_limit {
        config.max_output_tokens = Some(limit);
    }
    Ok(config)
}

fn capture(
    settings: &dyn ConfigService,
    use_name: Option<&str>,
) -> Result<CapturedModel, LlmError> {
    // 两个命名空间沿用同一 ConfigDocument revision；有界重读避免混用修订。
    for _ in 0..4 {
        let provider = settings
            .snapshot(config::OPENAI_NAMESPACE, 1)
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let role = config::configured_role(&provider)
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let (primary, role_revision) = match role {
            Some(role) => {
                let snapshot = settings
                    .snapshot(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION)
                    .map_err(|error| LlmError::Configuration(error.to_string()))?;
                if provider.revision != snapshot.revision {
                    continue;
                }
                let roles = ModelRolesConfig::try_from(&snapshot)
                    .map_err(|error| LlmError::Configuration(error.to_string()))?;
                let profile = roles
                    .require(role)
                    .map_err(|error| LlmError::Configuration(error.to_string()))?
                    .clone();
                (Some(profile), Some(roles.revision()))
            }
            None => (None, None),
        };
        let use_limit = match use_name {
            Some(name) => {
                let snapshot = settings
                    .snapshot(config::MODEL_USES_NAMESPACE, 1)
                    .map_err(|error| LlmError::Configuration(error.to_string()))?;
                if provider.revision != snapshot.revision {
                    continue;
                }
                let limit: u32 = snapshot
                    .get(&config::use_limit_field(name))
                    .map_err(|error| LlmError::Configuration(error.to_string()))?;
                (limit != 0).then_some(limit)
            }
            None => None,
        };
        let natural_style = if use_name == Some("dialogue") {
            let snapshot = settings
                .snapshot(crate::dialogue::NAMESPACE, 1)
                .map_err(|error| LlmError::Configuration(error.to_string()))?;
            if provider.revision != snapshot.revision {
                continue;
            }
            snapshot
                .get("natural_style")
                .map_err(|error| LlmError::Configuration(error.to_string()))?
        } else {
            false
        };
        return Ok(CapturedModel {
            provider,
            primary,
            role_revision,
            use_limit,
            natural_style,
            streamed_completions: match std::env::var("EVE_OPENAI_STREAM_COMPLETIONS") {
                Err(std::env::VarError::NotPresent) => false,
                Ok(value) if matches!(value.as_str(), "1" | "true") => true,
                Ok(value) if matches!(value.as_str(), "0" | "false") => false,
                _ => {
                    return Err(LlmError::Configuration(
                        "EVE_OPENAI_STREAM_COMPLETIONS 必须为 true/false 或 1/0".into(),
                    ));
                }
            },
        });
    }
    Err(LlmError::Configuration(
        "模型配置持续变化，请在配置稳定后开启新轮。".into(),
    ))
}
impl LlmModelResolver for CoreModelResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        let captured = self.capture()?;
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| LlmError::Backend("模型 Provider 缓存不可用".into()))?;
        if let Some(previous) = cached.as_ref()
            && previous.captured == captured
        {
            return Ok(previous.selected.clone());
        }
        let mut config = selected_config(&captured)?;
        if let Some(cap) = self.timeout_cap {
            config.request_timeout = config.request_timeout.min(cap);
        }
        let provider_timeout = config.request_timeout;
        let mut provider = OpenAiProvider::new(config, &self.api_key)?;
        if captured.streamed_completions {
            provider = provider.with_streamed_completions();
        }
        if self.use_name.as_deref() == Some("dialogue") {
            provider = provider.with_transient_retries();
        }
        if self.use_name.as_deref() == Some("dialogue") && timing_enabled() {
            provider = provider.with_timing_diagnostics();
        }
        let provider: Arc<dyn eve_llm_api::LlmProvider> = Arc::new(provider);
        let selected = ModelSelection {
            provider_timeout,
            provider: if captured.natural_style {
                Arc::new(crate::dialogue::NaturalDialogue(provider))
            } else {
                provider
            },
        };
        *cached = Some(CachedModel {
            captured,
            selected: selected.clone(),
        });
        Ok(selected)
    }
}

#[cfg(test)]
mod tests;
