//! 核心主模型的每轮解析实现；读取公开配置契约，不把模型配置交给 Kernel。
use crate::config;
use eve_config_api::{
    ConfigService, ConfigSnapshot, ConfigSource, MODELS_NAMESPACE, MODELS_SCHEMA_VERSION,
    ModelProfile, ModelRole, ModelRolesConfig,
};
use eve_llm_api::{LlmError, LlmModelResolver, ModelSelection};
use eve_llm_openai::{OpenAiProvider, StreamTimeoutPolicy};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// 进程内的用量观察者（模型健康记录）；宿主启动时设置一次，之后解析出的 Provider 都会记录
/// 提供方报告的缓存命中与本地前缀复用。未设置时不记录。
static USAGE: std::sync::OnceLock<Arc<dyn eve_llm_api::ModelUsageObserver>> =
    std::sync::OnceLock::new();

pub(crate) fn observe_usage(observer: Arc<dyn eve_llm_api::ModelUsageObserver>) {
    let _ = USAGE.set(observer);
}

pub(crate) fn timing_enabled() -> bool {
    std::env::var("EVE_TRACE_DIALOGUE_TIMING").is_ok_and(|v| matches!(v.as_str(), "1" | "true"))
}

#[derive(PartialEq)]
struct CapturedModel {
    provider: ConfigSnapshot,
    profile: Option<ModelProfile>,
    selected_role: Option<ModelRole>,
    role_revision: Option<u64>,
    /// 用途配置的单次输出上限；None 表示沿用所选模型。
    use_limit: Option<u32>,
    natural_style: bool,
    streamed_completions: bool,
    transient_retries: bool,
    stream_timeouts: Option<StreamTimeoutPolicy>,
}
impl CapturedModel {
    fn credential_purpose(&self) -> eve_llm_api::CredentialPurpose {
        if self.selected_role == Some(ModelRole::AuxiliarySmall) {
            eve_llm_api::CredentialPurpose::Auxiliary
        } else {
            eve_llm_api::CredentialPurpose::Primary
        }
    }
}
struct CachedModel {
    captured: CapturedModel,
    credential_version: u64,
    selected: ModelSelection,
}

/// 凭据只在宿主启动时提供；缓存至多一份已校验选择，不实现 Debug。
pub(crate) struct CoreModelResolver {
    settings: Arc<dyn ConfigService>,
    credentials: Result<Arc<dyn eve_llm_api::ModelCredentials>, eve_llm_api::CredentialError>,
    use_name: Option<String>,
    timeout_cap: Option<Duration>,
    cached: Mutex<Option<CachedModel>>,
}
impl CoreModelResolver {
    pub(crate) fn new(settings: Arc<dyn ConfigService>, api_key: String) -> Self {
        Self::from_credentials(settings, crate::model_credentials::core_bank(api_key))
    }

