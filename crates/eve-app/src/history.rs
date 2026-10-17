//! 组合层按每轮读取公开配置，运行时只依赖历史窗口契约。
use eve_config_api::{ConfigService, ConfigSnapshot};
use eve_llm_api::{HistoryWindow, HistoryWindowResolver, LlmError};
use std::sync::Arc;

pub(crate) const NAMESPACE: &str = "runtime.history";
pub(crate) struct ConfiguredHistory(pub(crate) Arc<dyn ConfigService>);

pub(crate) fn window(snapshot: &ConfigSnapshot) -> Result<HistoryWindow, LlmError> {
    let read = || -> Result<HistoryWindow, eve_config_api::ConfigError> {
        Ok(HistoryWindow {
            max_turns: snapshot.get("max_turns")?,
            max_bytes: snapshot.get("max_bytes")?,
        })
    };
    let value = read().map_err(|_| LlmError::Configuration("历史窗口配置无法读取".into()))?;
    value.validate()?;
    Ok(value)
}

impl HistoryWindowResolver for ConfiguredHistory {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        let snapshot = self
            .0
            .snapshot(NAMESPACE, 1)
            .map_err(|_| LlmError::Configuration("历史窗口配置服务不可用".into()))?;
        window(&snapshot)
    }
}
