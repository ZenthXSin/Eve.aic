//! 受限 Node 测试：完整私有运行时及输入快照，无宿主实时库、网络或工具 RPC。
//! 仅报告这些冻结字节上的真实执行结果，不证明活工作区、确定性或整体目标完成。
//! 时间、随机数及宿主内核仍是外部环境；启动失败和期限结束不得作为通过证据。
//! 启动器摘要是可信启动设施元数据，执行前只读复核不保证并发更新无竞态。
//! 固定 Node 权限策略拒绝原生插件、另起进程、worker 与 WASI，不取代 OS 隔离。
use crate::{Result, archive_digest, digest, fail, reject};
use eve_llm_api::{ContextScope, ToolExecutionContext, ToolExecutionError};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};

const MAX_RUNTIME_FILES: usize = 512;
const MAX_RUNTIME_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LAUNCHER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INPUT_FILES: usize = 64;
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_INVOCATIONS: usize = 1024;
const MOUNT_DIRECTORIES: &[&str] = &["work", "control", "tmp", "dev"];
const POLICY_VERSION: &str = "frozen-node:v1";
const START_MARKER: &[u8] = b"eve-frozen-node-started:v1\n";
const BOOTSTRAP: &str = "require('node:fs').writeSync(1,'eve-frozen-node-started:v1\\n');\n";
const FLAGS: &[&str] = &[
    "--permission",
    "--allow-fs-read=*",
    "--allow-fs-write=/tmp",
    "--no-addons",
    "--jitless",
    "--max-old-space-size=64",
    "--max-semi-space-size=4",
    "--require",
    "/control/start.cjs",
];
const ENVIRONMENT: &[(&str, &str)] = &[
    ("HOME", "/tmp"),
    ("TMPDIR", "/tmp"),
    ("PATH", "/runtime"),
    ("LANG", "C"),
    ("TZ", "UTC"),
];

/// 只能由可信宿主准备；不接受模型直接指定运行时目录。
/// 首版只支持普通目录/文件，拒绝符号链接、设备、套接字等特殊对象。
pub struct FrozenNodeRuntime {
    snapshot: tempfile::TempDir,
    node_relative: String,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ManifestEntry {
    path: String,
    directory: bool,
    executable_bits: u32,
    bytes: u64,
    sha256: Option<String>,
}

impl FrozenNodeRuntime {
    /// 生成宿主发布的结构摘要：包含全部路径、目录、字节摘要、长度与各执行位。
    /// 调用者负责可信来源；这不是从未知目录自行建立信任的接口。
    pub fn bundle_sha256(
        rootfs: &Path,
        node_relative: &str,
        context: &ToolExecutionContext,
    ) -> Result<String> {
        relative_path(node_relative)?;
        let entries = inspect_runtime(rootfs, context, None, false)?;
        runtime_digest(node_relative, &entries)
    }

