//! 组合层只读取宿主配置与凭据引用。
use crate::{AppError, CoreBootstrap};
use eve_developer_api::{ActionReviewFuture, ActionReviewRequest, ActionReviewer, DeveloperConfig};
use eve_llm_api::{
    LlmError, LlmModelResolver, ModelHealthObserver, ModelOutcome, ModelSelection, ProviderFailure,
    ProviderFailureKind,
};
use eve_plugin_api::ServiceRegistry;
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

/// 行动审查的模型用途，单独记健康与输出上限。
pub(crate) const REVIEW_USE: &str = "review";

/// 真实宿主在没有显式配置时自动装配主人可用的工具；本地 fixture 进程保持
/// 无工具，避免把测试服务器或测试目录暴露给模型。
fn direct_tools_enabled() -> bool {
    if std::env::var("EVE_DIRECT_TOOLS").ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }) {
        return true;
    }
    if std::env::var("EVE_OPENAI_API_KEY")
        .ok()
        .is_none_or(|key| key.trim().is_empty())
    {
        return false;
    }
    let Some(base_url) = std::env::var("EVE_OPENAI_BASE_URL").ok() else {
        return true;
    };
    let normalized = base_url.trim().to_ascii_lowercase();
    !(normalized.starts_with("http://127.0.0.1")
        || normalized.starts_with("http://localhost")
        || normalized.starts_with("http://[::1]")
        || normalized.starts_with("https://127.0.0.1")
        || normalized.starts_with("https://localhost")
        || normalized.starts_with("https://[::1]"))
}

pub(crate) fn install(bootstrap: &mut CoreBootstrap, state: &Path) -> Result<(), AppError> {
    install_with_authorization(bootstrap, state, None)
}
pub(crate) fn install_with_authorization(
    bootstrap: &mut CoreBootstrap,
    state: &Path,
    authorization: Option<Arc<dyn eve_developer_api::DeveloperAuthorization>>,
) -> Result<(), AppError> {
    install_with_member_runtime(bootstrap, state, authorization, None, None)
}
pub(crate) fn install_with_member_runtime(
    bootstrap: &mut CoreBootstrap,
    state: &Path,
    authorization: Option<Arc<dyn eve_developer_api::DeveloperAuthorization>>,
    member_runtime: Option<eve_developer_api::MemberRuntime>,
    reviewer: Option<Arc<dyn ActionReviewer>>,
) -> Result<(), AppError> {
    let config: DeveloperConfig = if let Some(path) = std::env::var_os("EVE_DEVELOPER_CONFIG") {
        let bytes = std::fs::read(path).map_err(|_| "开发工具配置文件无法读取；保留原文件")?;
        if bytes.len() > 32 * 1024 {
            return Err("开发工具配置超过上限".into());
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| "开发工具配置无效；不能在配置内填写 token 正文")?
    } else if direct_tools_enabled() {
        let workspace =
            std::env::current_dir().map_err(|_| "无法确定 Eve 工作目录；未装配开发工具")?;
        // 主人默认会话直接拥有受限开发工具；自升级准备、命令策略和行动审查生效。
        // 实时切换仍交给独立监督器；GitHub 写入保持显式仓库与凭据授权。
        DeveloperConfig {
            workspace,
            storage_directory: None,
            operators: BTreeSet::from(["owner".into()]),
            ordinary_access: false,
            commands: true,
            github: true,
            github_all_users: false,
            github_all_repositories: false,
            github_write_repositories: BTreeSet::new(),
            github_token_env: "EVE_GITHUB_TOKEN".into(),
            command_timeout_ms: 120_000,
            max_tool_rounds: 8,
            self_upgrade: true,
            live_upgrade: None,
            command_policy: None,
            action_review: true,
            cargo_program: "cargo".into(),
            cargo_arguments: Vec::new(),
        }
    } else {
        return Ok(());
    };
    config.validate()?;
    let token = std::env::var(&config.github_token_env).ok();
    if !config.github_write_repositories.is_empty()
        && token.as_ref().is_none_or(|v| v.trim().is_empty())
    {
        return Err("配置了 GitHub 写仓库，但对应的凭据环境变量为空".into());
    }
    let storage = config
        .storage_directory
        .clone()
        .unwrap_or_else(|| state.join("developer-tools"));
    let mut backend = eve_developer_tools::LocalDeveloper::new(config.clone(), storage, token)?;
    if let Some(authorization) = authorization {
        backend = backend.with_authorization(authorization);
    }
    if let Some(runtime) = member_runtime {
        backend = backend.with_member_runtime(runtime);
    }
    // 命令策略：内置规则始终生效，配置的规则文件追加在后；规则或例子有误时拒绝启动。
    let mut policy = eve_command_policy::PrefixRulePolicy::builtin();
    if let Some(path) = &config.command_policy {
        let text = std::fs::read_to_string(path).map_err(|_| "命令策略规则文件无法读取")?;
        let extra = eve_command_policy::PrefixRulePolicy::from_json(&text)?;
        policy = policy.merged(extra);
    }
    backend = backend.with_command_policy(Arc::new(policy));
    // 需确认的命令交给审查者；配置关闭或宿主没有审查者时，仍由模型带 owner_confirmed。
    if let Some(reviewer) = reviewer.filter(|_| config.action_review) {
        backend = backend.with_action_reviewer(reviewer);
    }
    let backend = Arc::new(backend);
    bootstrap.host_config.max_tool_rounds = config.max_tool_rounds;
    // 期限只在显式启用开发工具时扩大，普通对话和旧配置保持原值。
    bootstrap.host_config.tool_timeout = Duration::from_millis(config.command_timeout_ms + 30_000);
    bootstrap
        .tools
        .extend(eve_developer_tools::tools(&config, backend));
    Ok(())
}

