//! 组合层只读取宿主配置与凭据引用。没有配置就不注册开发工具。
use crate::{AppError, CoreBootstrap};
use eve_developer_api::DeveloperConfig;
use std::{path::Path, sync::Arc, time::Duration};

pub(crate) fn install(bootstrap: &mut CoreBootstrap, state: &Path) -> Result<(), AppError> {
    install_with_authorization(bootstrap, state, None)
}
pub(crate) fn install_with_authorization(
    bootstrap: &mut CoreBootstrap,
    state: &Path,
    authorization: Option<Arc<dyn eve_developer_api::DeveloperAuthorization>>,
) -> Result<(), AppError> {
    install_with_member_runtime(bootstrap, state, authorization, None)
}
pub(crate) fn install_with_member_runtime(
    bootstrap: &mut CoreBootstrap,
    state: &Path,
    authorization: Option<Arc<dyn eve_developer_api::DeveloperAuthorization>>,
    member_runtime: Option<eve_developer_api::MemberRuntime>,
) -> Result<(), AppError> {
    let Some(path) = std::env::var_os("EVE_DEVELOPER_CONFIG") else {
        return Ok(());
    };
    let bytes = std::fs::read(path).map_err(|_| "开发工具配置文件无法读取；保留原文件")?;
    if bytes.len() > 32 * 1024 {
        return Err("开发工具配置超过上限".into());
    }
    let config: DeveloperConfig = serde_json::from_slice(&bytes)
        .map_err(|_| "开发工具配置无效；不能在配置内填写 token 正文")?;
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