    /// 完整复制并与宿主预期结构摘要比对，不挂载源目录或实时宿主库。
    /// 缺失动态库将在真实启动时失败；不解析通用 ELF 依赖或自动下载。
    pub fn prepare(
        rootfs: &Path,
        node_relative: &str,
        expected_rootfs_sha256: &str,
        context: &ToolExecutionContext,
    ) -> Result<Self> {
        platform()?;
        relative_path(node_relative)?;
        if expected_rootfs_sha256.len() != 64
            || !expected_rootfs_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(reject("运行时结构摘要无效"));
        }
        let snapshot = tempfile::tempdir().map_err(fail)?;
        let entries = inspect_runtime(rootfs, context, Some(snapshot.path()), false)?;
        let actual = runtime_digest(node_relative, &entries)?;
        if !actual.eq_ignore_ascii_case(expected_rootfs_sha256) {
            return Err(reject("运行时完整快照与宿主预期摘要不同"));
        }
        if !entries.iter().any(|entry| {
            entry.path == node_relative && !entry.directory && entry.executable_bits & 0o100 != 0
        }) {
            return Err(reject("冻结运行时缺少指定可执行 Node 文件"));
        }
        // 只把复制完成后的私有快照作为后续唯一读取源。
        let copied = inspect_runtime(snapshot.path(), context, None, false)?;
        if runtime_digest(node_relative, &copied)? != actual {
            return Err(reject("运行时私有复制校验失败"));
        }
        for name in MOUNT_DIRECTORIES {
            fs::create_dir(snapshot.path().join(name)).map_err(fail)?;
        }
        Ok(Self {
            snapshot,
            node_relative: node_relative.into(),
            sha256: actual,
        })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// 完整小型纯 JS bundle。字节由调用方明确提交，不读取模型指定宿主路径。
/// 禁止 node native addon；完整依赖必须随 bundle 提供，缺失即失败。
pub struct FrozenNodeTestBundle {
    files: BTreeMap<String, Vec<u8>>,
    entry: String,
    arguments: Vec<String>,
    sha256: String,
}
impl FrozenNodeTestBundle {
    pub fn new(
        files: BTreeMap<String, Vec<u8>>,
        entry: String,
        arguments: Vec<String>,
    ) -> Result<Self> {
        if files.is_empty() || files.len() > MAX_INPUT_FILES || arguments.len() > 16 {
            return Err(reject("Node 测试输入数量超限"));
        }
        relative_path(&entry)?;
        if !matches!(
            Path::new(&entry).extension().and_then(|s| s.to_str()),
            Some("js" | "cjs" | "mjs")
        ) || !files.contains_key(&entry)
        {
            return Err(reject("Node 测试入口须为 bundle 内的 JS 文件"));
        }
        let mut bytes = 0usize;
        let mut manifest = Vec::new();
        for (path, content) in &files {
            relative_path(path)?;
            if Path::new(path).extension().is_some_and(|ext| ext == "node") {
                return Err(reject("冻结纯 JS 测试不支持 native addon"));
            }
            bytes = bytes.saturating_add(content.len());
            if bytes > MAX_INPUT_BYTES {
                return Err(reject("Node 完整测试 bundle 超过 8 MiB"));
            }
            for parent in Path::new(path).ancestors().skip(1) {
                if files.contains_key(parent.to_str().unwrap_or_default()) {
                    return Err(reject("bundle 文件与父目录冲突"));
                }
            }
            manifest.push((path, content.len(), archive_digest(content)));
        }
        if arguments.iter().map(String::len).sum::<usize>() > 4096
            || arguments.iter().any(|value| value.contains('\0'))
        {
            return Err(reject("测试脚本参数超限或含 NUL"));
        }
        let sha256 = digest(&(POLICY_VERSION, &manifest))?;
        Ok(Self {
            files,
            entry,
            arguments,
            sha256,
        })
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// 进程观察；没有 current_valid 或任务完成状态。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FrozenNodeOutcome {
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Cancelled,
    Unknown,
}

/// 仅真实运行器能构造；没有 Deserialize，不接受模型 JSON 自填来源。
/// 持久保存、跨重启只读引用与任务完成由可信宿主负责。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FrozenNodeTestReceipt {
    owner: ContextScope,
    execution_id: String,
    call_id: String,
    input_sha256: String,
    runtime_sha256: String,
    command_sha256: String,
    outcome: FrozenNodeOutcome,
    started: bool,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}
impl FrozenNodeTestReceipt {
    pub fn owner(&self) -> &ContextScope {
        &self.owner
    }
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
    pub fn input_sha256(&self) -> &str {
        &self.input_sha256
    }
    pub fn runtime_sha256(&self) -> &str {
        &self.runtime_sha256
    }
    pub fn command_sha256(&self) -> &str {
        &self.command_sha256
    }
    pub fn outcome(&self) -> &FrozenNodeOutcome {
        &self.outcome
    }
    /// 只表示观察到了可信 bootstrap 标记；false 不证明进程从未启动。
    pub fn started(&self) -> bool {
        self.started
    }
    pub fn stdout(&self) -> &str {
        &self.stdout
    }
    pub fn stderr(&self) -> &str {
        &self.stderr
    }
    pub fn stdout_truncated(&self) -> bool {
        self.stdout_truncated
    }
    pub fn stderr_truncated(&self) -> bool {
        self.stderr_truncated
    }
}

/// 运行器不提供内存历史账本；used 只阻止同实例内同执行/调用的重放。
/// 重启不会恢复执行或用它承诺去重复，持久收尾必须由宿主另行绑定真实 receipt。
pub struct FrozenNodeTestRunner {
    runtime: FrozenNodeRuntime,
    bwrap: PathBuf,
    bwrap_sha256: String,
    used: Mutex<BTreeSet<String>>,
}
impl FrozenNodeTestRunner {
    pub fn new(runtime: FrozenNodeRuntime, bwrap: PathBuf) -> Result<Self> {
        platform()?;
        if !bwrap.is_absolute() || !bwrap.is_file() {
            return Err(reject("冻结测试缺少可信 bubblewrap"));
        }
        let bwrap = fs::canonicalize(bwrap).map_err(fail)?;
        let bwrap_sha256 = launcher_digest(&bwrap, None)?;
        Ok(Self {
            runtime,
            bwrap,
            bwrap_sha256,
            used: Mutex::new(BTreeSet::new()),
        })
    }

