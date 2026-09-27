use eve_kernel::{
    Kernel, KernelServices, PluginState,
    backends::{MemoryLogger, WriterLogger},
};
use eve_plugin_api::*;
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime};

struct TestPlugin<F> {
    manifest: PluginManifest,
    start: F,
}

impl<F> Plugin for TestPlugin<F>
where
    F: Fn(PluginContext) -> PluginResult<Option<Cleanup>> + Send,
{
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let result = (self.start)(ctx);
        Box::pin(async move { result })
    }
}

fn register(
    kernel: &Kernel,
    id: &str,
    start: impl Fn(PluginContext) -> PluginResult<Option<Cleanup>> + Send + 'static,
) {
    kernel
        .register(Box::new(TestPlugin {
            manifest: PluginManifest::new(id, "0.1.0").unwrap(),
            start,
        }))
        .unwrap();
}

fn entry(level: LogLevel, message: &str) -> LogEntry {
    LogEntry::new(level, "test.logging", message).unwrap()
}

fn record(level: LogLevel, message: &str) -> LogRecord {
    LogRecord {
        timestamp: SystemTime::UNIX_EPOCH + Duration::from_millis(1234),
        plugin: PluginId::new("test.owner").unwrap(),
        entry: entry(level, message),
    }
}

#[tokio::test]
async fn context_assigns_identity_and_timestamp_and_rejects_old_instances() {
    let logger = Arc::new(MemoryLogger::default());
    let kernel = Kernel::with_services(KernelServices {
        logger: logger.clone(),
        ..Default::default()
    });
    let contexts = Arc::new(Mutex::new(Vec::new()));
    for id in ["one", "two"] {
        let contexts = contexts.clone();
        register(&kernel, id, move |ctx| {
            ctx.log(entry(LogLevel::Info, "已启动").with_field("plugin", "pretend-owner"))?;
            contexts.lock().unwrap().push(ctx);
            Ok(None)
        });
    }
    let before = SystemTime::now();
    kernel.start_all().await.unwrap();
    let after = SystemTime::now();
    let snapshot = logger.snapshot().unwrap();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|r| r.plugin.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    for record in &snapshot.records {
        assert!(record.timestamp >= before && record.timestamp <= after);
        assert_eq!(record.entry.fields["plugin"], "pretend-owner");
    }
    let old = contexts.lock().unwrap()[0].clone();
    kernel.stop_all().await.unwrap();
    kernel.start_all().await.unwrap();
    assert!(matches!(
        old.log(entry(LogLevel::Info, "过期日志")),
        Err(PluginError::Lifecycle(_))
    ));
    let current = contexts.lock().unwrap()[2].clone();
    current.log(entry(LogLevel::Info, "新实例日志")).unwrap();
    assert_eq!(logger.snapshot().unwrap().records.len(), 5);
    kernel.stop_all().await.unwrap();
    kernel.flush_logs().unwrap();
    drop(kernel);
    assert!(current.log(entry(LogLevel::Info, "宿主已释放")).is_err());
    assert_eq!(logger.snapshot().unwrap().records.len(), 5);
}

#[test]
fn memory_logger_filters_and_evicts_oldest_without_counting_filtered_records() {
    assert!(MemoryLogger::new(LogLevel::Info, 0).is_err());
    let logger = MemoryLogger::new(LogLevel::Info, 2).unwrap();
    for (level, message) in [
        (LogLevel::Trace, "trace"),
        (LogLevel::Debug, "debug"),
        (LogLevel::Info, "info"),
        (LogLevel::Warn, "warn"),
        (LogLevel::Error, "error"),
    ] {
        logger.log(record(level, message)).unwrap();
    }
    let snapshot = logger.snapshot().unwrap();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|r| r.entry.message.as_str())
            .collect::<Vec<_>>(),
        ["warn", "error"]
    );
    assert_eq!(snapshot.dropped, 1);
    logger.flush().unwrap();
    assert_eq!(logger.snapshot().unwrap(), snapshot);
    let mut invalid = record(LogLevel::Debug, "被过滤也必须验证");
    invalid.entry.target = " ".into();
    assert!(matches!(logger.log(invalid), Err(PluginError::Log(_))));
    let mut invalid = record(LogLevel::Info, "非法字段名");
    invalid.entry = invalid.entry.with_field(" ", "值");
    assert!(logger.log(invalid).is_err());
    assert_eq!(logger.snapshot().unwrap(), snapshot);
}