    pub(crate) fn from_credentials(
        settings: Arc<dyn ConfigService>,
        credentials: Result<Arc<dyn eve_llm_api::ModelCredentials>, eve_llm_api::CredentialError>,
    ) -> Self {
        Self {
            settings,
            credentials,
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

/// 面板显示的用途当前模型配置与编辑目标；不含凭据与端点。
pub(crate) struct ModelSummary {
    /// 与展示模型相同的一致性配置修订及实际修改目标。
    pub(crate) config_revision: u64,
    pub(crate) model_namespace: &'static str,
    pub(crate) model_field: String,
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
    let (namespace, field) = match (use_name, captured.use_limit, &captured.profile) {
        (Some(name), Some(_), _) => (config::MODEL_USES_NAMESPACE, config::use_limit_field(name)),
        (_, _, Some(_)) => (
            MODELS_NAMESPACE,
            captured
                .selected_role
                .unwrap_or(ModelRole::Primary)
                .field("max_output_tokens"),
        ),
        _ => (config::OPENAI_NAMESPACE, "max_output_tokens".into()),
    };
    let (model_namespace, model_field) = match captured.selected_role {
        Some(role) => (MODELS_NAMESPACE, role.field("model")),
        None => (config::OPENAI_NAMESPACE, "model".into()),
    };
    Ok(ModelSummary {
        config_revision: captured.provider.revision,
        model_namespace,
        model_field,
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

/// 所选对话模型配置叠加用途上限；不改变角色、端点或期限。
fn selected_config(captured: &CapturedModel) -> Result<eve_llm_openai::OpenAiConfig, LlmError> {
    let mut config = config::provider_config_for(
        &captured.provider,
        captured.profile.as_ref(),
        captured.credential_purpose(),
    )
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
        let (use_limit, selected_use_role) = match use_name {
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
                let role = config::model_use_role(&snapshot, name)
                    .map_err(|error| LlmError::Configuration(error.to_string()))?;
                ((limit != 0).then_some(limit), role)
            }
            None => (None, ModelRole::Primary),
        };
        let role = config::configured_role(&provider)
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let role = if selected_use_role == ModelRole::AuxiliarySmall {
            Some(ModelRole::AuxiliarySmall)
        } else {
            role
        };
        let (profile, role_revision) = match role {
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
        let streamed_completions = match std::env::var("EVE_OPENAI_STREAM_COMPLETIONS") {
            Err(std::env::VarError::NotPresent) => false,
            Ok(value) if matches!(value.as_str(), "1" | "true") => true,
            Ok(value) if matches!(value.as_str(), "0" | "false") => false,
            _ => {
                return Err(LlmError::Configuration(
                    "EVE_OPENAI_STREAM_COMPLETIONS 必须为 true/false 或 1/0".into(),
                ));
            }
        };
        let transient_retries = match std::env::var("EVE_OPENAI_TRANSIENT_RETRIES") {
            Err(std::env::VarError::NotPresent) => true,
            Ok(value) if matches!(value.as_str(), "1" | "true") => true,
            Ok(value) if matches!(value.as_str(), "0" | "false") => false,
            _ => {
                return Err(LlmError::Configuration(
                    "EVE_OPENAI_TRANSIENT_RETRIES 必须为 true/false 或 1/0".into(),
                ));
            }
        };
        let stream_timeouts =
            if streamed_completions && matches!(use_name, Some("dialogue" | "digest")) {
                let purpose = if role == Some(ModelRole::AuxiliarySmall) {
                    eve_llm_api::CredentialPurpose::Auxiliary
                } else {
                    eve_llm_api::CredentialPurpose::Primary
                };
                let config = config::provider_config_for(&provider, profile.as_ref(), purpose)
                    .map_err(|e| LlmError::Configuration(e.to_string()))?;
                let mut policy = stream_timeout_policy(config.request_timeout)?;
                if use_name == Some("digest") {
                    policy.total = policy.total.min(eve_llm_api::CONTEXT_COMPACTION_TIMEOUT);
                    policy.first_response = policy.first_response.min(policy.total);
                    policy.idle = policy.idle.min(policy.total);
                }
                Some(policy)
            } else {
                None
            };
        return Ok(CapturedModel {
            provider,
            profile,
            selected_role: role,
            role_revision,
            use_limit,
            natural_style,
            streamed_completions,
            transient_retries,
            stream_timeouts,
        });
    }
    Err(LlmError::Configuration(
        "模型配置持续变化，请在配置稳定后开启新轮。".into(),
    ))
}
fn stream_timeout_policy(first: Duration) -> Result<StreamTimeoutPolicy, LlmError> {
    let duration = |name: &str, default: Duration| match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|v| (1..=600_000).contains(v))
            .map(Duration::from_millis)
            .ok_or_else(|| LlmError::Configuration(format!("{name} 必须为 1 至 600000 毫秒"))),
        Err(_) => Err(LlmError::Configuration(format!("{name} 必须为有效毫秒数"))),
    };
    let policy = StreamTimeoutPolicy {
        first_response: duration("EVE_MODEL_FIRST_RESPONSE_TIMEOUT_MS", first)?,
        idle: duration("EVE_MODEL_STREAM_IDLE_TIMEOUT_MS", first)?,
        total: duration(
            "EVE_MODEL_STREAM_TOTAL_TIMEOUT_MS",
            Duration::from_secs(600),
        )?,
    };
    policy.validate()?;
    Ok(policy)
}
impl LlmModelResolver for CoreModelResolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        let captured = self.capture()?;
        let credentials = self
            .credentials
            .as_ref()
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let credential = crate::model_credentials::resolve(
            credentials.as_ref(),
            captured.credential_purpose(),
            captured
                .profile
                .as_ref()
                .and_then(|profile| profile.credential_ref.as_deref()),
        )
        .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| LlmError::Backend("模型 Provider 缓存不可用".into()))?;
        if let Some(previous) = cached.as_ref()
            && previous.captured == captured
            && previous.credential_version == credential.version()
        {
            return Ok(previous.selected.clone());
        }
        let mut config = selected_config(&captured)?;
        if let Some(cap) = self.timeout_cap {
            config.request_timeout = config.request_timeout.min(cap);
        }
        let mut stream_timeouts = captured.stream_timeouts;
        if let (Some(policy), Some(cap)) = (&mut stream_timeouts, self.timeout_cap) {
            policy.total = policy.total.min(cap);
            policy.first_response = policy.first_response.min(policy.total);
            policy.idle = policy.idle.min(policy.total);
        }
        // 留给适配器先返回分阶段诊断；外层只作为略晚的安全期限。
        let provider_timeout =
            stream_timeouts.map_or(config.request_timeout, |p| p.total + Duration::from_secs(1));
        let mut provider = OpenAiProvider::new(config, credential.expose())?;
        if let Some(policy) = stream_timeouts {
            if timing_enabled() {
                eprintln!(
                    "EVE_MODEL_STREAM_TIMEOUTS {}",
                    serde_json::json!({
                        "first_response_ms":policy.first_response.as_millis(),
                        "idle_ms":policy.idle.as_millis(),"total_ms":policy.total.as_millis(),
                        "host_timeout_ms":provider_timeout.as_millis(),
                        "use_name":self.use_name,
                    })
                );
            }
            provider = provider.with_stream_timeout_policy(policy)?;
        }
        if captured.streamed_completions {
            provider = provider.with_streamed_completions();
        }
        // 对话与后台任务执行共用主链路：瞬时错误（超时/429/5xx）按底层有界重试处理。
        if captured.transient_retries {
            provider = provider.with_transient_retries();
        }
        if matches!(self.use_name.as_deref(), Some("dialogue" | "digest")) && timing_enabled() {
            provider = provider.with_timing_diagnostics();
        }
        let role = self.use_name.as_deref().unwrap_or("dialogue");
        let usage = USAGE.get().cloned();
        if let Some(observer) = &usage {
            provider = provider.with_usage_observer(observer.clone(), role);
        }
        let mut provider: Arc<dyn eve_llm_api::LlmProvider> = Arc::new(provider);
        // 探针紧贴实际发出的请求（在接话约束等改写之后），只测量不修改。
        if let Some(observer) = usage {
            provider = Arc::new(eve_model_health::PrefixProbe::new(provider, role, observer));
        }
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
            credential_version: credential.version(),
            selected: selected.clone(),
        });
        Ok(selected)
    }
}

#[cfg(test)]
mod tests;