    pub fn command_sha256(&self, bundle: &FrozenNodeTestBundle) -> Result<String> {
        digest(&(
            POLICY_VERSION,
            &self.bwrap_sha256,
            &self.runtime.node_relative,
            FLAGS,
            ENVIRONMENT,
            archive_digest(BOOTSTRAP.as_bytes()),
            &bundle.entry,
            &bundle.arguments,
        ))
    }

    pub fn runtime_sha256(&self) -> &str {
        self.runtime.sha256()
    }

    /// 用作已由 LlmHost 收费的叶子；不重复 reserve 工具预算。
    /// 调用者须已预留本次工具调用，仍要求同一共享预算上下文。
    pub async fn run_precharged(
        &self,
        bundle: &FrozenNodeTestBundle,
        execution_id: &str,
        call_id: &str,
        context: ToolExecutionContext,
    ) -> Result<FrozenNodeTestReceipt> {
        self.run(bundle, execution_id, call_id, context, false)
            .await
    }

    /// 没有外层工具收费的可信宿主执行；单独预留一次原共享预算。
    pub async fn run_independent(
        &self,
        bundle: &FrozenNodeTestBundle,
        execution_id: &str,
        call_id: &str,
        context: ToolExecutionContext,
    ) -> Result<FrozenNodeTestReceipt> {
        self.run(bundle, execution_id, call_id, context, true).await
    }