#[derive(Clone, Default)]
struct ChunkWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    flushes: Arc<AtomicUsize>,
}

impl Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // 故意短写并让出线程，只有 Logger 锁住整条记录才能避免交错。
        let count = bytes.len().min(3);
        self.bytes
            .lock()
            .unwrap()
            .extend_from_slice(&bytes[..count]);
        std::thread::yield_now();
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn writer_escapes_untrusted_text_and_serializes_concurrent_partial_writes() {
    let writer = ChunkWriter::default();
    let logger = Arc::new(WriterLogger::new(LogLevel::Info, writer.clone()));
    logger.log(record(LogLevel::Debug, "不可见")).unwrap();
    assert!(writer.bytes.lock().unwrap().is_empty());
    let mut special = record(LogLevel::Info, "中文\n伪造行\r\u{1b}[31m\"引号\"");
    special.plugin = PluginId::new("owner\nline").unwrap();
    special.entry.target = "target\nline".into();
    special.entry = special.entry.with_field("key\nline", "value\nline");
    special.timestamp = SystemTime::UNIX_EPOCH - Duration::from_millis(10);
    logger.log(special).unwrap();
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let logger = logger.clone();
            scope.spawn(move || {
                for index in 0..20 {
                    logger
                        .log(record(LogLevel::Info, &format!("{worker}:{index}")))
                        .unwrap();
                }
            });
        }
    });
    logger.flush().unwrap();
    assert_eq!(writer.flushes.load(Ordering::SeqCst), 1);
    let output = String::from_utf8(writer.bytes.lock().unwrap().clone()).unwrap();
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 81);
    assert!(lines[0].starts_with("timestamp_ms=-10 level=INFO"));
    assert!(lines[0].contains("中文\\n伪造行\\r\\u{1b}[31m\\\"引号\\\""));
    assert!(lines[0].contains("plugin=\"owner\\nline\""));
    assert!(lines[0].contains("target=\"target\\nline\""));
    assert!(lines[0].contains("\"key\\nline\": \"value\\nline\""));
    assert!(!output.contains('\r') && !output.contains('\u{1b}'));
    for worker in 0..4 {
        for index in 0..20 {
            let expected = format!(
                "timestamp_ms=1234 level=INFO plugin=\"test.owner\" target=\"test.logging\" message=\"{worker}:{index}\" fields={{}}"
            );
            assert_eq!(lines.iter().filter(|line| **line == expected).count(), 1);
        }
    }
}

struct BrokenWriter;
impl Write for BrokenWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("模拟写入故障"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("模拟刷新故障"))
    }
}

#[tokio::test]
async fn writer_errors_reach_plugin_and_trigger_cleanup_and_flush_errors_reach_host() {
    let logger = Arc::new(WriterLogger::new(LogLevel::Info, BrokenWriter));
    let kernel = Kernel::with_services(KernelServices {
        logger,
        ..Default::default()
    });
    let cleaned = Arc::new(AtomicUsize::new(0));
    let observed = cleaned.clone();
    register(&kernel, "failing", move |ctx| {
        let observed = observed.clone();
        ctx.cleanup(cleanup(move || async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))?;
        ctx.log(entry(LogLevel::Info, "失败"))?;
        Ok(None)
    });
    let id = PluginId::new("failing").unwrap();
    let error = kernel.start(&id).await.unwrap_err();
    assert!(error.to_string().contains("模拟写入故障"));
    assert_eq!(kernel.state(&id), Some(PluginState::Failed));
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    let error = kernel.flush_logs().unwrap_err();
    assert!(matches!(error, PluginError::Log(_)));
    assert!(error.to_string().contains("模拟刷新故障"));
}

