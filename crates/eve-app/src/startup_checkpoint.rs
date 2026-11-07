//! 发布用的一次性入站前检查点。只传实际持久字节的摘要，不保存或重建业务状态。
//!
//! 组合宿主可替换 sink；文件握手仅在显式设置 `EVE_STARTUP_CHECKPOINT_REQUEST` 时启用。
//! 发布端必须在屏障内读取完整状态（包括冷库），严格证明恢复保留后才确认继续。
use eve_plugin_api::{PluginError, PluginFuture, PluginId, PluginResult, StateStore};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// 文件握手只由启动器显式装配，不是模型或聊天参数。
pub const REQUEST_ENV: &str = "EVE_STARTUP_CHECKPOINT_REQUEST";
const MAX_CONTROL_BYTES: u64 = 64 * 1024;
const MAX_LEDGER_BYTES: usize = 16 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn failure(reason: &str) -> PluginError {
    PluginError::State(format!("入站前恢复检查点失败：{reason}；未开放通道"))
}

fn sha(bytes: &[u8]) -> String {
    ring::digest::digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// None 与空账本分开：缺失不能由默认序列化后的空对象代替。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerCheckpoint {
    pub present: bool,
    pub version: Option<u64>,
    pub bytes: u64,
    pub sha256: Option<String>,
}