    async fn run(
        &self,
        bundle: &FrozenNodeTestBundle,
        execution_id: &str,
        call_id: &str,
        context: ToolExecutionContext,
        reserve: bool,
    ) -> Result<FrozenNodeTestReceipt> {
        let owner = context
            .scope()
            .filter(|scope| !scope.user_id.is_empty() && !scope.session_id.is_empty())
            .ok_or_else(|| reject("冻结测试缺少可信会话身份"))?
            .clone();
        if execution_id.is_empty()
            || execution_id.len() > 256
            || execution_id.chars().any(char::is_control)
            || call_id.is_empty()
            || call_id.len() > 256
            || call_id.chars().any(char::is_control)
            || context.call_id() != Some(call_id)
            || context.shared_steps().is_none()
        {
            return Err(reject("冻结测试执行编号、调用编号或共享预算绑定无效"));
        }
        let deadline = context
            .deadline()
            .map(|at| at.min(Instant::now() + Duration::from_secs(30)))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        let context = context.with_deadline(Some(deadline));
        crate::runtime::check_stop(&context, deadline)?;
        if launcher_digest(&self.bwrap, Some(&context))? != self.bwrap_sha256 {
            return Err(reject("可信 bubblewrap 启动器身份已变化，拒绝启动"));
        }
        let key = digest(&(&owner, execution_id, call_id))?;
        {
            let mut used = self.used.lock().map_err(|_| fail("冻结测试调用锁不可用"))?;
            if used.len() >= MAX_INVOCATIONS || !used.insert(key) {
                return Err(reject("同实例冻结测试调用已使用或容量已满；不重放"));
            }
        }
        if reserve {
            crate::runtime::reserve(&context)?;
        }
        let command_sha256 = self.command_sha256(bundle)?;
        let stage = tempfile::tempdir().map_err(fail)?;
        let work = stage.path().join("work");
        let control = stage.path().join("control");
        fs::create_dir(&work).map_err(fail)?;
        fs::create_dir(&control).map_err(fail)?;
        for (path, bytes) in &bundle.files {
            crate::runtime::check_stop(&context, deadline)?;
            let destination = work.join(path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(fail)?;
            }
            fs::write(&destination, bytes).map_err(fail)?;
            set_mode(&destination, 0)?;
        }
        let bootstrap = control.join("start.cjs");
        fs::write(&bootstrap, BOOTSTRAP).map_err(fail)?;
        set_mode(&bootstrap, 0)?;
        // 再校验私有运行时；句柄不公开源目录或私有路径。
        if runtime_digest(
            &self.runtime.node_relative,
            &inspect_runtime(self.runtime.snapshot.path(), &context, None, true)?,
        )? != self.runtime.sha256
        {
            return Err(reject("私有运行时已变化，拒绝启动"));
        }
        crate::runtime::check_stop(&context, deadline)?;
        let mut command = Command::new(&self.bwrap);
        let _filter = crate::sandbox::install_process_limits(&mut command)?;
        command
            .env_clear()
            .args([
                "--unshare-all",
                "--unshare-user",
                "--disable-userns",
                "--assert-userns-disabled",
                "--die-with-parent",
                "--new-session",
                "--cap-drop",
                "ALL",
                "--clearenv",
                "--ro-bind",
            ])
            .arg(self.runtime.snapshot.path())
            .arg("/");
        command
            .arg("--ro-bind")
            .arg(&work)
            .arg("/work")
            .arg("--ro-bind")
            .arg(&control)
            .arg("/control")
            .args([
                "--size", "16777216", "--tmpfs", "/tmp", "--dev", "/dev", "--chdir", "/work",
            ]);
        for (name, value) in ENVIRONMENT {
            command.args(["--setenv", name, value]);
        }
        command
            .arg("--")
            .arg(format!("/{}", self.runtime.node_relative))
            .args(FLAGS)
            .arg(format!("/work/{}", bundle.entry))
            .args(&bundle.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut receipt = FrozenNodeTestReceipt {
            owner,
            execution_id: execution_id.into(),
            call_id: call_id.into(),
            input_sha256: bundle.sha256.clone(),
            runtime_sha256: self.runtime.sha256.clone(),
            command_sha256,
            outcome: FrozenNodeOutcome::Unknown,
            started: false,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        crate::runtime::check_stop(&context, deadline)?;
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let (diagnostics, cut) = bounded_text(format!("隔离启动失败：{error}").as_bytes());
                receipt.stderr = diagnostics;
                receipt.stderr_truncated = cut;
                return Ok(receipt);
            }
        };
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| fail("冻结测试缺少 stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| fail("冻结测试缺少 stderr"))?;
        let result = crate::runtime::guarded(
            async {
                let (status, stdout, stderr) = tokio::join!(
                    child.wait(),
                    capture(stdout, MAX_OUTPUT_BYTES + START_MARKER.len()),
                    capture(stderr, MAX_OUTPUT_BYTES),
                );
                Ok((status.map_err(fail)?, stdout?, stderr?))
            },
            &context,
            deadline,
        )
        .await;
        match result {
            Ok((status, (stdout, out_cut), (stderr, err_cut))) => {
                receipt.started = stdout.starts_with(START_MARKER);
                let stdout = stdout.strip_prefix(START_MARKER).unwrap_or(&stdout);
                let (out_text, out_encoding_cut) = bounded_text(stdout);
                let (err_text, err_encoding_cut) = bounded_text(&stderr);
                receipt.stdout = out_text;
                receipt.stderr = err_text;
                receipt.stdout_truncated = out_cut || out_encoding_cut;
                receipt.stderr_truncated = err_cut || err_encoding_cut;
                receipt.outcome = exit_outcome(&status, receipt.started, &context, deadline);
            }
            Err(error) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                if matches!(error, ToolExecutionError::Cancelled(_)) {
                    receipt.outcome = FrozenNodeOutcome::Cancelled;
                }
            }
        }
        Ok(receipt)
    }
}

fn exit_outcome(
    status: &std::process::ExitStatus,
    started: bool,
    context: &ToolExecutionContext,
    deadline: Instant,
) -> FrozenNodeOutcome {
    // guarded 可与取消/期限同时就绪；收尾时再次确认，不能记为通过。
    if crate::runtime::check_stop(context, deadline).is_err() {
        return FrozenNodeOutcome::Cancelled;
    }
    if !started {
        return FrozenNodeOutcome::Unknown;
    }
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    FrozenNodeOutcome::Exited {
        exit_code: status.code(),
        signal,
    }
}

async fn capture(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<(Vec<u8>, bool)> {
    let mut saved = Vec::new();
    let mut cut = false;
    let mut buffer = [0u8; 4096];
    loop {
        let n = stream.read(&mut buffer).await.map_err(fail)?;
        if n == 0 {
            break;
        }
        let keep = n.min(limit.saturating_sub(saved.len()));
        saved.extend_from_slice(&buffer[..keep]);
        cut |= keep < n;
    }
    Ok((saved, cut))
}

fn bounded_text(bytes: &[u8]) -> (String, bool) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let truncated = text.len() > MAX_OUTPUT_BYTES;
    if truncated {
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    (text, truncated)
}

fn platform() -> Result<()> {
    if !cfg!(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )) {
        return Err(reject("冻结 Node 测试只支持已核对的 Linux 架构"));
    }
    Ok(())
}

fn relative_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(char::is_control)
        || value.contains('\\')
        || value.starts_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || Path::new(value)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(reject("冻结 bundle 路径须为安全相对路径"));
    }
    Ok(())
}