#[derive(Default)]
struct ContextLogger {
    context: Mutex<Option<PluginContext>>,
    calls: AtomicUsize,
}

impl Logger for ContextLogger {
    fn log(&self, record: LogRecord) -> PluginResult<()> {
        record.entry.validate()?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let context = self.context.lock().unwrap().clone().unwrap();
        context.state_set("logged", record.entry.message.into_bytes())
    }
    fn flush(&self) -> PluginResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn replacement_logger_can_use_context_without_scope_lock_reentrancy() {
    let logger = Arc::new(ContextLogger::default());
    let backend = logger.clone();
    let kernel = Kernel::with_services(KernelServices {
        logger: logger.clone(),
        ..Default::default()
    });
    register(&kernel, "reentrant", move |ctx| {
        *backend.context.lock().unwrap() = Some(ctx.clone());
        ctx.log(entry(LogLevel::Info, "可重入写入"))?;
        assert_eq!(
            ctx.state_get("logged")?,
            Some("可重入写入".as_bytes().to_vec())
        );
        // 公开字段可修改，Context 必须在调用替代后端之前校验。
        let mut invalid = entry(LogLevel::Info, "不应投递");
        invalid.target.clear();
        assert!(ctx.log(invalid).is_err());
        Ok(None)
    });
    kernel.start_all().await.unwrap();
    assert_eq!(logger.calls.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stopping_tasks_can_log_before_cleanup_and_closed_context_cannot() {
    let logger = Arc::new(MemoryLogger::default());
    let kernel = Kernel::with_services(KernelServices {
        logger: logger.clone(),
        ..Default::default()
    });
    let ready = Arc::new(tokio::sync::Notify::new());
    let started = ready.clone();
    register(&kernel, "task-owner", move |ctx| {
        let task_ctx = ctx.clone();
        let started = started.clone();
        ctx.spawn_task(TaskSpec::new(
            "日志收尾",
            TaskMode::Background,
            TaskSchedule::Immediate,
            Arc::new(move |signal| {
                let task_ctx = task_ctx.clone();
                let started = started.clone();
                Box::pin(async move {
                    started.notify_one();
                    signal.cancelled().await;
                    task_ctx.log(entry(LogLevel::Info, "任务已收尾"))
                })
            }),
        )?)?;
        let closed = ctx.clone();
        ctx.cleanup(cleanup(move || async move {
            assert!(
                closed
                    .log(entry(LogLevel::Info, "旧 Context 已关闭"))
                    .is_err()
            );
            Ok(())
        }))?;
        Ok(None)
    });
    kernel.start_all().await.unwrap();
    ready.notified().await;
    kernel.stop_all().await.unwrap();
    let records = logger.snapshot().unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].entry.message, "任务已收尾");
    assert_eq!(records[0].plugin.as_str(), "task-owner");
}

#[test]
fn invalid_entries_are_rejected_before_writer_output_even_when_filtered() {
    assert!(LogEntry::new(LogLevel::Info, " ", "empty").is_err());
    let writer = ChunkWriter::default();
    let logger = WriterLogger::new(LogLevel::Error, writer.clone());
    let mut invalid = record(LogLevel::Debug, "invalid");
    invalid.entry.target.clear();
    assert!(logger.log(invalid).is_err());
    let mut invalid = record(LogLevel::Info, "invalid");
    invalid.entry = invalid.entry.with_field(" ", "value");
    assert!(logger.log(invalid).is_err());
    assert!(writer.bytes.lock().unwrap().is_empty());
}