impl LedgerCheckpoint {
    fn read(
        state: &dyn StateStore,
        owner: &str,
        key: &str,
        version_field: &str,
    ) -> PluginResult<Self> {
        let Some(bytes) = state.get(&PluginId::new(owner)?, key)? else {
            return Ok(Self {
                present: false,
                version: None,
                bytes: 0,
                sha256: None,
            });
        };
        if bytes.len() > MAX_LEDGER_BYTES {
            return Err(failure("持久账本超出摘要读取上限"));
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| failure("持久账本不是有效 JSON"))?;
        let version = value
            .get(version_field)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| failure("持久账本缺少版本"))?;
        Ok(Self {
            present: true,
            version: Some(version),
            bytes: bytes.len() as u64,
            sha256: Some(sha(&bytes)),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupLedgers {
    pub tasks: LedgerCheckpoint,
    pub phrases: LedgerCheckpoint,
    pub receipts: LedgerCheckpoint,
}

impl StartupLedgers {
    fn read(state: &dyn StateStore) -> PluginResult<Self> {
        Ok(Self {
            tasks: LedgerCheckpoint::read(
                state,
                eve_task_api::TASK_PLUGIN_ID,
                eve_task_api::TASK_STATE_KEY,
                "format_version",
            )?,
            phrases: LedgerCheckpoint::read(
                state,
                eve_training_api::TRAINING_PLUGIN_ID,
                "phrases.v1",
                "version",
            )?,
            receipts: LedgerCheckpoint::read(
                state,
                eve_qqbot_plugin::QQBOT_PLUGIN_ID,
                "receipts.v1",
                "version",
            )?,
        })
    }
}

/// 同一 StateStore 完成恢复后的热账本摘要；冷库由 sink 在回调期间读取。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupState {
    pub state_directory: PathBuf,
    pub automatic_capabilities: bool,
    pub ledgers: StartupLedgers,
}

/// 返回成功才允许 QQ 桥、Web 监听与后台激活。可信宿主可以替换文件实现。
pub trait StartupCheckpointSink: std::fmt::Debug + Send + Sync {
    fn checkpoint(&self, state: StartupState) -> PluginFuture<'_, ()>;
}

pub(crate) struct Boundary {
    state: Arc<dyn StateStore>,
    directory: PathBuf,
    automatic_capabilities: bool,
    sink: Arc<dyn StartupCheckpointSink>,
}

impl Boundary {
    pub(crate) fn new(
        state: Arc<dyn StateStore>,
        directory: &Path,
        automatic_capabilities: bool,
        sink: Arc<dyn StartupCheckpointSink>,
    ) -> PluginResult<Self> {
        Ok(Self {
            state,
            directory: fs::canonicalize(directory).map_err(|_| failure("状态目录无法规范化"))?,
            automatic_capabilities,
            sink,
        })
    }
}

impl eve_qqbot_plugin::QqBeforeIngress for Boundary {
    fn before_ingress(&self) -> PluginFuture<'_, ()> {
        Box::pin(async move {
            let ledgers = StartupLedgers::read(self.state.as_ref())?;
            self.sink
                .checkpoint(StartupState {
                    state_directory: self.directory.clone(),
                    automatic_capabilities: self.automatic_capabilities,
                    ledgers: ledgers.clone(),
                })
                .await?;
            if StartupLedgers::read(self.state.as_ref())? != ledgers {
                return Err(failure("等待恢复证明期间热账本变化"));
            }
            Ok(())
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format_version: u8,
    nonce: String,
    source_revision: String,
    binary_sha256: String,
    runtime_path: PathBuf,
    runtime_sha256: String,
    state_directory: PathBuf,
    checkpoint_path: PathBuf,
    acknowledgement_path: PathBuf,
    timeout_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint<'a> {
    format_version: u8,
    request_sha256: &'a str,
    nonce: &'a str,
    source_revision: &'a str,
    binary_sha256: &'a str,
    runtime_path: &'a Path,
    runtime_sha256: &'a str,
    state_directory: &'a Path,
    checkpoint_path: &'a Path,
    acknowledgement_path: &'a Path,
    pid: u32,
    process_start_ticks: u64,
    exe_path: &'a Path,
    automatic_capabilities: bool,
    ledgers: &'a StartupLedgers,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledgement {
    format_version: u8,
    nonce: String,
    pid: u32,
    process_start_ticks: u64,
    checkpoint_sha256: String,
    preservation_sha256: String,
    accepted: bool,
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len() && left.modified().ok() == right.modified().ok()
    }
}

fn open_regular(path: &Path, limit: u64) -> PluginResult<(File, Metadata)> {
    let before = fs::symlink_metadata(path).map_err(|_| failure("绑定文件不可读"))?;
    if !before.file_type().is_file() || before.len() > limit {
        return Err(failure("绑定文件非普通文件或超出上限"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| failure("绑定文件无法安全打开"))?;
    let current = file.metadata().map_err(|_| failure("绑定文件属性不可读"))?;
    if !current.file_type().is_file() || !same_file(&before, &current) {
        return Err(failure("绑定文件打开期间变化"));
    }
    Ok((file, before))
}

fn read_regular(path: &Path, limit: u64) -> PluginResult<Vec<u8>> {
    let (mut file, before) = open_regular(path, limit)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("绑定文件读取失败"))?;
    if bytes.len() as u64 != before.len()
        || !same_file(
            &before,
            &file.metadata().map_err(|_| failure("绑定文件属性不可读"))?,
        )
    {
        return Err(failure("绑定文件读取期间变化"));
    }
    Ok(bytes)
}

fn file_sha(path: &Path, limit: u64) -> PluginResult<String> {
    let (mut file, before) = open_regular(path, limit)?;
    let mut context = Context::new(&SHA256);
    let mut block = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut block)
            .map_err(|_| failure("绑定文件摘要读取失败"))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > before.len() {
            return Err(failure("绑定文件摘要读取期间增长"));
        }
        context.update(&block[..read]);
    }
    if total != before.len()
        || !same_file(
            &before,
            &file.metadata().map_err(|_| failure("绑定文件属性不可读"))?,
        )
    {
        return Err(failure("绑定文件摘要读取期间变化"));
    }
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn canonical(path: &Path) -> PluginResult<()> {
    if !path.is_absolute() || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return Err(failure("绑定路径必须是规范绝对路径"));
    }
    Ok(())
}

fn absent(path: &Path) -> PluginResult<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(failure("一次性输出或确认文件已存在")),
    }
}

