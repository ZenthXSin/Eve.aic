//! 日志契约。插件提交内容，宿主附加来源与时间，后端决定过滤和输出方式。

use crate::{PluginError, PluginId, PluginResult};
use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

/// 按严重程度递增，后端可用最小级别过滤。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        })
    }
}

/// 插件可填写的日志内容。字段只属于内容，不会覆盖宿主附加的来源和时间。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

impl LogEntry {
    pub fn new(
        level: LogLevel,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> PluginResult<Self> {
        let entry = Self {
            level,
            target: target.into(),
            message: message.into(),
            fields: BTreeMap::new(),
        };
        entry.validate()?;
        Ok(entry)
    }

    /// 重复键以最后一次写入为准；内容在投递时再次校验。
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    pub fn validate(&self) -> PluginResult<()> {
        if self.target.trim().is_empty() {
            return Err(PluginError::Log("日志目标不能为空".into()));
        }
        if self.fields.keys().any(|key| key.trim().is_empty()) {
            return Err(PluginError::Log("日志字段名不能为空".into()));
        }
        Ok(())
    }
}

/// 完整记录；从 Context 投递时，plugin 与 timestamp 由宿主生成。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogRecord {
    pub timestamp: SystemTime,
    pub plugin: PluginId,
    pub entry: LogEntry,
}

/// 可替换的同步日志后端，不使用进程全局单例。
/// log 必须校验内容；成功表示已接收或按配置过滤，不保证落盘持久化。
/// 后端错误应返回 Err，由调用方决定是否影响业务；不要递归记录自身错误。
pub trait Logger: Send + Sync {
    fn log(&self, record: LogRecord) -> PluginResult<()>;
    /// 宿主停止生产日志后调用。将已接收记录交给输出设备，不保证 fsync。
    fn flush(&self) -> PluginResult<()>;
}