pub(crate) fn member_runtime(
    java: Option<&std::ffi::OsStr>,
    jar: Option<&Path>,
) -> eve_developer_api::MemberRuntime {
    let java = java.unwrap_or_else(|| std::ffi::OsStr::new("java"));
    let path = std::path::PathBuf::from(java);
    let resolved = if path.is_absolute() {
        path.canonicalize().ok()
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|p| p.join(java))
                    .find(|p| p.is_file())
            })
            .and_then(|p| p.canonicalize().ok())
    };
    eve_developer_api::MemberRuntime {
        java_home: resolved
            .filter(|p| p.file_name().is_some_and(|n| n == "java"))
            .and_then(|p| p.parent()?.parent().map(Path::to_path_buf)),
        mindustry_server: jar.and_then(|p| p.canonicalize().ok()),
    }
}

/// 首次审查时才从已启动的配置服务取模型；之后沿用同一解析器，热配置照常生效。
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
/// 每次审查的结局写入标准错误的 `EVE_ACTION_REVIEW` 行，不含命令与原话。
struct Reported(
    eve_action_review::ModelActionReviewer,
    Arc<eve_model_health::HealthBook>,
);
impl ActionReviewer for Reported {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        Box::pin(async move {
            let delegated = request.delegated;
            let result = self.0.review(request).await;
            let outcome = match &result {
                Ok(_) => ModelOutcome::Succeeded,
                Err(e) if e.contains("超时") => ModelOutcome::TimedOut,
                Err(e) if e.starts_with("审查输出") || e.starts_with("审查理由") => {
                    ModelOutcome::InvalidResponse
                }
                Err(_) => ModelOutcome::Failed {
                    failure: ProviderFailure::new(ProviderFailureKind::Other),
                },
            };
            self.1
                .record(REVIEW_USE, outcome, crate::qq_people::now_ms());
            let line = match &result {
                Ok(v) => json!({"outcome": if v.permits() { "allowed" } else { "denied" },
                    "risk": v.risk, "authorization": v.authorization, "delegated": delegated}),
                Err(_) => json!({"outcome": "failed", "error": outcome, "delegated": delegated}),
            };
            eprintln!("EVE_ACTION_REVIEW {line}");
            result
        })
    }
}
/// QQ 宿主的行动审查者；没有模型凭据时不装配，开发工具退回由模型带 owner_confirmed。
pub(crate) fn action_reviewer(
    registry: Arc<dyn ServiceRegistry>,
    health: Arc<eve_model_health::HealthBook>,
) -> Option<Arc<dyn ActionReviewer>> {
    let key = std::env::var("EVE_OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())?;
    health.register(REVIEW_USE);
    let resolver = RegistryResolver {
        registry,
        key,
        resolver: OnceLock::new(),
    };
    Some(Arc::new(Reported(
        eve_action_review::ModelActionReviewer::new(Arc::new(resolver)),
        health,
    )))
}