fn output_path(path: &Path, directory: &Path) -> PluginResult<()> {
    if !path.is_absolute()
        || path.parent() != Some(directory)
        || !matches!(
            path.components().next_back(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(failure("检查点与确认必须位于请求的私有目录"));
    }
    absent(path)
}

fn sync_directory(path: &Path) -> PluginResult<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| failure("检查点目录同步失败"))
}

fn publish(path: &Path, bytes: &[u8]) -> PluginResult<()> {
    let directory = path.parent().ok_or_else(|| failure("检查点目录缺失"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .map_err(|_| failure("检查点临时文件创建失败"))?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| failure("检查点保存失败"))?;
    temporary
        .persist_noclobber(path)
        .map_err(|_| failure("检查点不可覆盖已有文件"))?;
    sync_directory(directory)
}

fn process_start_ticks() -> PluginResult<u64> {
    let stat = fs::read_to_string("/proc/self/stat").map_err(|_| failure("进程启动身份不可读"))?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| failure("进程启动身份无效"))
}

/// 默认文件实现。请求及所有输出位于操作者预先创建的独占目录，失败原样留证。
#[derive(Debug)]
pub struct FileStartupCheckpoint {
    request: Request,
    request_path: PathBuf,
    request_sha256: String,
    called: AtomicBool,
}

