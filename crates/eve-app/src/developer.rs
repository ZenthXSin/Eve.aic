//! 组合层只读取宿主配置与凭据引用。没有配置就不注册开发工具。
use crate::{AppError, CoreBootstrap};
use eve_developer_api::DeveloperConfig;
use std::{path::Path, sync::Arc, time::Duration};

pub(crate) fn install(bootstrap: &mut CoreBootstrap, state: &Path) -> Result<(), AppError> {
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
    let backend = Arc::new(eve_developer_tools::LocalDeveloper::new(
        config.clone(),
        state.join("developer-tools"),
        token,
    )?);
    bootstrap.host_config.max_tool_rounds = config.max_tool_rounds;
    // 期限只在显式启用开发工具时扩大，普通对话和旧配置保持原值。
    bootstrap.host_config.tool_timeout = Duration::from_millis(config.command_timeout_ms + 30_000);
    bootstrap
        .tools
        .extend(eve_developer_tools::tools(&config, backend));
    Ok(())
}
