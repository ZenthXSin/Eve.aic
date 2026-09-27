//! 同步日志后端：串行写入输出设备，或按条数限制保留最近记录。

use eve_plugin_api::{LogLevel, LogRecord, Logger, PluginError, PluginResult};
use std::collections::VecDeque;
use std::io::{Stderr, Write};
use std::sync::Mutex;
use std::time::SystemTime;

/// 每条记录在同一把锁下完整写入，避免本实例的并发日志交错。
pub struct WriterLogger<W> {
    min_level: LogLevel,
    writer: Mutex<W>,
}

impl<W: Write + Send> WriterLogger<W> {
    pub fn new(min_level: LogLevel, writer: W) -> Self {
        Self {
            min_level,
            writer: Mutex::new(writer),
        }
    }
}

pub type StderrLogger = WriterLogger<Stderr>;

impl Default for StderrLogger {
    fn default() -> Self {
        Self::new(LogLevel::Info, std::io::stderr())
    }
}

impl<W: Write + Send> Logger for WriterLogger<W> {
    fn log(&self, record: LogRecord) -> PluginResult<()> {
        record.entry.validate()?;
        if record.entry.level < self.min_level {
            return Ok(());
        }
        let timestamp = match record.timestamp.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => duration.as_millis().to_string(),
            Err(error) => format!("-{}", error.duration().as_millis()),
        };
        // Debug 字符串编码保留中文并转义换行、引号和控制字符，保持一条物理行。
        let line = format!(
            "timestamp_ms={timestamp} level={} plugin={:?} target={:?} message={:?} fields={:?}\n",
            record.entry.level,
            record.plugin.as_str(),
            record.entry.target,
            record.entry.message,
            record.entry.fields,
        );
        self.writer
            .lock()
            .map_err(|_| PluginError::Log("日志写入锁中毒".into()))?
            .write_all(line.as_bytes())
            .map_err(|error| PluginError::Log(format!("写入失败：{error}")))
    }

    fn flush(&self) -> PluginResult<()> {
        self.writer
            .lock()
            .map_err(|_| PluginError::Log("日志写入锁中毒".into()))?
            .flush()
            .map_err(|error| PluginError::Log(format!("刷新失败：{error}")))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryLogSnapshot {
    /// 按接收顺序排列，不按墙上时钟排序。
    pub records: Vec<LogRecord>,
    /// 容量满后被淘汰的记录总数，不包含级别过滤的记录。
    pub dropped: u64,
}

#[derive(Default)]
struct MemoryLogs {
    records: VecDeque<LogRecord>,
    dropped: u64,
}

/// 记录条数有上限；单条内容大小由调用者控制。默认保留 1024 条 Info 及以上记录。
pub struct MemoryLogger {
    min_level: LogLevel,
    capacity: usize,
    logs: Mutex<MemoryLogs>,
}

impl MemoryLogger {
    pub fn new(min_level: LogLevel, capacity: usize) -> PluginResult<Self> {
        if capacity == 0 {
            return Err(PluginError::Log("内存日志容量必须大于零".into()));
        }
        Ok(Self {
            min_level,
            capacity,
            logs: Mutex::new(MemoryLogs::default()),
        })
    }

    pub fn snapshot(&self) -> PluginResult<MemoryLogSnapshot> {
        let logs = self
            .logs
            .lock()
            .map_err(|_| PluginError::Log("内存日志锁中毒".into()))?;
        Ok(MemoryLogSnapshot {
            records: logs.records.iter().cloned().collect(),
            dropped: logs.dropped,
        })
    }
}

impl Default for MemoryLogger {
    fn default() -> Self {
        Self::new(LogLevel::Info, 1024).expect("默认内存日志容量有效")
    }
}

impl Logger for MemoryLogger {
    fn log(&self, record: LogRecord) -> PluginResult<()> {
        record.entry.validate()?;
        if record.entry.level < self.min_level {
            return Ok(());
        }
        let mut logs = self
            .logs
            .lock()
            .map_err(|_| PluginError::Log("内存日志锁中毒".into()))?;
        if logs.records.len() == self.capacity {
            logs.records.pop_front();
            logs.dropped = logs.dropped.saturating_add(1);
        }
        logs.records.push_back(record);
        Ok(())
    }

    fn flush(&self) -> PluginResult<()> {
        Ok(())
    }
}