impl FileStartupCheckpoint {
    /// 只解析显式请求；不接受未知/重复字段，不创建业务目录或状态。
    pub fn open(request_path: &Path) -> PluginResult<Self> {
        if !cfg!(target_os = "linux") {
            return Err(failure("文件握手要求可验证的 Linux 进程身份"));
        }
        canonical(request_path)?;
        let directory = request_path
            .parent()
            .ok_or_else(|| failure("请求目录缺失"))?;
        let metadata = fs::symlink_metadata(directory).map_err(|_| failure("请求目录不可读"))?;
        if !metadata.file_type().is_dir() {
            return Err(failure("请求目录不是普通目录"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(failure("请求目录必须由宿主独占"));
            }
        }
        let bytes = read_regular(request_path, MAX_CONTROL_BYTES)?;
        let request: Request =
            serde_json::from_slice(&bytes).map_err(|_| failure("请求协议无效"))?;
        if request.format_version != 1
            || !hex(&request.nonce, 64)
            || !hex(&request.source_revision, 40)
            || !hex(&request.binary_sha256, 64)
            || !hex(&request.runtime_sha256, 64)
            || !(1000..=60_000).contains(&request.timeout_ms)
            || !request.state_directory.is_absolute()
        {
            return Err(failure("请求身份、摘要或期限无效"));
        }
        canonical(&request.runtime_path)?;
        output_path(&request.checkpoint_path, directory)?;
        output_path(&request.acknowledgement_path, directory)?;
        if request.checkpoint_path == request.acknowledgement_path {
            return Err(failure("检查点与确认路径不能相同"));
        }
        let claimed = directory.join(format!("{}.claimed", request.nonce));
        if request.checkpoint_path == claimed || request.acknowledgement_path == claimed {
            return Err(failure("输出路径与一次性请求占用冲突"));
        }
        let request_sha256 = sha(&bytes);
        // create_new 让崩溃、并发或再次启动都无法重复使用同一个请求。
        publish(&claimed, request_sha256.as_bytes())?;
        Ok(Self {
            request,
            request_path: request_path.to_path_buf(),
            request_sha256,
            called: AtomicBool::new(false),
        })
    }

    async fn run(&self, state: StartupState) -> PluginResult<()> {
        if self.called.swap(true, Ordering::SeqCst) {
            return Err(failure("检查点接口不能重复调用"));
        }
        let request = &self.request;
        canonical(&request.state_directory)?;
        if state.state_directory != request.state_directory {
            return Err(failure("请求状态目录与实际后端不符"));
        }
        if file_sha(&self.request_path, MAX_CONTROL_BYTES)? != self.request_sha256 {
            return Err(failure("启动期间请求字节变化"));
        }
        let executable =
            std::env::current_exe().map_err(|_| failure("实际可执行文件路径不可读"))?;
        canonical(&executable)?;
        if file_sha(&executable, MAX_BINARY_BYTES)? != request.binary_sha256
            || file_sha(&request.runtime_path, MAX_CONTROL_BYTES * 128)? != request.runtime_sha256
        {
            return Err(failure("实际二进制或 runtime 与请求不符"));
        }
        let pid = std::process::id();
        let start_ticks = process_start_ticks()?;
        absent(&request.acknowledgement_path)?;
        let checkpoint = Checkpoint {
            format_version: 1,
            request_sha256: &self.request_sha256,
            nonce: &request.nonce,
            source_revision: &request.source_revision,
            binary_sha256: &request.binary_sha256,
            runtime_path: &request.runtime_path,
            runtime_sha256: &request.runtime_sha256,
            state_directory: &state.state_directory,
            checkpoint_path: &request.checkpoint_path,
            acknowledgement_path: &request.acknowledgement_path,
            pid,
            process_start_ticks: start_ticks,
            exe_path: &executable,
            automatic_capabilities: state.automatic_capabilities,
            ledgers: &state.ledgers,
        };
        let bytes = serde_json::to_vec(&checkpoint).map_err(|_| failure("检查点编码失败"))?;
        publish(&request.checkpoint_path, &bytes)?;
        let checkpoint_sha256 = sha(&bytes);
        let wait = async {
            loop {
                match fs::symlink_metadata(&request.acknowledgement_path) {
                    Ok(_) => {
                        let ack: Acknowledgement = serde_json::from_slice(&read_regular(
                            &request.acknowledgement_path,
                            MAX_CONTROL_BYTES,
                        )?)
                        .map_err(|_| failure("恢复证明确认无效"))?;
                        if ack.format_version != 1
                            || ack.nonce != request.nonce
                            || ack.pid != pid
                            || ack.process_start_ticks != start_ticks
                            || ack.checkpoint_sha256 != checkpoint_sha256
                            || !hex(&ack.preservation_sha256, 64)
                            || !ack.accepted
                        {
                            return Err(failure("确认未绑定本次检查点或未通过恢复证明"));
                        }
                        if file_sha(&self.request_path, MAX_CONTROL_BYTES)? != self.request_sha256
                            || file_sha(&request.checkpoint_path, MAX_CONTROL_BYTES)?
                                != checkpoint_sha256
                            || file_sha(&executable, MAX_BINARY_BYTES)? != request.binary_sha256
                            || file_sha(&request.runtime_path, MAX_CONTROL_BYTES * 128)?
                                != request.runtime_sha256
                        {
                            return Err(failure("等待确认期间绑定文件变化"));
                        }
                        return Ok(());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(failure("恢复证明确认不可读")),
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::time::timeout(Duration::from_millis(request.timeout_ms), wait)
            .await
            .map_err(|_| failure("等待恢复证明超时"))?
    }
}

impl StartupCheckpointSink for FileStartupCheckpoint {
    fn checkpoint(&self, state: StartupState) -> PluginFuture<'_, ()> {
        Box::pin(self.run(state))
    }
}

pub(crate) fn configured_sink(
    injected: Option<Arc<dyn StartupCheckpointSink>>,
    database_backend: bool,
) -> PluginResult<Option<Arc<dyn StartupCheckpointSink>>> {
    let request = std::env::var_os(REQUEST_ENV);
    if injected.is_some() && request.is_some() {
        return Err(failure("宿主接口与文件握手不能重复配置"));
    }
    if let Some(path) = request {
        if database_backend {
            return Err(failure("文件握手不支持 PostgreSQL；请替换宿主 sink"));
        }
        Ok(Some(Arc::new(FileStartupCheckpoint::open(Path::new(
            &path,
        ))?)))
    } else {
        Ok(injected)
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "startup_checkpoint_tests.rs"]
mod tests;
