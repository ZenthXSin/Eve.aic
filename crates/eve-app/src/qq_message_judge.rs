//! QQ 在途消息判断的宿主装配；凭据不进入普通配置或渠道进程。
use crate::{AppError, models::CoreModelResolver};
use eve_config_api::{
    ConfigField, ConfigKind, ConfigSchema, ConfigService, ConfigSnapshot, MODELS_NAMESPACE,
    MODELS_SCHEMA_VERSION, ModelProfile, ModelRole, ModelRolesConfig,
};
use eve_jev::{JevConfig, JevConfigurationError, JevRelationJudge};
use eve_llm_api::LlmModelResolver;
use eve_message_api::{
    DiscardRelationObservations, RelationError, RelationExample, RelationFuture, RelationInput,
    RelationJudge, RelationObservation, RelationObserver, observe_relation,
};
use eve_message_plugin::RelationPlugin;
use eve_runtime::LlmRelationJudge;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const PROVIDER_NAMESPACE: &str = "provider.jev";
const PRIMARY_KEY: &str = "EVE_OPENAI_API_KEY";
const JEV_KEY: &str = "EVE_JEV_API_KEY";

/// 只在宿主显式选择时启用自然消息判断；默认仍将普通消息排队。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MessageJudgeMode {
    #[default]
    Off,
    Primary,
    Jev,
}
impl MessageJudgeMode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Primary => "primary",
            Self::Jev => "jev",
        }
    }
}

pub(crate) fn provider_schema() -> ConfigSchema {
    let mut base_url = ConfigField::new(ConfigKind::String, Some(json!(eve_jev::DEFAULT_BASE_URL)));
    base_url.environment = Some("EVE_JEV_BASE_URL".into());
    ConfigSchema {
        namespace: PROVIDER_NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("base_url".into(), base_url)]),
    }
}

pub(crate) fn relation_plugin(
    mode: MessageJudgeMode,
    settings: Arc<dyn ConfigService>,
) -> Result<RelationPlugin, AppError> {
    relation_plugin_with_key_reader(mode, settings, |name| {
        std::env::var(name).map_err(|_| match name {
            JEV_KEY => "Jev 消息判断需要宿主 EVE_JEV_API_KEY。".into(),
            _ => "模型消息判断需要宿主 EVE_OPENAI_API_KEY。".into(),
        })
    })
}

pub(crate) fn group_judge(settings: Arc<dyn ConfigService>) -> Result<Arc<JevRoleJudge>, AppError> {
    let key = std::env::var(JEV_KEY).map_err(|_| "群话题判断需要宿主 EVE_JEV_API_KEY")?;
    let judge = Arc::new(JevRoleJudge::new(settings, key));
    judge.resolve()?;
    Ok(judge)
}

/// 记忆相关性独立装配，不开启群消息自动筛选或主动回复。无凭据/配置错误只用本地回退。
pub(crate) fn context_selector(
    settings: Arc<dyn ConfigService>,
) -> Option<Arc<dyn eve_memory_api::ContextRelevanceSelector>> {
    let key = std::env::var(JEV_KEY).ok()?;
    let judge = Arc::new(JevRoleJudge::new(settings, key));
    judge.resolve().ok()?;
    Some(judge)
}
impl eve_memory_api::ContextRelevanceSelector for JevRoleJudge {
    fn select<'a>(
        &'a self,
        input: eve_memory_api::ContextSelectionInput,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = eve_memory_api::MemoryResult<Vec<String>>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let judge = self
                .resolve()
                .map_err(|_| eve_memory_api::MemoryError::Unavailable)?;
            eve_memory_api::ContextRelevanceSelector::select(judge.as_ref(), input).await
        })
    }
}

/// 群关系和在途任务共用同一 Jev 客户端、热配置与并发槽。
pub(crate) fn relation_plugin_shared(
    settings: Arc<dyn ConfigService>,
    judge: Arc<JevRoleJudge>,
) -> Result<RelationPlugin, AppError> {
    let resolver = Arc::new(CoreModelResolver::new(
        settings,
        std::env::var(PRIMARY_KEY).map_err(|_| "消息判断需要宿主主模型凭据")?,
    ));
    resolver.resolve()?;
    Ok(RelationPlugin::with_fallback(
        Some(judge),
        Arc::new(LlmRelationJudge::with_resolver(resolver)),
    )?)
}

fn relation_plugin_with_key_reader(
    mode: MessageJudgeMode,
    settings: Arc<dyn ConfigService>,
    mut read_key: impl FnMut(&str) -> Result<String, AppError>,
) -> Result<RelationPlugin, AppError> {
    if mode == MessageJudgeMode::Off {
        return Ok(RelationPlugin::rules()?);
    }
    let resolver = Arc::new(CoreModelResolver::new(
        settings.clone(),
        read_key(PRIMARY_KEY)?,
    ));
    // 只构造客户端，不发探测请求。运行中每次判断重新解析主模型选择。
    resolver
        .resolve()
        .map_err(|_| "消息判断主模型配置或宿主凭据无效。")?;
    let fallback = Arc::new(LlmRelationJudge::with_resolver(resolver));
    let primary: Option<Arc<dyn RelationJudge>> = if mode == MessageJudgeMode::Jev {
        let judge = Arc::new(JevRoleJudge::new(settings, read_key(JEV_KEY)?));
        judge.resolve()?;
        Some(judge)
    } else {
        None
    };
    Ok(RelationPlugin::with_fallback(primary, fallback)?)
}