fn runtime_digest(node: &str, entries: &[ManifestEntry]) -> Result<String> {
    digest(&("frozen-node-rootfs:v1", node, entries))
}

fn launcher_digest(path: &Path, context: Option<&ToolExecutionContext>) -> Result<String> {
    let before = fs::symlink_metadata(path).map_err(fail)?;
    let executable_bits = executable_bits(&before);
    if !before.is_file() || executable_bits & 0o100 == 0 || before.len() > MAX_LAUNCHER_BYTES {
        return Err(reject("可信 bubblewrap 须为有界普通可执行文件"));
    }
    let deadline = context.and_then(ToolExecutionContext::deadline);
    let mut file = File::open(path).map_err(fail)?;
    if !same_file(&before, &file.metadata().map_err(fail)?) {
        return Err(reject("启动器在打开前后变化"));
    }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut seen = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if let (Some(context), Some(deadline)) = (context, deadline) {
            crate::runtime::check_stop(context, deadline)?;
        }
        let n = file.read(&mut buffer).map_err(fail)?;
        if n == 0 {
            break;
        }
        seen = seen.saturating_add(n as u64);
        if seen > before.len() {
            return Err(reject("启动器读取时增长"));
        }
        hash.update(&buffer[..n]);
    }
    if seen != before.len()
        || !same_file(&before, &file.metadata().map_err(fail)?)
        || !same_file(&before, &fs::symlink_metadata(path).map_err(fail)?)
    {
        return Err(reject("启动器读取时变化"));
    }
    let sha256: String = hash
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    digest(&("frozen-node-launcher:v1", executable_bits, seen, sha256))
}

