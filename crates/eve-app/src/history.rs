//! 组合层按每轮读取公开配置，运行时只依赖历史窗口契约。
use eve_config_api::{ConfigService, ConfigSnapshot};
use eve_llm_api::{ContextCompactionPolicy, HistoryWindow, HistoryWindowResolver, LlmError};
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
    fn compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        let read = || -> Result<_, eve_config_api::ConfigError> {
            let snapshot = self.0.snapshot(NAMESPACE, 1)?;
            Ok((
                snapshot.get::<usize>("compact_at_tokens")?,
                snapshot.get::<usize>("compact_to_tokens")?,
            ))
        };
        let (trigger_tokens, target_tokens) =
            read().map_err(|_| LlmError::Configuration("主动上下文压缩配置无法读取".into()))?;
        if trigger_tokens == 0 {
            return Ok(None);
        }
        let policy = ContextCompactionPolicy {
            trigger_tokens,
            target_tokens,
        };
        policy.validate()?;
        Ok(Some(policy))
    }
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        let snapshot = self
            .0
            .snapshot(NAMESPACE, 1)
            .map_err(|_| LlmError::Configuration("历史窗口配置服务不可用".into()))?;
        window(&snapshot)
    }
}