#[derive(PartialEq)]
struct CapturedJev {
    provider: ConfigSnapshot,
    profile: ModelProfile,
    role_revision: u64,
}

struct CachedJev {
    credential_version: u64,
    captured: CapturedJev,
    judge: Arc<JevRelationJudge>,
}

/// 只保存宿主启动时读取的独立 Jev 凭据；不实现 Debug，不解析任意凭据引用。
pub(crate) struct JevRoleJudge {
    settings: Arc<dyn ConfigService>,
    credentials: Result<Arc<dyn eve_llm_api::ModelCredentials>, eve_llm_api::CredentialError>,
    cached: Mutex<Option<CachedJev>>,
}

impl eve_chime_api::GroupRelationJudge for JevRoleJudge {
    fn classify(
        &self,
        input: eve_chime_api::GroupRelationInput,
    ) -> eve_llm_api::LlmFuture<'_, eve_chime_api::GroupRelationDecision> {
        Box::pin(async move {
            let judge = self
                .resolve()
                .map_err(|_| eve_llm_api::LlmError::Context("Jev 群关系配置不可用".into()))?;
            judge.classify(input).await
        })
    }
}

impl eve_chime_api::GroupRecallSelector for JevRoleJudge {
    fn select(
        &self,
        input: eve_chime_api::GroupRecallInput,
    ) -> eve_llm_api::LlmFuture<'_, Vec<String>> {
        Box::pin(async move {
            let judge = self
                .resolve()
                .map_err(|_| eve_llm_api::LlmError::Context("Jev 原话挑选配置不可用".into()))?;
            judge.select(input).await
        })
    }
}

impl JevRoleJudge {
    fn new(settings: Arc<dyn ConfigService>, api_key: String) -> Self {
        Self {
            settings,
            credentials: crate::model_credentials::bank(
                eve_llm_api::CredentialPurpose::Jev,
                api_key,
            ),
            cached: Mutex::new(None),
        }
    }

    fn capture(&self) -> Result<CapturedJev, JevConfigurationError> {
        // 两个命名空间共享 ConfigDocument revision，最多重读四次避免混用配置。
        for _ in 0..4 {
            let provider = self
                .settings
                .snapshot(PROVIDER_NAMESPACE, 1)
                .map_err(|_| JevConfigurationError)?;
            let roles = self
                .settings
                .snapshot(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION)
                .map_err(|_| JevConfigurationError)?;
            if provider.revision != roles.revision {
                continue;
            }
            let roles = ModelRolesConfig::try_from(&roles).map_err(|_| JevConfigurationError)?;
            let profile = roles
                .require(ModelRole::Jev)
                .map_err(|_| JevConfigurationError)?
                .clone();
            if profile.provider != "jev"
                || crate::model_credentials::reference(
                    eve_llm_api::CredentialPurpose::Jev,
                    profile.credential_ref.as_deref(),
                )
                .is_err()
                || profile.max_output_tokens.is_some()
            {
                return Err(JevConfigurationError);
            }
            return Ok(CapturedJev {
                provider,
                profile,
                role_revision: roles.revision(),
            });
        }
        Err(JevConfigurationError)
    }

    fn resolve(&self) -> Result<Arc<JevRelationJudge>, JevConfigurationError> {
        let captured = self.capture()?;
        let credentials = self
            .credentials
            .as_ref()
            .map_err(|_| JevConfigurationError)?;
        let credential = crate::model_credentials::resolve(
            credentials.as_ref(),
            eve_llm_api::CredentialPurpose::Jev,
            captured.profile.credential_ref.as_deref(),
        )
        .map_err(|_| JevConfigurationError)?;
        let mut cached = self.cached.lock().map_err(|_| JevConfigurationError)?;
        if let Some(previous) = cached.as_ref()
            && previous.captured == captured
            && previous.credential_version == credential.version()
        {
            return Ok(previous.judge.clone());
        }
        let config = JevConfig {
            base_url: captured
                .provider
                .get("base_url")
                .map_err(|_| JevConfigurationError)?,
            model: captured.profile.model.clone(),
            timeout: Duration::from_millis(captured.profile.timeout_ms),
            max_concurrent_requests: captured.profile.max_concurrent_requests,
        };
        let judge = Arc::new(JevRelationJudge::new(config, credential.expose())?);
        *cached = Some(CachedJev {
            captured,
            credential_version: credential.version(),
            judge: judge.clone(),
        });
        Ok(judge)
    }
}

impl RelationJudge for JevRoleJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        self.judge_observed(input, Arc::new(DiscardRelationObservations))
    }
    fn judge_observed(
        &self,
        input: RelationInput,
        observer: Arc<dyn RelationObserver>,
    ) -> RelationFuture<'_> {
        self.judge_with_examples(input, vec![], observer)
    }
    fn judge_with_examples(
        &self,
        input: RelationInput,
        examples: Vec<RelationExample>,
        observer: Arc<dyn RelationObserver>,
    ) -> RelationFuture<'_> {
        Box::pin(async move {
            observe_relation(observer.as_ref(), RelationObservation::Supported);
            let selected = self.resolve().map_err(|_| RelationError::Unavailable)?;
            selected
                .judge_with_examples(input, examples, observer)
                .await
        })
    }
}

#[cfg(test)]
mod tests;