fn inspect_runtime(
    root: &Path,
    context: &ToolExecutionContext,
    copy_to: Option<&Path>,
    allow_mount_directories: bool,
) -> Result<Vec<ManifestEntry>> {
    let deadline = context
        .deadline()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
    if !root.is_absolute()
        || fs::canonicalize(root).map_err(fail)? != root
        || !fs::symlink_metadata(root).map_err(fail)?.is_dir()
    {
        return Err(reject("运行时 rootfs 必须为规范真实目录"));
    }
    let mut queue = vec![(root.to_path_buf(), String::new())];
    let mut entries = Vec::new();
    let mut bytes = 0u64;
    while let Some((directory, prefix)) = queue.pop() {
        crate::runtime::check_stop(context, deadline)?;
        let mut children = Vec::new();
        for child in fs::read_dir(directory).map_err(fail)? {
            crate::runtime::check_stop(context, deadline)?;
            if children.len() >= MAX_RUNTIME_FILES + MOUNT_DIRECTORIES.len() {
                return Err(reject("运行时单目录条目数量超限"));
            }
            children.push(child.map_err(fail)?);
        }
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| reject("运行时路径不是 UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            relative_path(&relative)?;
            let before = fs::symlink_metadata(child.path()).map_err(fail)?;
            if prefix.is_empty() && MOUNT_DIRECTORIES.contains(&relative.as_str()) {
                if !allow_mount_directories
                    || !before.is_dir()
                    || fs::read_dir(child.path()).map_err(fail)?.next().is_some()
                {
                    return Err(reject("运行时 rootfs 包含保留挂载目录"));
                }
                continue;
            }
            if prefix.is_empty() && relative == "proc" {
                return Err(reject("冻结运行时不提供 proc"));
            }
            if before.file_type().is_symlink() || !(before.is_dir() || before.is_file()) {
                return Err(reject("运行时快照只接受普通文件和目录，不接受链接"));
            }
            if entries.len() >= MAX_RUNTIME_FILES {
                return Err(reject("运行时文件和目录数量超限"));
            }
            let executable_bits = executable_bits(&before);
            if before.is_dir() {
                if executable_bits & 0o100 == 0 {
                    return Err(reject("运行时目录缺少所有者遍历执行位"));
                }
                if let Some(target) = copy_to {
                    fs::create_dir(target.join(&relative)).map_err(fail)?;
                    set_mode(&target.join(&relative), executable_bits)?;
                }
                entries.push(ManifestEntry {
                    path: relative.clone(),
                    directory: true,
                    executable_bits,
                    bytes: 0,
                    sha256: None,
                });
                queue.push((child.path(), relative));
                continue;
            }
            bytes = bytes.saturating_add(before.len());
            if bytes > MAX_RUNTIME_BYTES {
                return Err(reject("运行时完整快照超过 256 MiB"));
            }
            let mut file = File::open(child.path()).map_err(fail)?;
            if !same_file(&before, &file.metadata().map_err(fail)?) {
                return Err(reject("运行时对象在打开前后变化"));
            }
            let mut target = copy_to
                .map(|root| File::create(root.join(&relative)))
                .transpose()
                .map_err(fail)?;
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            let mut seen = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                crate::runtime::check_stop(context, deadline)?;
                let n = file.read(&mut buffer).map_err(fail)?;
                if n == 0 {
                    break;
                }
                seen = seen.saturating_add(n as u64);
                if seen > before.len() {
                    return Err(reject("运行时文件读取时增长"));
                }
                hash.update(&buffer[..n]);
                if let Some(target) = &mut target {
                    target.write_all(&buffer[..n]).map_err(fail)?;
                }
            }
            if seen != before.len()
                || !same_file(&before, &file.metadata().map_err(fail)?)
                || !same_file(&before, &fs::symlink_metadata(child.path()).map_err(fail)?)
            {
                return Err(reject("运行时文件读取时变化"));
            }
            if let Some(root) = copy_to {
                set_mode(&root.join(&relative), executable_bits)?;
            }
            entries.push(ManifestEntry {
                path: relative,
                directory: false,
                executable_bits,
                bytes: seen,
                sha256: Some(
                    hash.finish()
                        .as_ref()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                ),
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn executable_bits(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.is_file()
            && b.is_file()
            && a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        a.is_file() && b.is_file() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
    }
}

fn set_mode(path: &Path, executable_bits: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644 | executable_bits))
            .map_err(fail)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable_bits);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[tokio::test]
    async fn ready_exit_cannot_override_cancellation_at_settlement() {
        let context = ToolExecutionContext::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        context.cancellation_handle().cancel();
        // 立即就绪的退出 future 会先于 guarded 的 10ms 轮询完成。
        let status = crate::runtime::guarded(
            async { Ok(std::process::ExitStatus::from_raw(0)) },
            &context,
            deadline,
        )
        .await
        .expect("ready future is returned before polling cancellation");
        assert_eq!(
            exit_outcome(&status, true, &context, deadline),
            FrozenNodeOutcome::Cancelled
        );
    }

    #[test]
    fn expired_success_and_missing_bootstrap_are_not_passing_exits() {
        let context = ToolExecutionContext::default();
        let status = std::process::ExitStatus::from_raw(0);
        assert_eq!(
            exit_outcome(&status, true, &context, Instant::now()),
            FrozenNodeOutcome::Cancelled
        );
        assert_eq!(
            exit_outcome(
                &status,
                false,
                &context,
                Instant::now() + Duration::from_secs(30)
            ),
            FrozenNodeOutcome::Unknown
        );
    }
}
